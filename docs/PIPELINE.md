# The pipeline: generate → validate → compile → run

This is the pitch in executable form. A model generates AINL under a grammar
constraint; you check it in linear time; you compile it; you run it. Every
command and every output below was run on the commit this file landed in — the
transcript is not reconstructed.

**What the grammar guarantees and what it does not.** The constraint makes
output *syntactically* valid. It does not make output *semantically* correct.
Those are two different guarantees and the difference is the entire point of
the pipeline: syntax is what a decoder can promise you, so a decoder promises
it and validation checks the promise cheaply. Semantics is the model's job, and
no grammar can do it for the model. See
[the honest verdict](GENERATION.md#the-single-miss-is-a-real-semantic-error).

## The four stages

| stage | what it does | cost | fails loudly? |
|---|---|---|---|
| **generate** | a decoder emits AINL under `ainl grammar --gbnf` | one LLM call | no — the decoder cannot emit invalid syntax by construction |
| **validate** | is the output in the GBNF language? | **O(n), ~0.6 µs/byte** | yes — this is the check |
| **compile** | AINL → a single self-contained C file → `cc` → native binary | ~0.7 s | yes, `cc` errors |
| **run** | execute | as fast as the program | yes, non-zero exit |

Validate is the interesting one: it is a *sound* check, and it is the only
stage cheap enough to run on every single generation.

## 0. Setup

```sh
git clone https://github.com/GRITui/ai-lang.git
cd ai-lang
cargo build --release          # → target/release/ainl
./target/release/ainl doctor   # 7 checks; exits 0 only if all pass
```

Real output from this repo at the commit this file landed in:

```
version    ok    ainl 0.2.0 aarch64-apple-darwin (tree state unknown) (f5ddf5108b53)
cc         ok    cc 21.0.0
grammar    ok    GBNF ok (10 rules, 561 bytes)
eval       ok    (def sq (fn (x) (* x x))) (+ (sq 12) (len (join (split "a,b,c" ",") "-"))) => 149
run        ok    (fib 10) => 55
transpile  ok    python 162 bytes, js 169 bytes, ruby 168 bytes
aot        ok    generated C compiles (59340 bytes)
all checks passed in 338ms
```

## 1. Generate — the grammar is the product

The grammar is a **first-class export**, not a build artifact. It is 10 rules,
561 bytes, and it is what you hand to any constrained decoder:

```sh
./target/release/ainl grammar --gbnf
```

```gbnf
# AINL v0.1 — GBNF grammar for constrained decoding.
# Every string this grammar accepts is a syntactically valid AINL program.
root    ::= ws form (ws form)* ws
form    ::= list | atom
list    ::= "(" ws (form ws)* ")"
atom    ::= (string | number | symbol) ws
string  ::= "\"" ([^"\\] | "\\" ["\\/nrt] )* "\""
number  ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?
symbol  ::= sym-char+
sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!" | "?" | "." | "_" | "&"
ws      ::= ( [ \t\n\r] | comment )*
comment ::= ";" [^\n]* "\n"
```

That is the whole contract. Any deployment that speaks GBNF — **llama.cpp,
vLLM, llama-cpp-python, LM Studio, Ollama's structured outputs** — can be given
this file. There is nothing AINL-specific about consuming it.

### 1a. Local: llama.cpp

```sh
llama-cli -m /path/to/model.gguf -f prompt.txt \
    --grammar-file <(./target/release/ainl grammar --gbnf) \
    --temp 0 -n 1200 -st
```

`--grammar-file` is llama.cpp's native constraint, so this is the most direct
possible form of the proof. The full 20-prompt harness (both arms, both
modes) is:

```sh
python3 scripts/gen-harness/run_generation.py \
    --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

### 1b. Remote: any OpenAI-compatible gateway

Because the grammar is portable, generation is not tied to one runtime. The
harness that produced the [9/10 result](GENERATION.md) runs against a vLLM
backend:

```sh
export HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY=...   # never committed
python3 scripts/gen-harness/run_gateway.py \
    --suite scripts/gen-harness/suite_checkable_grounded.json \
    --out scripts/gen-harness/results-gateway-qwen3.8-27b
```

Two gateway facts are encoded in that script because each one silently
corrupts results if missed — both are written up in
[docs/GENERATION.md](GENERATION.md#three-gateway-quirks-each-one-silently-corrupts-results):

- constrained decoding is `structured_outputs.grammar`; the commonly-cited
  `guided_grammar` is **silently ignored** on vLLM (HTTP 200, no constraint);
- the default `Python-urllib` User-Agent is rejected at the Cloudflare edge
  with error 1010, which looks exactly like an invalid API key.

## 2. Validate — O(n), and it is the sound check

This is the stage that earns its keep. `ainl ast` is **not** the right check:
the AINL lexer is permissive and the parser is a **superset** of the GBNF, so
it happily accepts free-form Python as "symbols". A check that passes on
invalid input measures nothing.

The sound question is the strict one — *is this string in the GBNF language?* —
and `scripts/gen-harness/gbnf_fast.py` answers exactly that. It accepts
precisely the strings the exported GBNF accepts, and it was cross-validated
against an independent Earley reference parser
([`scripts/gbnf-conformance.py`](../scripts/gbnf-conformance.py)) on 14/14
drift-evidence cases.

```sh
python3 scripts/gen-harness/validate_gbnf.py fib-10.ainl
```

```console
$ python3 scripts/gen-harness/validate_gbnf.py fib-10.ainl
True
$ python3 scripts/gen-harness/validate_gbnf.py some_python.py
False
```

That second line is the negative control, and it is the whole argument for this
being the sound check: free-form Python is **rejected**. `ainl ast` would have
accepted it.

Measured cost, single core, on the `fib-10` program scaled up:

| bytes | validate | µs/byte |
|---:|---:|---:|
| 82 | 47.3 µs | 0.576 |
| 410 | 240.2 µs | 0.586 |
| 2 050 | 1 209.8 µs | 0.590 |
| 10 250 | 6 036.4 µs | 0.589 |
| 51 250 | 30 969.0 µs | 0.604 |

Per-byte cost is flat across a 625× size range — **linear, not quadratic**.
At ~0.6 µs/byte, validating a 10 KB generation costs about 6 ms. There is no
reason not to run this on everything.

## 3. Compile — AINL → C → native

```sh
./target/release/ainl compile fib-10.ainl -o fib10
```

The micro-runtime (value model, refcounting, cons cells, interning, scopes,
closures, 46 builtins, step counter) is inlined into a **single self-contained
C file** that links against nothing but libc. Keep the C with `--keep-c`.

```
compiled /tmp/pipeline_demo.ainl -> /tmp/pipeline_demo
```

## 4. Run

```sh
./target/release/ainl run fib-10.ainl     # → 55
./fib10                                  # → 55   (the compiled binary)
```

## The whole pipeline, end to end

The program below is **not hand-written**. It is the literal output of
`results-gateway-qwen3.8-27b/constrained/fib-10.ainl` — a real generation from
Qwen3.8-27B-FP8 under the grammar, scored correct by the harness because its
stdout matched the expected value exactly.

```lisp
(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))
(print (fib 10))
```

Every stage, run for real:

```console
$ python3 scripts/gen-harness/validate_gbnf.py fib-10.ainl
True
$ ainl run fib-10.ainl               # 4. interpret
55
$ ainl compile fib-10.ainl -o fib10  # 3. AOT
compiled fib-10.ainl -> fib10
$ ./fib10
55
```

Note that stage 2 is a **script, not an `ainl` subcommand** — `ainl` has no
`validate` verb. `validate_gbnf.py` is a two-line wrapper around `gbnf_fast.py`,
which is the validator the harness itself scores every single generation with,
so the check you run by hand is the same check the published numbers come
from.

## 5. And the escape hatch: run it somewhere else

Because every program has one unambiguous parse tree with source spans, the
same AST projects to idiomatic Python, JavaScript, and Ruby — byte-equivalently
for the example programs:

```console
$ ainl transpile fib-10.ainl --to python
...
# ainl:1  fib
def fib(n):
    if (n < 2):
        return n
    else:
        return (fib((n - 1)) + fib((n - 2)))
_print(fib(10))
```

So the pipeline is not a dead end for a model that produced valid syntax but
the wrong logic: the output is still *readable, editable, portable* code. That
is the practical payoff of the uniform `(op arg...)` structure — a model that
is wrong about semantics is wrong in a way a human can see and fix, because
the syntax has nowhere to hide.

## Reproducing this document

```sh
cargo build --release
./target/release/ainl doctor
python3 scripts/gen-harness/run_generation.py --model /path/to/model.gguf   # stage 1a
python3 scripts/gen-harness/run_gateway.py --suite …                        # stage 1b
```

`scripts/gen-harness/README.md` documents the harness flags and dependencies.
The model download is heavy, so the harness is committed and runnable but
deliberately **not** in CI.

Related: [GENERATION.md](GENERATION.md) (the experiments and the honest
verdict) · [PERFORMANCE.md](PERFORMANCE.md) (interpreter, VM, and AOT speed) ·
[SYNTAX.md](SYNTAX.md) (the grammar, written for model ingestion).
