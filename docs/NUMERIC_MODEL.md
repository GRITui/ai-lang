# Numeric model — what "lossless" actually covers

**Run it:** `cargo build --release && bash scripts/numeric-divergence-demo.sh` reproduces
the numbers below.

AINL has one integer type at the *source* level, but different runtime
representations of it: the interpreter's arbitrary-precision `BigNum`
(exact, never overflows), Python's and Ruby's arbitrary-precision integers,
and JavaScript's single `f64` number type. For any program whose integer
values stay within `i64` range (roughly ±9.2×10¹⁸) and away from
float-precision edges, all four agree byte-for-byte — that's what
`scripts/check-transpile.sh` verifies for the example programs. **Outside that
range the interpreter still agrees with Python and Ruby exactly, and diverges
only from JavaScript**, whose single `f64` type cannot represent large integers
precisely.

## Measured divergence

```lisp
(print (* 9223372036854775807 2))   ; i64::MAX * 2
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
```

| target | `i64::MAX * 2` | `(fact 25)` |
|---|---|---|
| interpreter (`ainl run`) | `18446744073709551614` | `15511210043330985984000000` |
| JS (`ainl transpile --to js`) | `18446744073709552000` | `1.5511210043330986e+25` |
| Python (`ainl transpile --to python`) | `18446744073709551614` | `15511210043330985984000000` |
| Ruby (`ainl transpile --to ruby`) | `18446744073709551614` | `15511210043330985984000000` |

Two answers from two different design choices, neither a bug in isolation:

- **The interpreter** uses `BigNum` (`crates/ainl-core/src/bignum.rs`), a
  hand-written arbitrary-precision integer with an allocation-free `i64` fast
  path. Integers never overflow: an operation that leaves `i64` range widens to
  a signed-magnitude bignum and stays exact. This matches Python and Ruby
  exactly, and is the model the AOT C target is required to match (below).
- **JavaScript** has exactly one number type (`f64`) for everything, so every
  AINL integer is already a float there — it rounds at 2^53, well before
  `i64::MAX`. There is no separate "promote on overflow" step; it is float
  arithmetic from the first multiplication.

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
`scripts/check-transpile.sh`, the M4 milestone in MASTER_PLAN.md) now hold for
**any integer value AINL can express**, not just those inside the safe range:
the interpreter, the bytecode VM and the Python/Ruby targets all compute the
exact mathematical value, because all three use arbitrary-precision integers.
What is *not* covered is JavaScript, which has no integer type at all and
rounds at 2^53 — a limit of the target language, not of the transpiler, and not
something a transpiler can fix without abandoning `number` for `BigInt`.

Float behaviour is unchanged and still bounded: `/` returns a float, and float
arithmetic remains `f64` in every backend.

## The AOT compiler targets the *interpreter's* model, not the transpilers'

`ainl compile` (Stage 2, `crates/ainl-cc`) does **not** reproduce the C
transpiler's wrapping behaviour described above. It is required to match the
**AINL interpreter** exactly — arbitrary-precision integers, the same as
Python/Ruby and *not* the same as the C/JS targets.

That distinction is the whole point, and it is enforced rather than asserted:
`crates/ainl-cc/tests/aot_numeric.rs` compiles each edge case with `ainl_cc`,
runs the binary, and diffs against `ainl_core`'s own result. Covered:
integer arithmetic, the `i64` boundary in `+`/`-`/`*`, float shortest-round-trip
formatting (`(/ 1.0 3)`, `(+ 0.1 0.2)`), int/float mixing and cross-type
comparison, and runtime type errors.

**Known gap (tracked, not fixed here).** The C runtime still implements the
*old* "i64 promoting to `f64` on overflow" model, so on values outside `i64`
range it now diverges from the interpreter: the C runtime's own overflow test
is correct for the model it implements (it neither wraps nor traps), but it
prints a float where the interpreter now prints exact digits. For example
`(+ 9223372036854775807 1)` gives `9223372036854775808` in the interpreter and
`9223372036854775808.0` from the AOT binary. Two of the cases in
`aot_numeric.rs` (`i64_overflow_promotes_to_float_not_wrap` and
`int64_min_magnitude_boundary`) are the ones that cross the boundary and are
therefore expected to fail until the C runtime gains arbitrary-precision
integers. The in-range cases — every example program, and everything
`scripts/check-transpile.sh` covers — still agree exactly, which is what the
rest of that file's tests assert.

Writing the C runtime to that model surfaced three genuine defects, all of which
returned plausible-looking wrong answers rather than failing loudly (they were
bugs under the old promoting-to-`f64` model, and remain bugs of the same kind
under the new one — the C runtime still implements the promoting model):

- `(* 2 -9223372036854775808)` returned `0` instead of
  `-18446744073709551616.0`. The overflow check computed the unsigned magnitude
  product first — `2 × 2^63` wraps `uint64` to `0` — and the wrapped `0` then
  *passed* the range test. The check is now a division (`ub > limit / ua`), so
  the product is formed only once it is known to fit.
- Any product landing exactly on `2^63` in magnitude (e.g.
  `(* -1 -9223372036854775808)`, which is in range and equals
  `9223372036854775808.0`) hit `-(int64_t)ur`, which is undefined behaviour at
  that magnitude (not representable as `int64_t`); clang folded it to `0`.
- The hot-path arithmetic inlines (`a_add`/`a_sub2`/`a_mul`/`a_div`) skipped the
  operand type check, reinterpreting a `V_STR`'s pointer as a `double`. That is
  a silent union-type-confusion read — `(+ 1 "a")` returned `1.0` rather than
  raising `expected a number, got str`.

So the transpiler divergence table above is a property of *cross-language*
targets; within the AINL toolchain (interpreter, bytecode VM, and AOT binary)
the numeric model is identical, and there is a test that fails if it stops being
so.

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

This is also why the JS divergence for JSON is one-directional. JS has one
number type, so an AINL `int` arrives as a `Number` and comes back out as
`1.0` — but a whole *float* agrees, because `1.0` and `1` are the same JS value
and emitting `1.0` is what every backend does anyway. The output is valid JSON
that re-parses to an equal value in both cases;
`crates/ainl-transpile/tests/json_parity.rs` pins both halves so neither can
drift.

## Options for what is still open (tracked here for whoever picks it up)

1. **Make the AOT C runtime arbitrary-precision**, matching the interpreter.
   Same reasoning as the interpreter change, in `crates/ainl-cc/src/runtime.c`:
   the emitted C would need a small bignum implementation and a dynamic `Value`
   payload for the big case. This is what closes the remaining in-toolchain
   gap, and the two failing cases in `aot_numeric.rs` are the fixture.
2. **Make JS match**, by emitting `BigInt`-based arithmetic instead of native
   `number` for the JS target. Loses JS-native ergonomics (no mixing with
   `Math.*`, `JSON`, etc. without explicit conversion) in exchange for integer
   fidelity. This is the only remaining *cross-language* divergence.
3. **Accept and scope the JS claim** (current state): the interpreter, the VM
   and the Python/Ruby targets are exact for all integers; JavaScript is not,
   because `f64` cannot be. §"Measured divergence" above is a regression
   fixture (`crates/ainl-transpile/tests/numeric_divergence.rs`) so a future
   change to any target's arithmetic doesn't silently drift without a test
   noticing.
