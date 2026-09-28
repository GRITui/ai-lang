# AINL constrained-decoding proof harness

Two harnesses live here, sharing the same detectors and the same three metrics:

- **`run_generation.py`** — local `llama-cli` (Qwen2.5-0.5B). The original run.
- **`run_gateway.py`** — a remote OpenAI-compatible gateway (Qwen3.8-27B-FP8 on
  vLLM). The current Stage 3.4 result: **9/10 correct**.

Shared scoring rules, defined once so the two paths cannot disagree:

| check | detector | meaning |
|---|---|---|
| **GBNF membership** | `gbnf_fast.py` (sound, O(n)) | is the output *in the language of the exported GBNF*? — the decisive test |
| **valid prefix** | `gbnf_prefix.py` | how much of a *truncated* output is a real program (an artifact, not a failure) |
| **runs** | `ainl run` | exits 0, no runtime error |
| **correct** | stdout comparison | runs AND stdout equals the expected value in the suite |

"Does it *do* what was asked?" is only auto-judged where the expected stdout is
known in advance (`suite_checkable.json`); raw output is always saved for human
inspection.

## Running against a remote gateway

```sh
export HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY=...   # never commit a key
python3 scripts/gen-harness/build_grounded_suite.py   # syntax-grounded prompts
python3 scripts/gen-harness/run_gateway.py \
    --suite scripts/gen-harness/suite_checkable_grounded.json \
    --out scripts/gen-harness/results-gateway-qwen3.8-27b
```

`--resume` reuses generations already in `results.jsonl`; `--rescore-only`
re-scores the ledger without calling the network at all. A remote gateway will
eventually time out (this one returns Cloudflare 524s), and without a ledger a
long run is lost whole.

`score_from_disk.py` re-derives the tables from the saved `.ainl` files, which
is what you want if a run died before writing its summary.

### Three gateway behaviours worth knowing

1. **Send a browser User-Agent.** The default `Python-urllib` UA is blocked by
   Cloudflare (Error 1010) *before* auth — indistinguishable from a bad key.
2. **The constraint parameter is `structured_outputs.grammar`.** On this vLLM
   build `guided_grammar` returns HTTP 200 and is silently ignored. Verify any
   constraint with an impossible grammar (`root ::= "Z"`): a live one returns
   exactly `Z`, an ignored one returns whatever the prompt asked for.
3. **It is a reasoning model.** It burns leading tokens on `reasoning_content`
   and returns `content: null` with `finish_reason: length` when the budget is
   too small. Pass `chat_template_kwargs: {"enable_thinking": false}`.

The 0.5B local run below used `llama-cli` on PATH. The original 20-prompt
results are in `results/`.

## Why GBNF membership is the sound detector

`ainl ast` is a **superset** of the GBNF (C1 documented the GBNF ⊊ parser gap,
and the lexer is permissive — it tokenizes `:`, `#`, `->`, `class`, even
llama.cpp's banner as "symbols"). So `ainl ast` accepting a string is *not*
evidence the GBNF was applied. The sound question is the stricter one: **is the
string a member of the GBNF language?**

`gbnf_fast.py` answers that with a fast recursive-descent parser that accepts
*exactly* the strings the exported GBNF accepts. It was cross-validated against
C1's independent reference Earley parser (`scripts/gbnf-conformance.py`) — 14/14
agreement on the drift-evidence cases. (The general Earley is correct but
O(n³) in practice because the `ws` star rule spawns O(n) origins per position;
it exceeds 300s on a ~400-char model output, hence the dedicated fast parser.)

## Dependencies

- **llama.cpp** `llama-cli` on `PATH` (tested on 0.5.0, build 11146). Note:
  0.5.0 needs `-st`/`--single-turn` (otherwise it drops into an interactive
  REPL and loops forever) and the long form `--chat-template qwen`.
- **A small GGUF model.** The harness was run with
  `Qwen2.5-0.5B-Instruct` (Q4_K_M). Pass any model via `--model`.
- **A built `ainl` binary** — `cargo build --release` (defaults to
  `target/release/ainl`, override with `--ainl`).
- **Python 3** (stdlib only).

## Run

```sh
cd <ai-lang checkout>
python3 scripts/gen-harness/run_generation.py \
    --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

Useful flags: `--limit N` (first N prompts only), `--max-tokens 256`,
`--threads 8`, `--out <dir>`. With 20 prompts × 2 modes × ~25s each, a full run
takes ~15–17 min on an 8-core Apple Silicon CPU.

## Results (Qwen2.5-0.5B-Instruct Q4_K_M, 20 prompts, `--temp 0`)

| mode | GBNF membership | `ainl ast` parse | `ainl run` |
|---|---|---|---|
| **constrained** | **20/20 (100%)** | 20/20 (100%) | 0/20 (0%) |
| unconstrained | 0/20 (0%) | 20/20 (100%) | 0/20 (0%) |

**Headline: constrained GBNF-membership 100% vs unconstrained 0%.**

### Interpretation

- **The constraint works, and it is the only thing that separates the two
  modes.** Every constrained output is a member of the GBNF language; none of
  the unconstrained outputs are (they are free Python with `:` and `#`, which
  the GBNF's `sym-char` does not allow outside strings). The `ainl ast` column
  is 100% for *both* modes, which is exactly why it is not the sound detector —
  the parser is a superset of the GBNF.
- **The 0.5B model is too weak for semantics.** All 20 constrained outputs are
  byte-identical (a single degenerate LeetCode-flavored template), and all 20
  unconstrained outputs are byte-identical too. With `--temp 0` the model is
  **prompt-insensitive** — it emits the same template regardless of the task.
  The template is valid AINL *syntax* (it parses) but does not *run*
  (`runtime error: unbound symbol '/leetcode'`) and does not do what was asked.
- **So the honest result is:** grammar-constrained decoding reliably produces
  *syntactically valid* AINL (the thesis), but a 0.5B model cannot be relied on
  to produce *semantically correct* AINL. The decoder guarantees the syntax;
  the model's capability determines the semantics. A larger model is the
  obvious next experiment (tracked in the README / MASTER_PLAN).

## Files

- `run_generation.py` — the harness (run this).
- `gbnf_fast.py` — the sound, fast GBNF-membership parser (self-test:
  `python3 gbnf_fast.py`).
- `prompts.txt` — the 20 task prompts.
- `results/` — committed outputs from the reference run:
  `results.json` (full per-prompt detail incl. raw text), `results.csv`
  (flat table), and the raw `.ainl` generations.
