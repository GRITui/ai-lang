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
