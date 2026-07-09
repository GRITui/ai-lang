# Token-density benchmark — results & honest analysis

**Run it:** `python3 bench/bench.py` (needs `tiktoken`).

The project's founding claim is that AINL is *"high-density… optimized for LLM
context windows… maximize token efficiency."* This benchmark tests that claim
directly, and **the claim does not hold as the language is currently designed.**

## Method

For each example program, we count the tokens of the **AINL source** vs.
**idiomatic, hand-written equivalents** in Python / JavaScript / Ruby — the code
a competent developer would actually write (not the mechanical transpiler
output, which carries a runtime shim and would unfairly favor AINL). Every
equivalent is verified to produce byte-identical output to the AINL interpreter.
Tokenization uses OpenAI's real tokenizers via `tiktoken`.

## Results (source tokens; lower is better)

### GPT-4o (o200k_base)

| example | AINL | Python | JavaScript | Ruby |
|---------|-----:|-------:|-----------:|-----:|
| hello   | 127  | 61 (+108%) | 80 (+59%) | 68 (+87%) |
| fib     | 202  | 97 (+108%) | 120 (+68%) | 101 (+100%) |
| **TOTAL** | **329** | **158 (+108%)** | **200 (+64%)** | **169 (+95%)** |

_(% = AINL relative to that language; positive means AINL uses **more** tokens.)_

GPT-4 (`cl100k_base`) gives essentially the same picture (AINL +108% vs Python,
+64% vs JS, +100% vs Ruby).

## Interpretation — the uncomfortable truth

**AINL is currently ~2× *less* token-efficient than idiomatic Python, and worse
than JS and Ruby too.** The S-expression surface syntax is the cause: parentheses
are structural but each pair costs tokens, and explicit `def`/`fn`/`if`/`let`
keywords add more. The tokens saved by dropping whitespace and sugar do not come
close to offsetting the delimiter overhead.

So the headline premise — "token-dense, optimized for LLM context" — is **not
supported by the data** for the current design.

## What *is* still true and valuable

The benchmark disproves *token minimalism*, not the whole project. AINL's real,
measured strengths are:

- **A tiny, regular grammar** → trivially expressible as GBNF, so a small model
  can be *forced* to emit only valid programs (grammar-constrained decoding).
  This is about **reliability of generation**, not token count.
- **Uniform, unambiguous structure** → one parse tree, easy to validate, easy to
  map to/from other languages (verified byte-equal transpile to Python/JS/Ruby).
- **Zero-dependency, portable runtime.**

These are genuine advantages for the Phase 2 auditor use case. But they are a
*different* value proposition than "fewer tokens," and the docs should say so.

## Paths to actually deliver density (future work)

If token efficiency is to remain a goal, the surface syntax must change — this is
design work, not polish:

1. **Parenless / layout-based surface** (significant-indentation or operator
   application without full parens), lowering the parser back to the same AST.
2. **A compact bytecode / opcode form** (the master plan's "stack-based bytecode"
   option) as the on-the-wire representation, with S-expressions as a debug view.
3. **Shorter primitives** and implicit application to cut keyword/delimiter cost.

Each needs its own before/after run of this benchmark to prove it helps.
