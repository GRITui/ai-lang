# Backlog & priorities

Status as of Phase 1 complete (M1–M5 + §1.4) and Phase 2 started (M6). See
[MASTER_PLAN.md](MASTER_PLAN.md) for the full plan. This file is the working
priority list: **Prioritize** = do next (high value, unblocked), **Backlog** =
valuable but later, **Blockers** = needs an external resource or decision.

## 🔴 Blockers

| Item | Blocked on | Impact |
|------|-----------|--------|
| **M7 live model runs** | Local models not installed — need Ollama (or llama.cpp server) with `qwen3.5:9b`, `deepseek-r1-distill-qwen:7b`, `phi-4-mini:3.8b`, `granite4.1:8b`, `llama3.1:8b-instruct`. | Can't exercise the real (non-mock) pipeline. Code path (`HttpBackend`) is ready; needs a running server to test. |
| **Model tag confirmation** | Exact Ollama registry tags for `qwen3.5` and `granite4.1` need verifying/pinning; some may differ or need a Modelfile. | `Role::model()` strings may need adjustment once a real registry is targeted. |
| **musl static binary artifact** | No Docker/`cross` or musl cross-toolchain on this host; macOS can't link a Linux musl target without one. | The *pipeline* (`scripts/build-release.sh`, `.cargo/config.toml`, docs) is done and zero-dep is verified natively, but the actual static Linux artifact can't be produced here. |

_Unblock path for M7: install Ollama, `ollama pull` the five models (or the closest available tags), then `ainl audit "<text>" --backend http`._

## 🟡 Prioritize (next, unblocked)

1. **M7a — Auditor retry-on-invalid loop.** If the Auditor's AINL fails
   `ainl_core::parse`, re-prompt with the parser error (bounded retries) until it
   parses. Pure code; testable now with a deliberately-invalid mock backend.
   *The grammar makes failure rare, but the loop guarantees valid output.*
2. **JSON → AST round-trip (deserialization).** Hand-write a small JSON parser in
   `ainl-core` so `ast --json` output can be read back into a `Node` tree.
   Completes the §1.1 bidirectional-interop story; unblocks tools that author/
   rewrite AST as JSON. Verify: `parse → to_json → from_json` round-trips equal.
3. **Validate & surface the Code Engine stage.** Also parse the CodeEngine output
   (not just the Auditor's), and expose both in the report / `--stages`.
4. **`cargo install` + release packaging.** Wire `ainl` for `cargo install --path`
   and a GitHub-release workflow (native artifacts now, musl once unblocked).

## 🟢 Backlog (later)

- **GUI shell** over the pipeline (Tauri wrapping the Rust core, or a thin web UI) — the plan's "GUI intent compiler" (§2.2). Depends on M7 being useful first.
- **Live MCP `ContextProvider`s** (§2.3): real filesystem/DB/MCP-server context fetch (the `McpContextProvider` hook exists but is unimplemented).
- **AINL stdlib**: promote `map`/`filter`/`fold`, string ops, and hash/map values into the core so common programs are shorter.
- **Tail-call handling / stack safety** for deep recursion in the tree-walking evaluator (or a bytecode VM per §1.3's "stack-based bytecode" option).
- **More transpiler targets**: Go, Rust; and a *reverse* path (Python/JS → AINL) to strengthen the bidirectional framework claim (§1.1).
- **Editor tooling**: syntax highlighting + an LSP built on the AST + source spans.
- **JS int/float fidelity note**: JS has one number type, so AINL float division that yields a whole number prints without `.0` (documented limitation; examples avoid it).

## Recently completed

M1 interpreter · M2 grammar export · M3 JSON AST + source maps · M4 Python
transpiler · §1.4 JavaScript + Ruby transpilers (3×3 byte-equal) · M5 zero-dep
release pipeline · M6 Prompt Auditor pipeline skeleton (mock backend, in-process
grammar validation).
