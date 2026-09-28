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

## Bytecode VM (2026-09-27)

A bytecode compiler + stack machine was added to `ainl-core`
(`src/code.rs` = `Instr` + self-contained `FnCode`, `src/vm.rs` = compiler +
dispatch loop). `run`/`run_str`/`run_in` now route through the VM; the
tree-walking evaluator is retained as `run_in_tree_walk` for comparison and
fallback. Semantics are identical — the full test suite (88 tests, including
the recursion/step-limit and `LIVE_SCOPES` memory tests) passes unchanged, and
there are zero new dependencies.

Design (settled by the PO): flat dispatch loop, O(1) dense local slots (hot
variables resolve to fixed slot indices, no `HashMap`), an explicit frame stack
(recursion is a data operation, not native call-stack recursion), and a local
step counter that replaces the tree-walk's thread-local `tick()`. A per-function
`env_active` flag means a closure-free hot loop never touches the `Env`
`HashMap` at all. Malformed special forms (wrong arity, non-symbol `def` name,
…) are compile-time errors — the compiler returns `Err` rather than emitting a
runtime-error instruction.

Measured on the 40,000-iteration sum-to loop, release build, best of 3
(`crates/ainl-core/tests/vm_perf.rs`):

| Runtime | 40k sum-to loop | Speedup |
|---|---|---|
| AINL tree-walk | ~35 ms | 1.0× |
| AINL bytecode VM | ~5.8 ms | **~6×** |

The gate is ≥5× over tree-walk; the VM lands at ~6×. The single largest win was
eliminating a per-`LoadSlot` `String` clone (160,000 heap allocations in the
benchmark); the rest came from dense slots, a precomputed slot-name-symbol pool
for `def`, and inlined integer fast paths for `+` and `<`.

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

## Stage 2 — AOT compiler (AINL → C → `cc`)

`crates/ainl-cc` compiles the AST to a **single self-contained C file** — the
micro-runtime (value model, refcounting, cons cells, symbol interning, scopes,
closures, 27 builtins, step counter) is inlined into the output, so the compiled
program links against nothing but libc. `ainl compile prog.ainl -o prog` runs
`cc`; `ainl run` is untouched and still goes through the VM.

The trade is explicit: **the compiler needs a host toolchain (`cc`); the output
needs nothing.** Zero new external dependencies — `ainl-cc` depends only on
`ainl-core`.

### The three numbers

40,000-iteration sum-to-N loop (`bench/loop40k.ainl`), release build, best of 25.
Measured on Apple Silicon / macOS 26.2, clang `-O2`
(`./scripts/bench-aot.sh`):

| Engine | 40k sum-to loop | vs tree-walk |
|---|---|---|
| AINL tree-walk (in-process) | 35.4 ms | 1.0× |
| AINL bytecode VM (in-process) | 5.9 ms | ~6× |
| **AINL AOT (whole process)** | **2.25 ms** | **~16×** |
| native Rust equivalent (whole process) | 1.77 ms | ~20× |

So the "on par with Rust" pillar holds: **the AOT binary is within 1.27× of a
hand-written Rust program** for the same loop (target was ≤2×).

### Why ~1.27×, when the loop itself is 50× faster than the interpreter

Of the AOT binary's 2.25 ms, **1.56 ms is process startup** — fork, exec, dynamic
link, libc init. That floor is measured directly, not estimated: a trivial
compiled program (`(print 1)`) costs 1.56 ms, and the same harness measures
1.76 ms for the Rust equivalent. Subtracting it:

| | AOT | Rust |
|---|---|---|
| total (whole process) | 2.25 ms | 1.77 ms |
| process startup floor | 1.56 ms | 1.76 ms |
| **compute only (total − floor)** | **0.69 ms** | **not resolvable** |

AOT compute-only is 0.69 ms = **~51× the tree-walk**
(`crates/ainl-cc/tests/aot_perf.rs` asserts this ≥30× gate).

The Rust compute column is deliberately blank. Its startup floor (1.76 ms) came
out within noise of its total (1.77 ms), so the subtraction is ~0.01 ms — the
Rust loop's entire work is below this harness's noise floor, and quoting a
compute ratio against it would be inventing precision. What the data does
support is the whole-process figure — **AOT 2.25 ms vs Rust 1.77 ms, i.e.
1.27×** — because that is the number a user of either program actually pays, and
both sides are measured the same way. A dynamically-typed, refcounted value
model with a tag check on every arithmetic op will not match a bare `i64` loop;
the AOT backend is within 2× of Rust for the same program, which was the target.

**Methodology note.** These process-inclusive numbers are measured with
`scripts/execbench.c` (fork+exec the target from C, no intermediate process).
Timing a ~2 ms binary with `python3 -c 'subprocess.run(...)'` or `/usr/bin/time`
charges the *timer's* own startup to the binary: measured here as ~27 ms, which
is >10× the thing being measured and makes every AOT number look like 28 ms. Any
sub-millisecond AOT benchmark that uses an external timer is measuring the
timer.

### What the AOT backend had to fix to get there

Four real bugs, all found by the parity/numeric test suites rather than by the
examples (every example and the 40k loop passed throughout):

1. **Variadic operators were truncated.** `gen_call`'s fast path matched any call
   whose callee was a known 2-arg operator without checking arity, so `(+ 1 2 3 4 5)`
   inlined the first two operands and silently **dropped the rest** — printing
   `3` where the interpreter prints `15`. The 40k loop uses 2-arg `+`, which is
   why the headline benchmark never caught it.
2. **Arithmetic inlines never type-checked.** `a_add`/`a_sub2`/`a_mul`/`a_div`
   read `u.f` unconditionally on the non-int path, reinterpreting a `V_STR`'s
   `Str*` as a `double`. `(+ 1 "a")` returned `1.0` instead of raising
   `expected a number, got str`; `(* 2 "a")` returned a garbage float. Silent UB.
3. **`checked_mul` overflowed its own check.** For `INT64_MIN` (magnitude 2^63),
   `(* 2 -9223372036854775808)` computed magnitudes `2 × 2^63 = 2^64`, which wraps
   `uint64` to 0 — the wrap then *passed* the `ur > limit` test and returned `0`.
   Fixed with a division-based check (`ub > limit / ua`) so the product is only
   formed once known to fit.
4. **Undefined negation at the sign boundary.** `-(int64_t)ur` is UB when `ur` is
   exactly 2^63 (not representable as `int64_t`); clang folded it to `0`. Fixed
   with `(int64_t)(0 - ur)`, the same bit pattern without UB.

`crates/ainl-cc/tests/` now covers all of it: `aot_parity.rs` (4/4 examples
byte-identical + step cap), `aot_numeric.rs` (i64-overflow promotion to f64,
float formatting, type errors — each asserted against `ainl_core`, not a
hardcoded string), `aot_perf.rs` (the ≥30× compute gate).

### Codegen shape

- **Top-level `def`s** use dense global slots (`g_top[]`), so the hot loop
  indexes `g_top[0]`/`g_top[1]` directly — no name lookup.
- **Function locals** use the runtime's name-based `Scope` chain, which is what
  closures, `let` and nested `def` need.
- **Binary operators** inline to `a_add`/`a_lt`/… instead of going through
  `v_call`'s switch; 3+-operand forms fall back to `v_call` → `numeric_fold`.
- **`def`'s result symbol is pre-interned.** `def` evaluates to its own name, and
  in a hot loop that symbol is built and immediately discarded. Calling
  `v_sym()` per iteration re-ran the intern-table probe (FNV hash + `strcmp`)
  for a constant. Hoisting each distinct def name to one file-scope `static`
  initialized once at startup took the loop from 28.7× to ~51× the tree-walk —
  a 1.6× win from deleting pure waste.

### Safety property preserved

The C runtime keeps the interpreter's step cap: **2,000,000 by default**,
overridable with the `AINL_MAX_STEPS` environment variable. Ticks are per
`while` iteration and per function call (the two runaway vectors) rather than per
node, so the cap bounds runaway work without adding per-expression cost to the
hot loop. Exhaustion sets a runtime error and exits non-zero. Enforced in CI by
`scripts/check-aot.sh` and in `aot_parity.rs`.

## Verdict

The pre-fix "do not benchmark AINL on speed — it always loses to CPython/MRI"
verdict was an artifact of the O(n²) list bug, not of the interpreter design.
With cons cells the interpreter is on par with CPython/MRI for list-heavy
workloads (0.13s vs 0.31s / 0.19s on the composite). Speed is still a
non-goal — the differentiator remains the constrained-generation correctness
number (see [docs/GENERATION.md](GENERATION.md)) — but the interpreter is no
longer the thing that makes AINL look slow.
