# Constrained generation (proof of concept)

This is the first demonstration that AINL's core thesis is real: **a small
local model, run through a real GBNF-constrained decoder, can be forced to
emit only syntactically valid AINL.** Before this, the project shipped a
grammar export but had never run a model through an actual constrained
decoder. This page is the method, the data, and the honest interpretation.

## Method

- **Decoder:** llama.cpp `llama-cli` (v0.5.0, build 11146) with
  `--grammar-file <gbnf>`. GBNF is llama.cpp's native constraint format, so
  this is the most direct possible proof. (0.5.0 needs `-st`/`--single-turn`
  or it drops into an interactive REPL; and the long form
  `--chat-template qwen`.)
- **Model:** `Qwen2.5-0.5B-Instruct` (Q4_K_M), a small free-tier GGUF.
- **Grammar:** the exact output of `ainl grammar --gbnf` (the conformance-
  verified grammar from C1 — see [SYNTAX.md](SYNTAX.md) §6). Nothing hand-
  written.
- **Prompts:** 20 varied task prompts (see [the prompt list](#prompt-list)
  and `scripts/gen-harness/prompts.txt`).
- **Modes:** each prompt is run twice — **constrained** (the AINL GBNF
  applied) and **unconstrained** (no grammar, the model simply asked to emit
  AINL). Same model, same prompts, `--temp 0`.
- **Checks per generation:**
  1. **GBNF membership** — is the output a member of the *language of the
     exported GBNF*? This is the sound, decisive check (see below).
  2. `ainl ast` — does it parse as AINL? (supplementary)
  3. `ainl run` — does it run without error? (supplementary)
  4. "does it do what was asked" — not auto-judged; the raw output is saved
     for human inspection.

### Why "GBNF membership" is the sound detector

`ainl ast` is a **superset** of the GBNF. C1 documented the GBNF ⊊ parser gap,
and the AINL lexer is permissive — it happily tokenizes `:`, `#`, `->`,
`class`, and even llama.cpp's own startup banner as "symbols". So `ainl ast`
accepting a string is **not** evidence the GBNF was applied. The sound
question is the stricter one: **is the string in the GBNF language?**

`scripts/gen-harness/gbnf_fast.py` answers that with a fast recursive-descent
parser that accepts *exactly* the strings the exported GBNF accepts. It was
cross-validated against C1's independent reference Earley parser
(`scripts/gbnf-conformance.py`) — 14/14 agreement on the drift-evidence cases.
(The general Earley is correct but O(n³) in practice because the `ws` star
rule spawns O(n) origins per position; it exceeds 300s on a ~400-char model
output, hence the dedicated fast parser.)

## Headline numbers

| mode | GBNF membership | `ainl ast` parse | `ainl run` |
|---|---|---|---|
| **constrained** | **20/20 (100%)** | 20/20 (100%) | 0/20 (0%) |
| unconstrained | 0/20 (0%) | 20/20 (100%) | 0/20 (0%) |

**Constrained GBNF-membership 100% vs unconstrained 0%.**

## Raw results

| # | prompt | C gbnf | C parse | C run | U gbnf | U parse | U run |
|---|---|:-:|:-:|:-:|:-:|:-:|:-:|
| 0 | Print the sum of 2 and 3. | Y | Y | N | N | Y | N |
| 1 | Define a function sq that squares its argument, then print (sq 12). | Y | Y | N | N | Y | N |
| 2 | Compute and print the 10th fibonacci number (fib 0 = 0, fib 1 = 1). | Y | Y | N | N | Y | N |
| 3 | Print the sum of the list (list 1 2 3 4 5). | Y | Y | N | N | Y | N |
| 4 | Define a function that doubles each element of a list, then print it applied to (list 1 2 3). | Y | Y | N | N | Y | N |
| 5 | Print the string "hello, world". | Y | Y | N | N | Y | N |
| 6 | Print the length of the list (list 1 2 3 4 5). | Y | Y | N | N | Y | N |
| 7 | Print the first element of the list (list 10 20 30). | Y | Y | N | N | Y | N |
| 8 | Define a function fact that computes the factorial of n, then print (fact 5). | Y | Y | N | N | Y | N |
| 9 | Print true if the number 4 is even, else print false. | Y | Y | N | N | Y | N |
| 10 | Define a function max2 that returns the larger of two numbers, then print (max2 7 9). | Y | Y | N | N | Y | N |
| 11 | Print the product of the list (list 1 2 3 4). | Y | Y | N | N | Y | N |
| 12 | Define a function that reverses a list, then print it applied to (list 1 2 3). | Y | Y | N | N | Y | N |
| 13 | Print the sum of the first five natural numbers (1 + 2 + 3 + 4 + 5). | Y | Y | N | N | Y | N |
| 14 | Define a function that counts how many elements a list has, then print it for (list 1 2 3 4 5 6). | Y | Y | N | N | Y | N |
| 15 | Build a map with key "name" bound to "Ada", then print (get user "name"). | Y | Y | N | N | Y | N |
| 16 | Print the keys of the map (hash "a" 1 "b" 2). | Y | Y | N | N | Y | N |
| 17 | Print the remainder of 10 divided by 3. | Y | Y | N | N | Y | N |
| 18 | Define a function that returns the last element of a list, then print it for (list 1 2 3 4). | Y | Y | N | N | Y | N |
| 19 | Print the number 42. | Y | Y | N | N | Y | N |

`C` = constrained, `U` = unconstrained. Full per-prompt detail (including the
raw generated text) is in
[`scripts/gen-harness/results/results.json`](../scripts/gen-harness/results/results.json);
the flat table is
[`results.csv`](../scripts/gen-harness/results/results.csv).

## Honest interpretation

**The constraint works, and it is the only thing that separates the two modes.**
Every constrained output is a member of the GBNF language; none of the
unconstrained outputs are. The unconstrained outputs are free Python — full of
`:` and `#`, which the GBNF's `sym-char` does not allow outside a string — so
they are not AINL at all. The `ainl ast` column is 100% in *both* modes, which
is exactly why it is not the sound detector: the parser is a superset of the
GBNF, so it accepts the Python too. GBNF membership is the check that actually
proves the decoder was constrained.

**The 0.5B model is too weak for semantics.** All 20 constrained outputs are
byte-identical (a single degenerate LeetCode-flavored template), and all 20
unconstrained outputs are byte-identical too. With `--temp 0` the model is
**prompt-insensitive** — it emits the same template regardless of the task. The
template is valid AINL *syntax* (it parses), but it does not *run*
(`runtime error: unbound symbol '/leetcode'`) and it does not do what was asked.

**So the honest result is:** grammar-constrained decoding reliably produces
*syntactically valid* AINL — the thesis, demonstrated. But a 0.5B model cannot
be relied on to produce *semantically correct* AINL. The decoder guarantees the
syntax; the model's capability determines the semantics. A larger model (e.g.
Llama-3.2-1B or a 3B/7B) is the obvious next experiment.

## Reproduce

```sh
# 1. build the ainl binary
cargo build --release

# 2. get a small GGUF (Qwen2.5-0.5B-Instruct Q4_K_M) onto disk, e.g.:
#    huggingface-cli download Qwen/Qwen2.5-0.5B-Instruct-GGUF \
#        qwen2.5-0.5b-instruct-q4_k_m.gguf

# 3. run the harness (llama-cli on PATH; ~15-17 min for 20 prompts)
python3 scripts/gen-harness/run_generation.py \
    --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

Dependencies and flags are documented in
[`scripts/gen-harness/README.md`](../scripts/gen-harness/README.md). The
harness is intentionally **not** in CI (the model download is heavy); it is
committed and runnable.

## Prompt list

The 20 prompts (one per line) are in
[`scripts/gen-harness/prompts.txt`](../scripts/gen-harness/prompts.txt):

1. Print the sum of 2 and 3.
2. Define a function sq that squares its argument, then print (sq 12).
3. Compute and print the 10th fibonacci number (fib 0 = 0, fib 1 = 1).
4. Print the sum of the list (list 1 2 3 4 5).
5. Define a function that doubles each element of a list, then print it applied to (list 1 2 3).
6. Print the string "hello, world".
7. Print the length of the list (list 1 2 3 4 5).
8. Print the first element of the list (list 10 20 30).
9. Define a function fact that computes the factorial of n, then print (fact 5).
10. Print true if the number 4 is even, else print false.
11. Define a function max2 that returns the larger of two numbers, then print (max2 7 9).
12. Print the product of the list (list 1 2 3 4).
13. Define a function that reverses a list, then print it applied to (list 1 2 3).
14. Print the sum of the first five natural numbers (1 + 2 + 3 + 4 + 5).
15. Define a function that counts how many elements a list has, then print it for (list 1 2 3 4 5 6).
16. Build a map with key "name" bound to "Ada", then print (get user "name").
17. Print the keys of the map (hash "a" 1 "b" 2).
18. Print the remainder of 10 divided by 3.
19. Define a function that returns the last element of a list, then print it for (list 1 2 3 4).
20. Print the number 42.
