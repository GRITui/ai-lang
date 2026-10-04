# Numeric model — what "lossless" actually covers

**Run it:** `cargo build --release && bash scripts/numeric-divergence-demo.sh` reproduces
the numbers below.

AINL has one integer type at the *source* level, and **every backend now
implements it the same way: arbitrary precision**. The interpreter and the
bytecode VM share `BigNum`, the AOT C runtime carries its own bignum, Python
and Ruby have native arbitrary-precision integers, and the JavaScript target
emits a native `BigInt`. Integers never overflow and never lose precision on
any of the five, so all five agree byte-for-byte for **any integer value AINL
can express** — that is what `scripts/check-transpile.sh` and
`scripts/parity5.sh` verify.

## Measured divergence

```lisp
(print (* 9223372036854775807 2))   ; i64::MAX * 2
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
```

| target | `i64::MAX * 2` | `(fact 25)` | `(fact 100)` |
|---|---|---|---|
| interpreter / VM (`ainl run`) | `18446744073709551614` | `15511210043330985984000000` | 158 digits, exact |
| AOT binary (`ainl compile`) | `18446744073709551614` | `15511210043330985984000000` | 158 digits, exact |
| JS (`ainl transpile --to js`) | `18446744073709551614` | `15511210043330985984000000` | 158 digits, exact |
| Python (`ainl transpile --to python`) | `18446744073709551614` | `15511210043330985984000000` | 158 digits, exact |
| Ruby (`ainl transpile --to ruby`) | `18446744073709551614` | `15511210043330985984000000` | 158 digits, exact |

**All five agree.** There is no divergence left in the table.

- **The interpreter** uses `BigNum` (`crates/ainl-core/src/bignum.rs`), a
  hand-written arbitrary-precision integer with an allocation-free `i64` fast
  path. Integers never overflow: an operation that leaves `i64` range widens to
  a signed-magnitude bignum and stays exact. The AOT C runtime implements the
  same model (below).
- **JavaScript** emits an AINL `int` as a native `BigInt` (`42n`), which is
  arbitrary-precision and exact. Floats stay `number` (f64) wrapped in the
  `_Float` tag from card 1. See "JavaScript uses `BigInt`" below for the
  conversion boundaries this needs — and for the one comparison rule that is
  *not* simply "use exact integer math".

### What changed, and why it was a semantics change

The interpreter used to hold integers as `i64` and **promote to `f64` on
overflow** (`eval.rs::numeric_fold`), so `i64::MAX * 2` produced
`18446744073709551616.0` — already an approximation, since `f64` carries only
53 bits of exact integer precision, and not even the right approximation for
values above 2^53. That promotion path is **removed**. `(fact 25)` used to
print `15511210043330986055303168.0` (a float, rounded); it now prints the
exact integer `15511210043330985984000000`, and `(fact 100)` prints all 158
digits. Programs that relied on the old float output for out-of-range values
will see different text — that is the point of the change, and it is what
Python and Ruby have always done.

Scope is **integers only**: floats remain `f64`, and `/` still returns a float
(`(/ 1 2)` is `0.5`), so float precision edges are unchanged. Comparisons are
exact for two integers (they no longer round-trip through `f64`); an
int/float pair still compares as `f64`, so `(= 1 1.0)` remains `true`.

## What this means for "lossless"

"Lossless interop" and "byte-equal transpilation" (README.md,
`scripts/check-transpile.sh`, the M4 milestone in MASTER_PLAN.md) hold for
**any integer value AINL can express**, not just those inside a safe range:
the interpreter, the bytecode VM, the AOT binary and the Python, Ruby and
JavaScript targets all compute the exact mathematical value, because all of
them use arbitrary-precision integers.

Float behaviour is unchanged and still bounded: `/` returns a float, and float
arithmetic remains `f64` in every backend. So does one float-*display* rule —
see "the float display rule" below.

## JavaScript uses `BigInt`, and what that costs

The JS target's AINL `int` is a native `BigInt`. Nothing is hand-written here —
JS already has the arbitrary-precision integer, so unlike cards 2 and 3 there is
no bignum to port. The work is entirely in the **boundaries**, because a
BigInt is a different host type from a `number` and every JS-native API wants
`number`:

| boundary | conversion | why it is not automatic |
|---|---|---|
| `Math.sqrt`, `Math.floor`, `Math.abs` | `_num_f` → `Number(x)` | `Math.*` throws `TypeError` on a BigInt |
| `+=`, `-=`, `*=`, `%` | `_add`/`_sub`/`_mul`/`_mod` split on int-vs-float | `5n + 0.5` throws `TypeError: Cannot mix BigInt and other types` |
| `JSON.stringify` | hand-written `_json_ser` (pre-existing) | throws `Do not know how to serialize a BigInt` |
| array subscripts, slice bounds | `_ainl_idx` → `Number(i)` | a `bigint` index is not a valid property key |
| `Number.isInteger`, `process.exit` | `typeof i === "bigint"`, then `Number(code)` | `Number.isInteger(5n)` is `false` |
| `sort`, `min`/`max` | `_cmp` / `_sort_key` | see the comparison rule below |

That last column is the real cost, and it is the documented price of this
option: the emitted JS is no longer idiomatic JS arithmetic. A user who wants
to mix AINL-generated code with hand-written JS now has a `BigInt`/`number`
boundary to convert at, which native JS operators will not do for them.

**`mod` is Euclidean, and JS's `%` is not.** JS's `%` is truncated (the
remainder takes the dividend's sign), while the interpreter uses
`BigNum::rem_euclid`, whose answer is always in `[0, |b|)`. So `(mod -7 3)` is
`2` here and not `-2`. The adjustment is by `|b|` and *not* by `b`: the answer
does not depend on b's sign, so `(mod 7 -3)` is `1` and `(mod -7 -3)` is `2`,
and `r += b` would answer `-2` for the latter.

**Comparison is where exactness is a trap, not a goal.** Two rules, matching
`compare` in `ainl-core/src/eval.rs`:

* int vs int compares **exactly** (BigInt) — two large ints that are equal as
  f64 are not equal as ints.
* anything involving a float compares **as f64**, because that is what the
  interpreter does (`as_f64` on both sides).

The second rule is not a detail. Measured:

```lisp
(= 9007199254740993 (+ 9007199254740992 0.5))   ; interpreter: true
```

Exact BigInt math says `false` (2^53+1 ≠ 2^53); the f64 path says `true`,
because both sides round to 2^53. JS's native `<`/`===` on a BigInt against a
float compares *exactly*, so simply emitting the host operator would have
introduced a fresh divergence on precisely the values BigInt was adopted to
fix. `_cmp` picks the interpreter's rule, and
`numeric_divergence.rs` pins both sides of it.

`json-serialize`/`json-parse` carry the same model: an int serializes as its
exact digits, and `json-parse` reads an int-looking literal with `BigInt(t)`
on the *digit string* — `BigInt(Number(t))` would round first, turning
`"18446744073709551614"` into `18446744073709551616n`.

## the float display rule

Not new, and unchanged by any of the integer work above — recorded here because
it is the one place two backends still print different text for the *same*
value.

For a float, `print` uses the **exact** binary expansion on the interpreter
(`{:.1}`, so `(print 1e300)` emits 303 characters) while the transpilers print
the **shortest** round-tripping decimal. Both read back as the same `f64`; they
differ only in which decimal is chosen. It shows up only for large magnitudes —
`(* big 1.5)` prints `27670116110564327424.0` on the interpreter and
`27670116110564327000.0` on JS, which are the same f64 (the latter is the
shortest form of it). `json-serialize` uses the shortest form everywhere *by
design*; see "json-serialize has its own float rule" below for why.

## The AOT compiler targets the *interpreter's* model, not the transpilers'

`ainl compile` (Stage 2, `crates/ainl-cc`) is required to match the **AINL
interpreter** exactly — arbitrary-precision integers, the same as Python, Ruby
and JS.

That distinction is the whole point, and it is enforced rather than asserted:
`crates/ainl-cc/tests/aot_numeric.rs` compiles each edge case with `ainl_cc`,
runs the binary, and diffs against `ainl_core`'s own result. Covered:
integer arithmetic, the `i64` boundary in `+`/`-`/`*`, float shortest-round-trip
formatting (`(/ 1.0 3)`, `(+ 0.1 0.2)`), int/float mixing and cross-type
comparison, and runtime type errors.

**Closed.** The C runtime now carries the same arbitrary-precision integer as
the interpreter: a hand-written, zero-dependency bignum in
`crates/ainl-cc/src/runtime.c` (signed-magnitude, 32-bit limbs, with an
allocation-free `i64` fast path that keeps the common small-integer case off
the heap). An operation that leaves `i64` range widens to a bignum and stays
exact, so `(+ 9223372036854775807 1)` now prints `9223372036854775808` from the
AOT binary — the same digits the interpreter prints, no `.0`, no exponent, no
wrap, no trap. The old "promote to `f64` on overflow" model is gone from the C
runtime, and the two cases that used to cross the boundary
(`aot_numeric.rs::out_of_i64_range_is_exact_on_aot`,
`aot_stdlib.rs::aot_abs_of_i64_min_is_exact`) now assert *exact* parity with the
interpreter instead of pinning the divergence.

Writing the C runtime to that model surfaced three genuine defects, all of which
returned plausible-looking wrong answers rather than failing loudly:

- `(* 2 -9223372036854775808)` returned `0` instead of
  `-18446744073709551616`. The overflow check computed the unsigned magnitude
  product first — `2 × 2^63` wraps `uint64` to `0` — and the wrapped `0` then
  *passed* the range test. The check is now a division (`ub > limit / ua`), so
  the product is formed only once it is known to fit.
- Any product landing exactly on `2^63` in magnitude (e.g.
  `(* -1 -9223372036854775808)`) hit `-(int64_t)ur`, which is undefined
  behaviour at that magnitude (not representable as `int64_t`); clang folded it
  to `0`.
- The hot-path arithmetic inlines (`a_add`/`a_sub2`/`a_mul`/`a_div`) skipped the
  operand type check, reinterpreting a `V_STR`'s pointer as a `double`. That is
  a silent union-type-confusion read — `(+ 1 "a")` returned `1.0` rather than
  raising `expected a number, got str`.

The bignum port also had to keep the AOT compute loop at its pre-bignum speed:
`numeric_fold` (the `+`/`*` hot path) now does checked `i64` accumulation with
no allocation and only widens to the bignum path when an operand actually
overflows `i64`, so `aot_perf.rs`'s 30×-faster-than-tree-walk gate still holds.

So the numeric model is now identical across the whole toolchain —
interpreter, bytecode VM, AOT binary, and the Python, Ruby and JavaScript
transpilers — and there is a test that fails if it stops being so.

One known gap remains inside the AOT binary: `json-serialize` of an integer
outside `i64` range answers `cannot serialize a int`, where the interpreter and
the transpilers print the exact digits. It is a property of the C runtime's
bignum support (card 3), not of this card, and it is the one place the AOT
binary still differs on an integer.

## `json-serialize` has its own float rule

`json-serialize` prints floats in **plain fixed-point notation, never
scientific**, using the shortest decimal that reads back as the same `f64`,
with a mandatory `.0` on a whole value. That is *not* the rule `(print 1e300)`
uses, and the difference is deliberate.

`Value`'s `Display` — which is what `print` goes through — branches to `{:.1}`
for a whole float, which prints the **exact binary expansion**: `(print 1e300)`
emits 303 characters, the full exact value. `json-serialize` emits the
shortest round-tripping form instead (301 characters: a `1` and 300 zeros, plus
the `.0`). Both read back as the same `f64`; they differ only in which decimal
is chosen.

Following `Display` would have been the obvious move — it is the rule the rest
of the language uses, and the AOT C runtime's `format_float` is already a
hand-port of it, so reusing it would have saved a second float routine. Two
things say no:

- **The JS target could not follow it.** `toFixed` is specified only up to 1e21
  and falls back to exponential form beyond, so `(1e300).toFixed(1)` is the
  six-character string `"1e+300"`. Producing a 303-digit exact expansion from a
  JS `Number` needs arbitrary-precision decimal arithmetic that does not exist
  there. The shortest form is computable in all four backends from
  significant digits each of them can already produce, which is why it is the
  rule.
- **A second float routine would be untested by anything else.** The C runtime
  already has one correct shortest-form routine inside `format_float`; adding a
  near-duplicate for JSON means a second implementation that only the JSON tests
  exercise.

The `.0` suffix is the part with actual semantic weight: it is what keeps a
float distinguishable from an int in the output text. `(json-serialize 1.0)` is
`1.0` and `(json-serialize 1)` is `1`; drop the suffix and a reader could not
tell a float from an int at all, and `parse(serialize(v))` would stop being an
identity on floats.

JSON used to be a one-directional JS divergence, because JS has one number
type and an AINL `int` arrived as a `Number`. With `int` a `BigInt`, both
directions are exact: an int serializes as its own digits, and a whole *float*
still agrees, because `1.0` and `1` are the same JS value and emitting `1.0` is
what every backend does anyway. The output is valid JSON that re-parses to an
equal value in both cases; `crates/ainl-transpile/tests/json_parity.rs` pins
both halves so neither can drift.

## Options for what is still open (tracked here for whoever picks it up)

1. ~~**Make the AOT C runtime arbitrary-precision**, matching the
   interpreter.~~ **Done** — see "The AOT compiler targets the *interpreter's*
   model" above. The C runtime now carries a zero-dependency bignum with an
   `i64` fast path, and `aot_numeric.rs` / `aot_stdlib.rs` assert exact parity
   out of `i64` range.
2. ~~**Make JS match**, by emitting `BigInt`-based arithmetic instead of native
   `number` for the JS target.~~ **Done** — see "JavaScript uses `BigInt`"
   above. The emitted code is exact for every integer AINL can express, and the
   conversion boundaries are explicit. The price is real and paid: mixing the
   generated code with hand-written JS now needs a `BigInt`/`number`
   conversion, and the comparison rule had to be re-spelled (f64 for any
   float-involving pair) to match the interpreter rather than use exact math.
3. **Accept and scope the JS claim**: no longer needed for integers — all five
   backends are exact. What remains open is the *float display* rule above
   (exact expansion vs shortest round-trip), and three small pre-existing
   divergences that this work measured and did not touch:
   - `(/ 0)` answers `inf` on the interpreter and raises on the three
     transpilers.
   - `json-parse` of an integer literal beyond `i64` range yields a float on
     the interpreter and AOT binary (their reader parses `i64` and falls back
     to f64) and an exact int on Python, Ruby and JS.
   - `(mod n -1)`, `min`/`max` and comparison of an int against a float still
     differ between the interpreter/AOT and the Python/Ruby transpilers, whose
     host `Integer#%`/`Comparable` apply their own rules.

   §"Measured divergence" above is a regression fixture
   (`crates/ainl-transpile/tests/numeric_divergence.rs`) so a future change to
   any target's arithmetic doesn't silently drift without a test noticing, and
   `scripts/parity5.sh` runs one program through all five backends and diffs.
