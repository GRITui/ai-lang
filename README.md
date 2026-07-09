# AI-Native Lang (AINL)

A high-density, strictly-semantic programming language optimized for **Large Language Model context windows** — not human readability. Plus a multi-LLM **Prompt Auditor** that compiles vague natural language into perfect AINL schemas.

> Humans read English. Machines read tokens. AINL is written in the second dialect.

## Why

Frontier models waste context on syntactic sugar, whitespace, and ambiguous phrasing. AINL is a **low-entropy, uniform-grammar** language that:

- maximizes semantic density per token (S-expression core, no non-functional whitespace),
- is trivial to generate under **grammar-constrained decoding** (a tiny, regular CFG),
- maps **bidirectionally** to human-readable languages via AST + source maps, so version control and debugging stay human-friendly,
- ships as a **zero-dependency static binary** that installs anywhere.

## Repo layout

```
ai-native-lang/
├── docs/
│   ├── MASTER_PLAN.md    # full two-phase project plan
│   ├── SYNTAX.md         # the AI-ingestion grammar guide (feed this to any model)
│   └── ARCHITECTURE.md   # how the pieces fit together
├── crates/
│   ├── ainl-core/        # lexer + parser + AST + evaluator (Rust, zero deps)
│   └── ainl-cli/         # the `ainl` binary: run / repl / fmt / ast
├── examples/             # sample .ainl programs
└── Cargo.toml            # Rust workspace
```

## Status

**Phase 1 — the language — is complete.** AINL is implemented in Rust with zero external dependencies and:

- **runs** — tree-walking interpreter (`run`/`eval`/`repl`),
- **serializes** its AST to stable JSON with source-map spans (`ast --json`),
- **exports** a constrained-decoding grammar (`grammar`, GBNF/EBNF),
- **transpiles** byte-equivalently to **Python, JavaScript, and Ruby** (`transpile --to`),
- **ships** as a zero-dependency static binary (`scripts/build-release.sh`, see [docs/RELEASE.md](docs/RELEASE.md)).

**Phase 2 — the multi-SLM Prompt Auditor — has started.** The `ainl-auditor` crate runs a five-role pipeline (Orchestrator → Planner → Auditor → Code Engine → Generalist) that compiles vague natural language into a validated AINL schema. The Auditor stage emits AINL under the exported grammar and validates it with the real parser. A deterministic mock backend runs the whole flow offline today; an HTTP backend targets local Ollama/llama.cpp servers.

```sh
# compile a request into an audited, grammar-valid AINL prompt (offline mock)
./target/release/ainl audit "sum all numbers in a list" --stages
```

See [docs/MASTER_PLAN.md](docs/MASTER_PLAN.md) for the full roadmap and [docs/SYNTAX.md](docs/SYNTAX.md) for the grammar.

## Quick start

```sh
# build (needs the Rust toolchain: https://rustup.rs)
cargo build --release

# run a program
./target/release/ainl run examples/hello.ainl

# start a REPL
./target/release/ainl repl

# inspect the parsed AST (useful for tooling / source maps)
./target/release/ainl ast examples/fib.ainl

# emit the AST as stable JSON with source-map loc (span + line/col per node)
./target/release/ainl ast examples/fib.ainl --json

# project AINL into runnable, readable Python (bidirectional interop, §1.4)
./target/release/ainl transpile examples/fib.ainl --to python
```

## The language in 10 seconds

```lisp
(def sq (fn (x) (* x x)))
(print (sq 12))            ; 144

(def fib (fn (n)
  (if (< n 2) n
    (+ (fib (- n 1)) (fib (- n 2))))))
(print (fib 20))          ; 6765
```

Uniform `(op arg...)` structure, short keywords, no statement terminators, no significant whitespace. Every program is a single unambiguous parse tree.

## License

TBD.
