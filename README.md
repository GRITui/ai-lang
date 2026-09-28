# AI-Native Lang (AINL)

[![CI](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml)

A small, uniform programming language built to be **generated and verified by machines**. AINL's entire grammar is a tiny regular CFG, so an LLM can be *constrained* to emit only valid programs — and every program maps losslessly to and from human-readable Python, JavaScript, and Ruby.

> A reliable **generation target** for LLMs: easy to generate under grammar constraints, trivial to validate, and portable to the languages people already use.

## Why

Getting a model to emit correct code in a full language is unreliable — the grammar is huge and ambiguous. AINL inverts that:

- **Tiny, regular grammar** — expressible as GBNF, so grammar-constrained decoding forces syntactically valid output every time. Demonstrated: **100% of 20 constrained generations are valid AINL, 0% of the unconstrained baseline** ([docs/GENERATION.md](docs/GENERATION.md)).
- **One unambiguous parse tree** — uniform `(op arg...)` structure with byte spans, trivial to validate, analyze, and map.
- **Lossless interop** — the same AST projects to idiomatic Python/JS/Ruby (verified byte-equal for the example programs), so AINL slots into existing codebases and debugging. This holds for values that stay within `i64`/float-safe range; integer overflow behavior itself diverges by design across targets — see [docs/NUMERIC_MODEL.md](docs/NUMERIC_MODEL.md).
- **Zero-dependency runtime** — installs anywhere as a small static binary.

### A note on token efficiency

An earlier design goal was raw token density. Measured with real tokenizers, that goal is **not** met: AINL currently uses **~2× the tokens of idiomatic Python** (and more than JS/Ruby too) — the S-expression delimiters cost more than the whitespace they remove. See [docs/BENCHMARK.md](docs/BENCHMARK.md) for the numbers. AINL's actual advantage is *reliability and verifiability of machine generation*, not fewer tokens; making the surface syntax genuinely dense is tracked as future work.

## Repo layout

```
ai-native-lang/
├── docs/
│   ├── MASTER_PLAN.md    # full two-phase project plan
│   ├── SYNTAX.md         # the AI-ingestion grammar guide (feed this to any model)
│   ├── ARCHITECTURE.md   # how the pieces fit together
│   ├── PERFORMANCE.md    # execution speed (interpreter) + the 2M step-cap finding
│   └── RELEASE.md        # release pipeline, checksums, portability
├── crates/
│   ├── ainl-core/        # lexer + parser + AST + evaluator (Rust, zero deps)
│   ├── ainl-transpile/   # AINL -> Python / JavaScript / Ruby
│   ├── ainl-cc/          # AINL -> C codegen (AOT backend)
│   └── ainl-cli/         # the `ainl` binary: run / repl / compile / transpile / doctor
├── examples/             # sample .ainl programs
├── scripts/
│   ├── install.sh        # the one-line installer (verifies SHA256 first)
│   └── check-*.sh        # the CI gates, runnable locally
└── Cargo.toml            # Rust workspace
```

## Status

**Phase 1 — the language — is complete.** AINL is implemented in Rust with zero external dependencies and:

- **runs** — tree-walking interpreter (`run`/`eval`/`repl`),
- **serializes** its AST to stable JSON with source-map spans (`ast --json`)
  and **reads it back** (`ast --json-out`), a lossless round trip,
- **exports** a constrained-decoding grammar (`grammar`, GBNF/EBNF),
- **transpiles** byte-equivalently to **Python, JavaScript, and Ruby** (`transpile --to`),
- **ships** as a zero-dependency static binary (`scripts/build-release.sh`, see [docs/RELEASE.md](docs/RELEASE.md)).

See [docs/MASTER_PLAN.md](docs/MASTER_PLAN.md) for the roadmap, [docs/SYNTAX.md](docs/SYNTAX.md) for the grammar, and [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) for a hands-on walkthrough.

## Constrained generation (proof of concept)

[scripts/gen-harness/](scripts/gen-harness/) runs a small local model
(Qwen2.5-0.5B-Instruct Q4_K_M) through a **real GBNF-constrained decoder**
(llama.cpp `llama-cli --grammar-file` with the grammar from `ainl grammar`)
and measures the output against the GBNF language itself.

| mode | GBNF membership | `ainl ast` parse | `ainl run` |
|---|---|---|---|
| **constrained** | **20/20 (100%)** | 20/20 (100%) | 0/20 (0%) |
| unconstrained | 0/20 (0%) | 20/20 (100%) | 0/20 (0%) |

**The constraint works: 100% of constrained outputs are valid AINL, 0% of
unconstrained ones are.** (The `ainl ast` column is 100% in *both* modes — the
parser is a superset of the GBNF, so GBNF membership is the sound check; see
[docs/GENERATION.md](docs/GENERATION.md) for why.)

The honest negative result: a 0.5B model is too weak for *semantics*. All 20
constrained outputs are byte-identical degenerate templates — valid AINL
syntax, but they don't run and don't do what was asked. The decoder guarantees
the syntax; the model's capability determines the semantics. A larger model is
the next experiment. Full method, raw results, and the reproducible harness:
[docs/GENERATION.md](docs/GENERATION.md) and
[scripts/gen-harness/README.md](scripts/gen-harness/README.md).

## Install

One line. It picks the right binary for your platform, verifies its SHA256
against the release's `SHA256SUMS`, and installs to `~/.local/bin` (no root):

```sh
curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
```

Then confirm the install works — `doctor` runs the interpreter, the stdlib,
the grammar export, all three transpilers, and the AOT code generator, and
exits non-zero if anything fails:

```sh
ainl --version     # ainl 0.3.0 aarch64-apple-darwin (a46e7ad00b4d)
ainl doctor        # 7 checks; exit 0 only if all pass
ainl eval '(* 6 7)'   # 42
```

Pin a version with `AINL_VERSION=0.3.0`, or install to a specific directory
with `AINL_BIN_DIR=…`. The installer **refuses to install anything it cannot
verify**: a release with no `SHA256SUMS`, or an asset whose checksum does not
match, is an error rather than a silent install.

<details>
<summary>Other install paths</summary>

Prebuilt binaries for Linux x86_64 (fully static, musl) and macOS aarch64 are
attached to [GitHub releases](https://github.com/GRITui/ai-lang/releases) with
their checksums:

```sh
curl -LO https://github.com/GRITui/ai-lang/releases/download/v0.3.0/ainl-v0.3.0-x86_64-unknown-linux-musl.tar.gz
curl -LO https://github.com/GRITui/ai-lang/releases/download/v0.3.0/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar xzf ainl-v0.3.0-x86_64-unknown-linux-musl.tar.gz
./ainl-v0.3.0-x86_64-unknown-linux-musl/ainl eval '(* 6 7)'   # 42
```

Or build from source (needs the [Rust toolchain](https://rustup.rs)):

```sh
cargo install --git https://github.com/GRITui/ai-lang ainl-cli
# or, from a checkout:
cargo build --release && ./target/release/ainl doctor
```

`ainl compile` (AOT to a standalone C binary) additionally needs a host C
compiler. Without one, `ainl doctor` reports it as **SKIP**, not a failure —
everything else in the language works without `cc`.

</details>

See [docs/RELEASE.md](docs/RELEASE.md) for the release pipeline, checksums, and
staticness verification.

## Quick start

From a checkout, after `cargo build --release`:

```sh
# run a program
./target/release/ainl run examples/hello.ainl

# start a REPL
./target/release/ainl repl

# AOT-compile a program to a standalone native binary (needs cc)
./target/release/ainl compile examples/fib.ainl -o fib && ./fib

# inspect the parsed AST (useful for tooling / source maps)
./target/release/ainl ast examples/fib.ainl

# emit the AST as stable JSON with source-map loc (span + line/col per node)
./target/release/ainl ast examples/fib.ainl --json

# read a JSON AST back into the AST (inverse of --json; round-trip check)
./target/release/ainl ast examples/fib.ainl --json-out fib.json

# project AINL into runnable, readable Python (bidirectional interop, §1.4)
./target/release/ainl transpile examples/fib.ainl --to python

# the grammar a constrained decoder can be given
./target/release/ainl grammar --gbnf
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
