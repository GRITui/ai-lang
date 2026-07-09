# Master Plan: AI-Native Language & Orchestrated Prompt Auditor

## Phase 1 — Build an AI-Native Programming Language

**Core objective:** a high-density, strictly-semantic language optimized for LLM context windows rather than human readability.

### 1.1 Cross-language framework integration
- Bidirectional interop between AINL and traditional languages.
- AST mapping + source-map projection so the AI's dense operations translate back to human-readable code for version control and debugging.
- Rust core: fast, safe, good crate ecosystem for parsing/codegen.

### 1.2 Universal portability and installation
- Package the runtime as a **zero-dependency static binary**.
- Compile with Rust's `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` targets (and native macOS/Windows) so it installs instantly on any machine, edge device, or container with no dependency chain.

### 1.3 AI-optimized syntax & markdown documentation
- Rigid, low-entropy syntax (S-expression core) — no non-functional whitespace, no syntactic sugar — to maximize token efficiency.
- A specialized [SYNTAX.md](SYNTAX.md) written **for AI ingestion**: any model can read it once and emit perfectly-formed AINL.
- Grammar is a small regular CFG → directly usable as a GBNF / constrained-decoding grammar.

### 1.4 Cross-programming-language plugins
- Extensible plugin system so AINL apps interoperate with external codebases.
- Rollout order: **Python, JavaScript, Ruby** first; modular architecture lets the community add more.
- Each plugin = an AST transpiler (AINL AST ⇄ target-language AST) + a runtime FFI shim.

## Phase 2 — Multi-LLM Orchestrated Prompt Auditor

**Core objective:** a GUI app that intercepts vague human language and uses a swarm of local SLMs to audit, plan, and compile the request into a perfect AINL schema.

### 2.1 Agentic orchestration flow (sub-10B local models)
1. **Orchestrator** (Qwen3.5-9B) — ingests the request, analyzes intent, picks the execution path.
2. **Planner** (DeepSeek-R1-Distill-Qwen-7B) — maps logic, edge cases, step-by-step reasoning for complex work.
3. **Auditor / Syntax Enforcer** (Phi-4-mini 3.8B) — grammar-constrained decoding turns the plan into a mathematically strict AINL JSON/markdown schema.
4. **Code Engine** (IBM Granite 4.1 8B) — consumes the schema to generate final logic / dense AINL code.
5. **Generalist** (Llama 3.1 8B Instruct) — summarizes technical output into a friendly, human-readable result for the GUI.

### 2.2 Natural language → LLM-context translation
- The GUI is an **intent compiler**: users prompt conversationally; the swarm audits and rewrites into optimized, constraint-based LLM-context markdown.
- Output is copy-pasteable into any frontier model to get one-shot, correct generation without multi-turn correction.

### 2.3 Extensible router & server wiring
- Wire to Model Context Protocol (MCP) servers and third-party LLM routers.
- Pull real-time context from the local filesystem or enterprise databases before compiling the final prompt.

---

## Sequencing (how we actually build it)

| Milestone | Deliverable | Verifies |
|-----------|-------------|----------|
| **M1** ✅ | Working AINL interpreter (lexer→parser→eval), CLI `run`/`repl`/`ast` | Language exists and executes |
| M2 | `SYNTAX.md` + GBNF grammar export (`ainl grammar`) | Models can generate valid AINL |
| **M3** ✅ | Stable JSON AST serialization + source-map loc (`ainl ast --json`) | Foundation for interop & auditor schema |
| **M4** ✅ | Python transpiler plugin (AINL → Python), `ainl transpile` | Phase 1.4 proof of concept; output verified byte-equal to the interpreter |
| **M5** ✅ | musl static-binary release pipeline (`scripts/build-release.sh`, `.cargo/config.toml`, `docs/RELEASE.md`) | Phase 1.2 portability; native binary verified system-only deps (393 KB) |
| M6 | Prompt Auditor GUI shell + local model router | Phase 2 skeleton |
| M7 | Full 5-model orchestration + constrained decoding | Phase 2 complete |

**Phase 1 is complete.** The AINL language runs, serializes its AST with source
maps, exports a constrained-decoding grammar, transpiles byte-equivalently to
Python/JavaScript/Ruby, and ships as a zero-dependency binary. Phase 2 (the
multi-SLM Prompt Auditor) is next.

We build bottom-up: a language that runs, then tooling around its AST, then the auditor that targets it.
