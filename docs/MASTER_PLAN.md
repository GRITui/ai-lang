# Master Plan: AI-Native Language

## Build an AI-Native Programming Language

**Core objective (as originally stated):** a high-density, strictly-semantic
language optimized for LLM context windows rather than human readability.

> **Reality check (measured):** the token-density goal is *not* met by the
> current S-expression design — AINL uses ~2× the tokens of idiomatic Python
> (see [BENCHMARK.md](BENCHMARK.md)). The delivered value is instead
> *reliable, grammar-constrained generation* and *lossless interop*. Genuine
> density would require a syntax redesign (see §1.3 note). This plan is kept
> as the original vision; annotations mark where results diverged.

### 1.1 Cross-language framework integration
- Bidirectional interop between AINL and traditional languages.
- AST mapping + source-map projection so the AI's dense operations translate back to human-readable code for version control and debugging.
- Rust core: fast, safe, good crate ecosystem for parsing/codegen.

### 1.2 Universal portability and installation
- Package the runtime as a **zero-dependency static binary**.
- Compile with Rust's `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` targets (and native macOS/Windows) so it installs instantly on any machine, edge device, or container with no dependency chain.

### 1.3 AI-optimized syntax & markdown documentation
- Rigid, low-entropy syntax (S-expression core) — no non-functional whitespace, no syntactic sugar. _(Intended to maximize token efficiency; measured to be ~2× Python's token count instead — see [BENCHMARK.md](BENCHMARK.md). Density remains future work; the low-entropy grammar still pays off for constrained decoding.)_
- A specialized [SYNTAX.md](SYNTAX.md) written **for AI ingestion**: any model can read it once and emit perfectly-formed AINL.
- Grammar is a small regular CFG → directly usable as a GBNF / constrained-decoding grammar.

### 1.4 Cross-programming-language plugins
- Extensible plugin system so AINL apps interoperate with external codebases.
- Rollout order: **Python, JavaScript, Ruby** first; modular architecture lets the community add more.
- Each plugin = an AST transpiler (AINL AST ⇄ target-language AST) + a runtime FFI shim.

## A natural-language front end (separate project)

The original plan had a second phase — a tool to compile natural language into
AINL. That has been built and **spun out into its own repository**:
[GRITui/ainl-auditor](https://github.com/GRITui/ainl-auditor). It consumes this
project (`ainl-core`) as a crate and is developed independently. This plan now
covers the language + toolchain only.

---

## Sequencing (how we actually build it)

| Milestone | Deliverable | Verifies |
|-----------|-------------|----------|
| **M1** ✅ | Working AINL interpreter (lexer→parser→eval), CLI `run`/`repl`/`ast` | Language exists and executes |
| **M2** ✅ | `SYNTAX.md` + GBNF grammar export (`ainl grammar`) | Models can generate valid AINL |
| **M3** ✅ | Stable JSON AST serialization + source-map loc (`ainl ast --json`) | Foundation for interop & tooling |
| **M4** ✅ | Python transpiler plugin (AINL → Python), `ainl transpile` | §1.4 proof of concept; output verified byte-equal to the interpreter |
| **M5** ✅ | musl static-binary release pipeline (`scripts/build-release.sh`, `.cargo/config.toml`, `docs/RELEASE.md`) | §1.2 portability; native binary verified system-only deps (393 KB) |

Plus the §1.4 rollout completed the JavaScript and Ruby transpilers (full 3×3
byte-equal matrix), and **v0.1.0 shipped**.

**Phase 1 is complete.** The AINL language runs, serializes its AST with source
maps, exports a constrained-decoding grammar, transpiles byte-equivalently to
Python/JavaScript/Ruby, and ships as a zero-dependency binary. See
[BACKLOG.md](BACKLOG.md) for what's next.

We build bottom-up: a language that runs, then tooling around its AST.
