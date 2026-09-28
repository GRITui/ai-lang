# Numeric model — what "lossless" actually covers

**Run it:** `cargo build --release && bash scripts/numeric-divergence-demo.sh` reproduces
the numbers below.

AINL has one integer type at the *source* level, but four different runtime
representations of it: the interpreter's `i64` (promoting to `f64` on
overflow), Python's and Ruby's arbitrary-precision integers, and JavaScript's
single `f64` number type. For any program whose integer values stay within
`i64` range (roughly ±9.2×10¹⁸) and away from float-precision edges, all four
agree byte-for-byte — that's what `scripts/check-transpile.sh` verifies for
the example programs. **Outside that range, they diverge**, and no amount of
transpiler polish fixes this without changing what "the same value" means in
at least one of the four runtimes.

## Measured divergence

```lisp
(print (* 9223372036854775807 2))   ; i64::MAX * 2
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
```

| target | `i64::MAX * 2` | `(fact 25)` |
|---|---|---|
| interpreter (`ainl run`) | `18446744073709551616.0` | `15511210043330986055303168.0` |
| JS (`ainl transpile --to js`) | `18446744073709552000` | `1.5511210043330986e+25` |
| Python (`ainl transpile --to python`) | `18446744073709551614` | `15511210043330985984000000` |
| Ruby (`ainl transpile --to ruby`) | `18446744073709551614` | `15511210043330985984000000` |

Four different answers from four different design choices, none of them a
bug in isolation:

- **The interpreter** promotes `i64` overflow to `f64` (`eval.rs::numeric_fold`),
  matching typical dynamic-language "int until it doesn't fit" semantics —
  but `f64` only has 53 bits of exact integer precision, so the promoted
  value is already an approximation once it's produced.
- **Python and Ruby** have arbitrary-precision integers built into the
  language; the transpiler emits plain `+`/`*`, and those languages just...
  don't overflow. They compute the mathematically exact product/factorial.
- **JavaScript** has exactly one number type (`f64`) for everything, so
  every AINL integer is already a float there — there's no separate "promote
  on overflow" step, it's float arithmetic from the first multiplication.

## What this means for "lossless"

"Lossless interop" and "byte-equal transpilation" (README.md,
`scripts/check-transpile.sh`, the M4 milestone in MASTER_PLAN.md) are true
**for programs whose values stay in the safe range** — verified for the
example programs, which all do. They are not a claim about arbitrary AINL
programs. A program that intentionally overflows `i64` (e.g. computing large
factorials, hashing, or checksums) will produce four different numeric
results depending on where it runs, and none of the four transpilers can
detect this ahead of time — it's a runtime value, not something visible in
the static AST a transpiler works from.

## The AOT compiler targets the *interpreter's* model, not the transpilers'

`ainl compile` (Stage 2, `crates/ainl-cc`) does **not** reproduce the C
transpiler's wrapping behaviour described above. It is required to match the
**AINL interpreter** exactly — i64 with promotion to f64 on overflow, the same as
Python/Ruby and *not* the same as the C/JS targets.

That distinction is the whole point, and it is enforced rather than asserted:
`crates/ainl-cc/tests/aot_numeric.rs` compiles each edge case with `ainl_cc`,
runs the binary, and diffs against `ainl_core`'s own result. Covered:
integer arithmetic, i64 overflow at both `INT64_MAX`/`INT64_MIN` in `+`/`-`/`*`,
float shortest-round-trip formatting (`(/ 1.0 3)`, `(+ 0.1 0.2)`), int/float
mixing and cross-type comparison, and runtime type errors.

Writing the C runtime to that model surfaced three genuine defects, all of which
returned plausible-looking wrong answers rather than failing loudly:

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

## Options if this needs to be closed (not done — tracked here for whoever picks it up)

1. **Make the interpreter arbitrary-precision too**, matching Python/Ruby.
   Requires hand-writing a bignum type (add/sub/mul/div/compare/to-string) to
   preserve the zero-dependency runtime — a real feature, not a tweak.
2. **Make JS match**, by emitting `BigInt`-based arithmetic instead of native
   `number` for the JS target. Loses JS-native ergonomics (no mixing with
   `Math.*`, `JSON`, etc. without explicit conversion) in exchange for
   integer fidelity.
3. **Accept and scope the claim** (current state): document that
   losslessness holds within `i64`/float-safe range, and treat overflow
   behavior as an explicit, tested boundary rather than an implicit promise.
   This is what's currently done; §"Measured divergence" above is a
   regression fixture (`crates/ainl-transpile/tests/numeric_divergence.rs`)
   so a future change to any one target's arithmetic doesn't silently drift
   further from the other three without a test noticing.
