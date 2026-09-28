# Constrained generation

AINL's core thesis is that a grammar-constrained decoder can be forced to emit
only syntactically valid AINL. This page records the experiments, in order:

- **[0.5B local](#the-05b-local-run)** — the first proof. The constraint works
  (100% GBNF-valid constrained vs 0% unconstrained), but the model is
  degenerate: 0/20 outputs ran, and all 20 were byte-identical.
- **[0.5B re-run (2026-09-28)](#c2-re-run-2026-09-28)** — the same 20 prompts
  re-run against the current tree. Byte-identical to the original; kept so
  the headline numbers are a current measurement rather than a quotation.
- **[1B local](#the-larger-model-run-llama-32-1b)** — Stage 3.4, first attempt.
  Still degenerate: **0/12** correct.
- **[27B remote](#stage-34-the-larger-model-run--qwen38-27b-fp8-gateway)** — the
  current result. A larger model **does** produce correct programs:
  **9/10**. Verdict: **(a) correct**.
- **[`ainl gen`](#ainl-gen--the-product-command)** — the same pipeline as a
  shipped command: constrained generation, GBNF membership, a bounded repair
  loop, and an interpreter or AOT target.

## Verdict summary

| run | model | GBNF-valid | runs | **correct** |
|---|---|---|---|---|
| [0.5B local](#the-05b-local-run) | Qwen2.5-0.5B | 20/20 | 0/20 | n/a (not auto-judged) |
| [0.5B re-run](#c2-re-run-2026-09-28) | Qwen2.5-0.5B | 20/20 | 0/20 | n/a (byte-identical to the above) |
| [1B local](#the-larger-model-run-llama-32-1b) | Llama-3.2-1B | 10/12 | 1/12 | **0/12** |
| [27B gateway](#stage-34-the-larger-model-run--qwen38-27b-fp8-gateway) | Qwen3.8-27B-FP8 | 10/10 | 10/10 | **9/10** |

---

## Stage 3.4: the larger-model run — Qwen3.8-27B-FP8 (gateway)

The card for this stage assumed an OpenAI-compatible API could only support the
**unconstrained** half, because "a remote API can't load our .gbnf file". That
assumption is wrong for a **vLLM** backend: it serves GBNF structured outputs
over the normal API. Both halves therefore run here, and the constrained arm is
a real constrained decode, not a prompt instruction.

### Verdict: (a) correct

The 27B model writes AINL that runs and prints the right answer on 9 of 10
prompts. The 1B model managed 0 of 12. The 27B is the first model in this
project to produce semantically correct AINL.

### Method

- **Model:** `qwen3.8-27b-fp8` (Qwen3.8-27B-FP8, a reasoning model) served by
  vLLM behind an OpenAI-compatible gateway.
- **Constraint:** the same `ainl grammar --gbnf` output used everywhere else —
  nothing hand-written.
- **Prompts:** 12 checkable-answer tasks (`suite_checkable.json`); each has a
  known expected stdout, so "correct" is mechanical, not judged by eye.
- **Modes:** **constrained** (GBNF applied) and **unconstrained**, same model,
  same prompts, `temperature 0`.
- **Metrics:** GBNF membership (the sound detector), `ainl run` exits 0, and
  stdout exactly equals the expected value.

### Three gateway quirks (each one silently corrupts results)

1. **The User-Agent must be overridden.** The default `Python-urllib` UA is
   rejected by Cloudflare with Error 1010 *before* authentication runs. It
   presents exactly like an invalid key, and it is what made an earlier attempt
   conclude the gateway key was stale. A browser UA gets HTTP 200.
2. **`guided_grammar` is silently ignored.** The LiteLLM-style parameter
   returns HTTP 200 and changes nothing — worse than an error, because the run
   looks fine while measuring nothing. The parameter that actually constrains
   is **`structured_outputs.grammar`**. Proven with an impossible grammar
   (`root ::= "Z"`): asked to write an essay about the Roman empire, a live
   constraint returns exactly `Z`; an ignored one returns prose.
3. **It is a reasoning model.** It spends the opening tokens on
   `reasoning_content` and returns `content: null` with
   `finish_reason: length` if the budget is too small — which looks like a
   broken key. Thinking is disabled explicitly via
   `chat_template_kwargs.enable_thinking`.
4. **It enters 524 storms.** Cloudflare's read-timeout at ~125s, independent of
   `max_tokens`, so a heavier spec can fail on any attempt. `ainl gen` reports
   the HTTP status rather than retrying blindly; give it `--timeout` above the
   gateway's own, and expect occasional hard failures under load.

---

## `ainl gen` — the product command

The experiments above needed a harness (`scripts/gen-harness/run_gateway.py`).
`ainl gen` is the same pipeline as a command anyone can run:

```
spec → constrained generation → GBNF membership → parse → compile → run → output
```

A failure at validate, compile, or run is turned into model-readable feedback —
source position and the likely fix, never a Rust panic — and fed back for a
bounded repair loop (`--attempts`, default 3).

```sh
export AINL_GEN_API_KEY_ENV=MY_API_KEY_VAR
export AINL_GEN_ENDPOINT=https://your-gateway.example
export AINL_GEN_MODEL=your-model

ainl gen "print the sum of 2 and 3" --run
ainl gen -f spec.ainl --aot -o myprog          # a standalone binary
echo "print 2 and 3" | ainl gen --run           # spec on stdin
```

`--extra-json '{"chat_template_kwargs":{"enable_thinking":false}}'` passes
vendor-specific request members. The key is read from the environment and
handed to `curl` on **stdin**, so it appears in neither `ps` output nor shell
history; the command takes no TLS stack of its own (see
[HTTP_TLS.md](HTTP_TLS.md)).

### The few-shot corpus

The prompt is not just the language reference. `examples/few-shot.txt` holds
complete, working AINL programs, and `--examples` controls how many of them go
into the request:

```sh
ainl gen "count the words in a file"      # 2 examples (the default)
ainl gen --examples all "…"               # the whole corpus
ainl gen --no-examples "…"                # the language reference alone
```

The default is 2, which is a measured choice rather than a round number. Zero
leaves the model with the three tiny snippets in the language reference —
enough for a small program, but too small to contain the mistakes models
actually make. All crowds out the task, and a repair loop re-sends the prompt
on every attempt. Two puts a real program — a loop, a map, a recursion — in
front of the model while leaving the request dominated by what was asked.

**Selection is a count, not a keyword.** Matching examples against the spec
would need a matcher with no idea what the request is about, and a wrong guess
costs more than it saves: a model shown a file-I/O example when it asked for
arithmetic wastes context and may copy the wrong shape. So the corpus is
ordered and documented in [../examples/README.md](../examples/README.md), and
the caller picks the number.

The corpus is **generated** from the example programs by
`scripts/build-few-shot.sh`, and CI fails if the committed copy is out of date.
A hand-maintained prompt drifts from the code the moment a builtin is added,
and nothing notices until a generation quietly starts failing — a stale
few-shot prompt teaches syntax that no longer parses, which is the one failure
this corpus cannot have.

The file is read relative to the **working directory**, so run from an
ai-lang checkout. Without it the command still works: it sends the language
reference alone, which is what it did before the corpus existed, and says so
rather than failing.

### Two independent checks, because a backend can lie

Neither check is trusted to the other:

1. **The probe.** Before the real call, an impossible grammar (`root ::= "Z"`)
   is sent. A backend honouring constraints returns exactly `Z`; one ignoring
   them returns prose. This is the only way to catch the `guided_grammar`
   failure above, and it runs by default because that failure is silent.
2. **Local membership.** `crates/ainl-cli/src/gbnf.rs` decides membership with
   the Rust GBNF matcher and cross-checks it against the shipped Python
   detector over 813 cases — every committed generation, hand-picked edges, and
   800 fuzz inputs — with zero disagreements:

   ```sh
   python3 scripts/check-gbnf-rust.py
   ```

The end-of-run report never claims a check that did not run. `--no-probe`
says the enforcement is *unverified*; `--grammar-field none` says the run was
*not grammar-constrained*; a non-member program under a sent grammar is called
out as a constraint the backend ignored. `scripts/check-gen-repair.py` pins all
three against a local stub, with no network and no API key.

### Grounding: the model does not know AINL

Asked for "the sum of 2 and 3" with no description of the language, the model
emits `PRINT 2 + 3` — a valid S-expression that is **not** AINL, and which fails
with `runtime error: unbound symbol 'PRINT'`. This is not a small-model
symptom; the GBNF constrains *shape*, not *vocabulary*, so it accepts the wrong
language just as happily as the right one.

`build_grounded_suite.py` therefore primes both arms equally with the real
reference (special forms, builtins, two worked examples, drawn from
`docs/SYNTAX.md`, which is written for model ingestion). Grounding is applied
**identically to both arms**, so it cannot bias the constrained/unconstrained
comparison — that comparison remains the headline. It only stops the
experiment from being a test of whether a large model can guess an unknown
language.

With grounding, `fib-10` becomes:

```lisp
(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))
(print (fib 10))          ; prints 55
```

### Results — 10 of 12 prompts

| # | id | expected | C gbnf | C run | **C correct** | U gbnf | U run | U correct |
|---|---|---|:-:|:-:|:-:|:-:|:-:|:-:|
| 0 | sum-2-3 | 5 | Y | Y | **Y** | Y | Y | Y |
| 1 | print-42 | 42 | Y | Y | **Y** | Y | Y | Y |
| 2 | hello | hello, world | Y | Y | **Y** | Y | Y | Y |
| 3 | sq-12 | 144 | Y | Y | **Y** | Y | Y | Y |
| 4 | fib-10 | 55 | Y | Y | **Y** | Y | Y | Y |
| 5 | fact-5 | 120 | Y | Y | **Y** | Y | Y | Y |
| 6 | max2-7-9 | 9 | Y | Y | **Y** | Y | Y | Y |
| 7 | len-5 | 5 | Y | Y | **Y** | Y | Y | Y |
| 8 | first-10 | 10 | Y | Y | **Y** | Y | Y | Y |
| 9 | double-1-2-3 | (2 4 6) | Y | Y | **N** | Y | Y | N |

| mode | GBNF-valid | runs | **correct** | distinct outputs |
|---|---|---|---|---|
| **constrained** | **10/10 (100%)** | 10/10 (100%) | **9/10 (90%)** | 10 |
| unconstrained | 10/10 (100%) | 10/10 (100%) | 9/10 (90%) | 10 |

`map-ada` and `last-1-2-3-4` never completed: the gateway entered a 524
(read-timeout) storm, taking ~80s for a three-token reply. They are recorded as
**not run**, not as failures. No generation was truncated in this run.

### The single miss is a real semantic error

`double-1-2-3` fails because the model treats `list` as a two-argument
constructor:

```lisp
(def double-each (fn (xs) (if (= (len xs) 0) (list) (list (* 2 (first xs)) (double-each (rest xs))))))
(print (double-each (list 1 2 3)))   ; -> (2 (4 (6)))  not (2 4 6)
```

It is syntactically perfect, passes the grammar, runs, and prints a nested
list instead of a flat one. This is exactly the residual error class the
project cares about: **the constraint guarantees syntax, not semantics.** It is
a property of the model, not of the decoder, and no grammar can fix it.

### Constrained vs unconstrained: a tie here, and that is informative

With syntax grounding, both arms score 9/10 and produce **byte-identical**
outputs on all 10 prompts. The constraint is no longer the binding constraint:
the model already emits valid AINL unprompted, so there is nothing left for
the grammar to rescue. The constraint still guarantees 100% GBNF membership, and
it is what makes this safe to deploy — but on this model it is insurance, not
an improvement.

The 0.5B run below is the mirror image: there the constraint was the *only*
thing separating the two modes (100% vs 0%). Constraint value scales inversely
with model capability.

### Reproduce

```sh
cargo build --release
python3 scripts/gen-harness/build_grounded_suite.py
export HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY=...   # never committed
python3 scripts/gen-harness/run_gateway.py \
    --suite scripts/gen-harness/suite_checkable_grounded.json \
    --out scripts/gen-harness/results-gateway-qwen3.8-27b
```

`run_gateway.py` appends every generation to `results.jsonl` as it goes and
supports `--resume` / `--rescore-only`. This is not gold-plating: the first full
suite run lost 14 minutes of work to a 524 on its second-to-last prompt,
because scores were only written at the very end. `score_from_disk.py`
re-scores saved `.ainl` artifacts without touching the network.

---

## C2 re-run (2026-09-28)

The 20-prompt 0.5B harness was re-run against the current tree so the numbers
the pitch quotes are a current measurement, not a quotation from an earlier
commit. Two preconditions were verified before running, so the reproduction is
meaningful:

- **The grammar is unchanged.** `crates/ainl-core/src/grammar.rs` is
  byte-identical to the C2 commit (`sha256 386e2628…`, verified with
  `git show d7a992c:… | shasum`). The constraint under test is the same
  constraint.
- **The decoder is unchanged.** llama-cli `0.5.0 (build 11146)`, the same
  version as the original run.

Result — [`results.csv`](../scripts/gen-harness/results-rerun-2026-09-28/results.csv)
is **byte-identical** to the [original](#the-05b-local-run):

| mode | GBNF membership | `ainl ast` parse | `ainl run` |
|---|---|---|---|
| **constrained** | **20/20 (100%)** | 20/20 (100%) | 0/20 (0%) |
| unconstrained | 0/20 (0%) | 20/20 (100%) | 0/20 (0%) |

The degeneracy reproduces as well: all 20 constrained outputs are a **single
distinct byte-identical** LeetCode-flavored template, and it fails identically:

```console
$ ainl run scripts/gen-harness/results-rerun-2026-09-28/constrained/0.ainl
runtime error: unbound symbol '/leetcode'
```

So the 0.5B result is not a one-off: 100% valid syntax, 0% semantics, on the
current tree and the current grammar. Reproduce with:

```sh
python3 scripts/gen-harness/run_generation.py \
    --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
    --out scripts/gen-harness/results-rerun-2026-09-28
```

---

## The larger-model run (Llama-3.2-1B)

The first Stage 3.4 attempt ran Llama-3.2-1B locally against the same 12
checkable prompts and the same three metrics
(`scripts/gen-harness/run_checkable.py`,
[results](https://github.com/GRITui/ai-lang/tree/main/scripts/gen-harness/results-llama-3.2-1b)).

| mode | GBNF-valid | runs | correct |
|---|---|---|---|
| constrained | 10/12 | 1/12 | **0/12** |
| unconstrained | 0/12 | 1/12 | **0/12** |

**Verdict: (c) still degenerate.** A 1B model cannot write AINL. This run is
the baseline the 27B result above is measured against, and it is why the
experiment moved to a 27B model rather than declaring victory on syntax alone.

## The 0.5B local run

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
