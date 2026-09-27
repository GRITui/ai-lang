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

> **Update (2026-09-27):** the 3.23s figure above is the *pre-fix* AINL number,
> kept for the before/after record. After the cons-cell fix (see
> [Root cause of the 3.23s](#root-cause-of-the-323s)) the same composite runs
> in **~0.13s** on the same machine — a **~25× speedup** — with byte-identical
> output. The interpreter is now on par with, or faster than, CPython/MRI for
> this workload. The "always loses to CPython/MRI" verdict below was a
> consequence of the list bug, not of the tree-walking design.

## Root cause of the 3.23s

The 3.23s was **not** the tree-walking interpreter being slow at loops — it was
an O(n²) list representation. PO decomposition at N=40,000:

| Work | Time | Notes |
|---|---|---|
| Pure loop (40k iterations) | 0.04s | on par with Python/Ruby — loops were never the problem |
| List build via `cons` (prepend) | O(n²) | the entire 3.2s |

The list build alone scaled quadratically with N:

| N | 5k | 10k | 20k | 40k |
|---|---|---|---|---|
| `cons`-build time | 0.05s | 0.21s | 0.81s | 3.20s |

**Why O(n²).** Lists were `Value::List(Rc<Vec<Value>>)`, and every "mutation"
cloned the whole backing `Vec`:

- `cons` (prepend): `out.extend(l.iter().cloned())` — copied all n elements to
  add one at the front.
- `rest` (drop head): `l.iter().skip(1).cloned().collect()` — copied n−1.
- `push` (append): full copy + `extend`.

Building a 40k list by repeated prepend therefore did 1 + 2 + … + 40000 ≈ 8×10⁸
element copies. That quadratic work is also what burned the 2M evaluation-step
budget, which is why the step cap tripped on legitimate programs at N≥45k.

### The fix: linked cons cells

Lists are now `Value::List(Rc<ConsCell>)` where
`ConsCell { head: Value, tail: Option<Rc<ConsCell>>, len: usize }`:

- `cons` → **O(1)**: allocate one cell pointing at the old head.
- `rest` / `first` → **O(1)**: pointer read.
- `len` → **O(1)**: cached in the cell.
- `push` → **O(n)**: traverse to the end (documented; the old `Vec` version
  paid the same O(n) copy, so this is not a regression).
- `nth` / `map`-style walks → **O(n)**: correct semantics, linear.

Measured after the fix (same machine, release build):

| Benchmark | Before | After |
|---|---|---|
| 40k `cons`-build (the O(n²) case) | 3.20s | **0.04s** (~80×) |
| 3-benchmark composite (N=40k) | 3.23s | **0.13s** (~25×) |
| 100k `cons`-build | (would be ~20s) | 0.10s |

A regression test in `crates/ainl-core` guards this: building a 20,000-element
list via `cons` must complete in **< 500ms** (generous bound so slow CI
runners don't flake). An iterative `Drop` for `ConsCell` keeps dropping a long
list off the stack (the derived drop would recurse once per cell and overflow a
small test thread's stack).

## The step-cap finding (the important part)

`MAX_STEPS = 2_000_000` in `crates/ainl-core/src/eval.rs` (line ~32) is a
per-top-level-run budget. Before the fix, the full 3-benchmark program fit at
N=40,000 but **failed at N=45,000** with "step limit exceeded" — but that was
the O(n²) list work burning the budget, not the program being genuinely large.

**With O(1) `cons`, the cap is now a true safety valve.** The same 40k
composite runs in ~0.13s and the quadratic list cost that ate the budget is
gone, so the 2M-step ceiling only trips on programs that actually do
quadratic-or-worse *logical* work (deep recursion, nested loops), which is what
a step cap is for. The cap remains a known limitation for legitimately large
but linear programs; the fix direction (make the cap configurable / raise it,
and document it) is unchanged.

## Verdict

The pre-fix "do not benchmark AINL on speed — it always loses to CPython/MRI"
verdict was an artifact of the O(n²) list bug, not of the interpreter design.
With cons cells the interpreter is on par with CPython/MRI for list-heavy
workloads (0.13s vs 0.31s / 0.19s on the composite). Speed is still a
non-goal — the differentiator remains the constrained-generation correctness
number (see [docs/GENERATION.md](GENERATION.md)) — but the interpreter is no
longer the thing that makes AINL look slow.
