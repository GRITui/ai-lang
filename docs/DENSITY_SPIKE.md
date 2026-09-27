# Dense-AINL density spike — go/no-go (card t_4bb53833)

> **Status: spike complete — recommendation at the bottom.**
> This is a *de-risking* spike, not a shipped feature. It answers two
> questions at once: (1) does a **dense** AINL surface syntax actually use
> **fewer tokens** than Python/JS/Ruby, and (2) does the denser grammar **stay
> well-formed and constrainable** in a real GBNF decoder? No Rust was changed.

## TL;DR — the two headline answers

| Question | Answer | Evidence |
|---|---|---|
| **Token win?** Does dense AINL go **below Python** on any example? | **NO** — not on any example. Best case is `maps` at **+3%** vs Python. It does beat **Ruby** on `maps` (−8%) and is near-parity with **JS** on `maps` (−3%), but never below Python. | §3 token table |
| **Constrainable?** Does the denser grammar stay well-formed + constrainable? | **YES** — well-formed in llguidance *and* llama.cpp, and a real llama.cpp constrained decoder emits **100% GBNF-member** output (vs 0% unconstrained). | §4, §5 |

**Recommendation: DO NOT PROCEED with the density pivot** (surface syntax +
Rust rewrite) *as a means to beat Python on tokens* — the cheap, de-risking
surface change alone does not get below Python, so the expensive rewrite it
gates is not justified by the density goal. Details + the nuance in §7.

---

## 1. Context

The founding claim is that AINL is "high-density… optimized for LLM context
windows." `docs/BENCHMARK.md` measured the *current* S-expression AINL at
**+108% vs Python** (worse than JS/Ruby too). Before committing to a syntax
pivot + Rust rewrite, this spike tests whether a dense surface can flip that to
a win, *and* whether the denser grammar survives contact with a real GBNF
decoder.

The pivot is gated on the density goal: if the surface change doesn't beat
Python, the rewrite it unlocks isn't worth it.

## 2. The dense syntax design

A **surface-only** change over the existing S-expression core. The AST /
transpile target is unchanged. Levers applied:

- **Infix** arithmetic + comparison with standard precedence: `1 + 2 * 3`,
  `n < 2` (was `(+ 1 (* 2 3))`, `(< n 2)`).
- **`=`** for assignment, **`==`** for equality (was `(def x …)` / `(= a b)`).
- **`{ }`** blocks + `if`/`while`/`return` (was `(let …)` / `(while …)`).
- **`fn (params) { … }`** functions, **`[...]`** lists, **`{k: v}`** maps,
  **`#`** comments, ternary `? :`.

### Before / after (same program)

S-expression AINL (`examples/hello.ainl`):
```
(print "hello, ai-native world")
(def sq (fn (x) (* x x)))
(print "sq 12 =" (sq 12))
(def sum (fn (& xs)
  (def go (fn (lst acc)
    (if (= (len lst) 0) acc
      (go (rest lst) (+ acc (first lst))))))
  (go xs 0)))
(print "sum 1..5 =" (sum 1 2 3 4 5))
```

Dense AINL (`spike/dense-ainl/hello.ainl`):
```
print("hello, ai-native world")
sq = fn(x) { x * x }
print("sq 12 =", sq(12))
sum = fn(&xs) {
  go = fn(lst, acc) {
    if len(lst) == 0 { return acc }
    return go(rest(lst), acc + first(lst))
  }
  return go(xs, 0)
}
print("sum 1..5 =", sum(1, 2, 3, 4, 5))
```

**Braces, not indentation, for blocks — on purpose.** Significant indentation
is context-sensitive and **cannot be expressed in a context-free GBNF**, so
braces are the price of staying GBNF-constrainable. (An indent-based surface
would fail question 2.)

The four dense examples are in `spike/dense-ainl/` (deliberately **not** in
`examples/` — `scripts/check-transpile.sh` globs `examples/*.ainl` and runs the
*current* S-expression parser on each, which would reject dense files and break
CI).

## 3. Token table — dense AINL vs Python / JS / Ruby

Method: identical to `docs/BENCHMARK.md` — tiktoken (real OpenAI tokenizers),
**hand-written idiomatic equivalents** (not transpiler output). Run:
`python3 spike/bench_dense.py`.

### GPT-4o (o200k_base)

| example | dense | Python | JavaScript | Ruby |
|---------|------:|-------:|-----------:|-----:|
| hello   | 102   | 61 (+67%)  | 80 (+28%)  | 68 (+50%) |
| fib     | 128   | 97 (+32%)  | 120 (+7%)  | 101 (+27%) |
| lists   | 132   | 97 (+36%)  | 106 (+25%) | 106 (+25%) |
| maps    | 121   | 118 (+3%)  | 125 (−3%)  | 132 (−8%) |
| **TOTAL** | **483** | **373 (+29%)** | **431 (+12%)** | **407 (+19%)** |

### GPT-4 (cl100k_base)

| example | dense | Python | JavaScript | Ruby |
|---------|------:|-------:|-----------:|-----:|
| hello   | 102   | 61 (+67%)  | 80 (+28%)  | 66 (+55%) |
| fib     | 128   | 97 (+32%)  | 120 (+7%)  | 98 (+31%) |
| lists   | 132   | 97 (+36%)  | 106 (+25%) | 106 (+25%) |
| maps    | 121   | 118 (+3%)  | 125 (−3%)  | 130 (−7%) |
| **TOTAL** | **483** | **373 (+29%)** | **431 (+12%)** | **400 (+21%)** |

_(% = dense-AINL relative to that language; negative = dense-AINL uses **fewer**
tokens.)_

### Does dense AINL go below Python?

**No — not on any example.** The closest is `maps` at **+3%** (o200k) / **+3%**
(cl100k). It only goes below Python's *competitors* on one example: **Ruby
`maps` −8%** and **JS `maps` −3%**. Against Python itself it is positive
everywhere.

### But the pivot *is* a real density improvement over S-expression AINL

Comparing the same programs, S-expression AINL (from `BENCHMARK.md`) vs dense:

| example | S-expr AINL | dense AINL | change |
|---------|------------:|-----------:|-------:|
| hello   | 127         | 102        | **−20%** |
| fib     | 202         | 128        | **−37%** |

So the surface change cuts the S-expression overhead substantially (the
`+108% vs Python` gap shrinks to `+29%`), but **Python is already extremely
token-dense** for these programs (builtins like `sum`/`len`, list/dict
comprehensions, `lambda`), and a surface change alone can't close the remaining
gap. The residual cost in dense AINL is: the `fn` keyword + braces + explicit
`return` per function, and named builtin calls where Python has a one-token
builtin.

## 4. GBNF well-formedness

The dense GBNF is hand-authored in `spike/dense-ainl/dense.ainl.gbnf` (36
rules). Two independent checks:

1. **llguidance** (the GBNF front-end family): parses the grammar; a negative
   control (a dangling rule reference) is correctly rejected, proving the check
   is not vacuous. → **well-formed.**
2. **llama.cpp 0.5.0** (the actual decoder): **initially rejected the grammar**
   with `parse: error parsing grammar: expecting ::= at _stmt`. Root cause:
   **llama.cpp's GBNF front-end rejects rule names containing `_`** — its
   rule-name token is `[a-zA-Z][a-zA-Z0-9-]*` (hyphens OK, underscores not).
   `if_stmt` was read as rule `if`, then it expected `::=` at `_stmt`. Fixed by
   renaming all rules to **kebab-case** (`if-stmt`, `ident-start`, …), matching
   the repo's own AINL GBNF convention (`sym-char`, `comment`). After the fix,
   llama.cpp **accepts** the grammar and applies the constraint. → **well-formed
   in the real decoder.**

This is a genuine "stays constrainable in a real decoder" finding: the dense
grammar is a bit more complex than the S-expression one, and it surfaced a real
llama.cpp constraint (no `_` in rule names) that the simpler grammar never hit.

## 5. Constraint-decoding smoke test

Method: identical to `scripts/gen-harness/run_generation.py` (C2) — run
Qwen2.5-0.5B-Instruct Q4_K_M through a **real** `llama-cli --grammar-file`
constrained decoder using the dense GBNF, in two modes, and check GBNF
membership with a **sound** fast detector (`spike/dense_fast.py`, cross-
validated against the reference Earley — see §6). Run:
`python3 spike/dense_smoke.py --model …/qwen2.5-0.5b-instruct-q4_k_m.gguf`.

**Result (n=8 prompts, temp 0):**

| mode | GBNF-member rate |
|---|---|
| **constrained** (dense GBNF applied) | **8/8 (100%)** |
| **unconstrained** (no grammar) | **0/8 (0%)** |

The 0.5B model is too weak for *semantics* — its constrained output is its
degenerate template (`10 years ago` repeated, each line a valid bare
expr-stmt) — but the **syntax** is exactly what's under test, and it is
constrained: the decoder can only emit dense-GBNF members. Unconstrained, the
model emits its free-form `/leetcode` Python template, which is not dense AINL.

**Conclusion: the denser grammar is still constrainable in a real GBNF decoder.**
Question 2 = YES.

## 6. Detector soundness (cross-validation)

The smoke test's membership check uses a fast recursive-descent parser
(`spike/dense_fast.py`, O(n)) rather than the O(n³) reference Earley (too slow
for per-output checks on ~400-char generations) — the same approach that pins
`scripts/gen-harness/gbnf_fast.py`. To prove the fast parser is *sound* (accepts
exactly the GBNF language), it was cross-validated against the reference Earley
(`scripts/gbnf-conformance.py::gbnf_accepts`):

- **4 real deliverable examples** (hello/fib/lists/maps): fast = Earley on all 4.
- **36 clean + permissive case strings** (accept/reject): fast = Earley on all 36.
- **6 fuzz strings** (grammar-walker, depth/length-capped): fast = Earley on all 6.

**46/46 agreement.** Run: `python3 spike/crossvalidate_dense.py` +
`python3 spike/dense_fast.py` (self-test).

### A documented GBNF permissiveness (not a bug)

GBNF has **no negative-lookahead**, so keywords (`if`/`fn`/`while`/`return`)
are **not reserved** and bare expression statements are **whitespace-separated
with no delimiter**. Consequences the GBNF (and the fast parser, correctly)
accepts but a human would call malformed:

- `x 1` → two bare expr-stmts.
- `if a x = 1 { }` → `if`(expr) `a`(expr) `x=1`(assign) `{}`(empty map).
- `fn = 1` → `fn` usable as an identifier.

This makes the GBNF a **superset** of "intended" dense-AINL. It does not affect
the smoke test (constrained output is still valid dense-AINL), but the future
dense *parser* can be stricter than the GBNF (reject these) — the GBNF only
needs to be a safe *subset* for decoding, which it is. Worth a note in the
Phase-1 parser spec.

## 7. Recommendation — DO NOT PROCEED (with the nuance)

**DO NOT PROCEED with the density pivot** (surface syntax + Rust
lexer/parser/grammar rewrite) *as a means to beat Python on tokens.*

Reasoning:
- The surface change is the **cheap, low-risk, de-risking** part — and it is
  exactly what this spike built and measured. It does **not** go below Python on
  any example (+29% vs Python total; best case `maps` +3%).
- The pivot's **expensive** part (the Rust rewrite) is only justified if the
  surface change showed "beat Python" was achievable. It doesn't. So the cost
  is not recovered by a token win that doesn't materialize.
- Per the card's own gate — *"if either answer is no, the density goal is not
  worth the pivot"* — the token answer is no.

**The nuance (so this isn't thrown away):**
- The surface change is a **genuine density improvement** over S-expression
  AINL (hello −20%, fib −37% tokens; the +108% vs Python gap shrinks to +29%),
  and it **stays fully constrainable** (100% in a real decoder). So if the owner
  wants a *nicer, denser, still-constrainable* AINL, the surface change is worth
  doing **on its own merits** (readability + 20–37% density gain) — just not as
  a path to "beat Python."
- **Beating Python on tokens is not a surface-syntax problem.** Python is
  already near the token floor for these programs (builtins, comprehensions,
  `lambda`). To go below it you need **language features** (a `sum` builtin,
  list/dict comprehensions) or a **bytecode/opcode representation** (the
  "stack-based bytecode" option `BENCHMARK.md` already lists) — a larger,
  separate commitment, not a surface pivot. If "density winner" is a hard
  requirement, that is the actual path, and it deserves its own card + its own
  before/after benchmark.

## 8. Artifacts & how to reproduce

All under `spike/` (no Rust touched; `examples/` untouched so
`check-transpile.sh` is unaffected):

| file | what |
|---|---|
| `spike/dense-ainl/{hello,fib,lists,maps}.ainl` | the 4 dense examples |
| `spike/dense-ainl/dense.ainl.gbnf` | the dense GBNF (kebab-case rules) |
| `spike/bench_dense.py` | token benchmark (tiktoken) |
| `spike/dense_gbnf_check.py` | llguidance well-formedness + Earley example-membership |
| `spike/dense_fast.py` | sound O(n) GBNF membership detector (+ self-test) |
| `spike/crossvalidate_dense.py` | fast-vs-Earley cross-validation |
| `spike/dense_smoke.py` | llama.cpp constrained-decoding smoke test |
| `spike/dense-prompts.txt` | smoke-test prompts |
| `spike/results-dense/` | smoke-test raw outputs + `results.csv`/`.json` |

Reproduce:
```
python3 spike/bench_dense.py            # token table
python3 spike/dense_gbnf_check.py       # well-formedness + example membership
python3 spike/dense_fast.py             # detector self-test
python3 spike/crossvalidate_dense.py    # fast-vs-Earley soundness
python3 spike/dense_smoke.py --model <gguf>   # constrained-decoding smoke test
```
