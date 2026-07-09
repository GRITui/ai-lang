# AI-Native Lang (AINL)

[![CI](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml)

A small, uniform programming language built to be **generated and verified by machines**. AINL's entire grammar is a tiny regular CFG, so an LLM can be *constrained* to emit only valid programs — and every program maps losslessly to and from human-readable Python, JavaScript, and Ruby.

> A reliable **generation target** for LLMs: easy to generate under grammar constraints, trivial to validate, and portable to the languages people already use.

## Why

Getting a model to emit correct code in a full language is unreliable — the grammar is huge and ambiguous. AINL inverts that:

- **Tiny, regular grammar** — expressible as GBNF, so grammar-constrained decoding forces syntactically valid output every time.
- **One unambiguous parse tree** — uniform `(op arg...)` structure with byte spans, trivial to validate, analyze, and map.
- **Lossless interop** — the same AST projects to idiomatic Python/JS/Ruby (verified byte-equal), so AINL slots into existing codebases and debugging.
- **Zero-dependency runtime** — installs anywhere as a small static binary.

### A note on token efficiency

An earlier design goal was raw token density. Measured with real tokenizers, that goal is **not** met: AINL currently uses **~2× the tokens of idiomatic Python** (and more than JS/Ruby too) — the S-expression delimiters cost more than the whitespace they remove. See [docs/BENCHMARK.md](docs/BENCHMARK.md) for the numbers. AINL's actual advantage is *reliability and verifiability of machine generation*, not fewer tokens; making the surface syntax genuinely dense is tracked as future work.

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

See [docs/MASTER_PLAN.md](docs/MASTER_PLAN.md) for the roadmap, [docs/SYNTAX.md](docs/SYNTAX.md) for the grammar, and [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) for a hands-on walkthrough.

## Install

Needs the [Rust toolchain](https://rustup.rs) (`rustc`/`cargo`).

```sh
# from a checkout — installs the `ainl` binary to ~/.cargo/bin
cargo install --path crates/ainl-cli

# or straight from GitHub
cargo install --git https://github.com/GRITui/ai-lang ainl-cli

ainl version
ainl eval '(* 6 7)'        # 42
```

Prebuilt binaries are attached to [GitHub releases](https://github.com/GRITui/ai-lang/releases). For a fully-static Linux binary, see [docs/RELEASE.md](docs/RELEASE.md).

## Quick start

```sh
# build from source (needs the Rust toolchain: https://rustup.rs)
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

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this
project shall be dual-licensed as above, without any additional terms.
