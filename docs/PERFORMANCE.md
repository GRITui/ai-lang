# Execution speed (interpreter) — results & honest framing

**Framing first.** AINL is a tree-walking interpreter. Speed is **not** a design
goal — the value prop is the *reliability and verifiability of machine
generation*, and real execution happens in the transpiled Python/JS/Ruby target,
not in the AINL interpreter. This doc records the interpreter's measured speed
so nobody is surprised, and documents the hard evaluation-step cap that bounds
how much work a single program may do.

## Method

Three benchmarks — integer sum-to-N, iterative fib(30), and list build
(prepend) + sum — run at N=40,000, best of 3 runs, same machine (macOS arm64,
AINL release build). All three languages produce byte-identical output
(800020000 / 832040 / 800020000), so the comparison is apples-to-apples.

## Results

| Runtime | Best of 3 | vs Python | vs Ruby |
|---|---|---|---|
| AINL (tree-walking interp) | 3.23s | 10.4× slower | 17.0× slower |
| Python 3 | 0.31s | — | 1.6× slower |
| Ruby | 0.19s | 1.6× faster | — |

## The step-cap finding (the important part)

`MAX_STEPS = 2_000_000` in `crates/ainl-core/src/eval.rs` (line ~32) is a
per-top-level-run budget. The full 3-benchmark program fits at N=40,000 but
**fails at N=45,000** with "step limit exceeded". This is a hard ceiling on
loop iterations / program work, independent of speed.

Consequence: a model can emit a correct-but-large program that the interpreter
refuses to run — which undermines the "validate by running" part of the value
prop. This is a **known limitation**. Fix direction: make the cap configurable
/ raise it, and document it. (Not implemented here — docs-only card.)

## Verdict

Do not benchmark AINL on speed — it always loses to CPython/MRI as an
interpreter. The differentiator is the constrained-generation correctness
number (see [docs/GENERATION.md](GENERATION.md)). Speed is a non-goal.
