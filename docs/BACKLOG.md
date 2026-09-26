# Backlog & priorities — the language

This project is now **language + toolchain only**. The Prompt Auditor was spun
out to its own (currently private) repository; its backlog (real model wiring,
retry loop, GUI, MCP providers) lives there.

**Prioritize** = do next (high value, unblocked), **Backlog** = valuable but
later, **Blockers** = needs an external resource or decision.

## 🔴 Blockers

(none)

## 🟡 Prioritize (next, unblocked)

1. **Make the repo public** (optional) — so `cargo install --git` and the release
   assets work for others, and so the auditor's CI can fetch `ainl-core` without
   a token.
2. **Publish `ainl-core` to crates.io** — turns the auditor's git dependency into
   a normal versioned crate dependency and gives the language a real distribution
   channel. (Bigger commitment; do once the crate API feels stable.)

## 🟢 Backlog (later)

- **The density problem** (the interesting one): AINL is ~2× Python's tokens
  (see [BENCHMARK.md](BENCHMARK.md)). If density is to be a real selling point,
  the surface syntax needs redesign — a parenless/layout-based form, or the
  "stack-based bytecode" option from the plan. Re-run `bench/bench.py` to prove
  any change helps.
- **AINL stdlib**: promote `map`/`filter`/`fold` and string ops into the core so
  common programs are shorter (and closes some of the token gap). Hash/map
  values landed — `hash`/`get`/`assoc`/`has`/`keys`/`vals`, see SYNTAX.md §3.
- **Tail-call handling / stack safety** for deep recursion in the tree-walking
  evaluator (or a bytecode VM).
- **More transpiler targets**: Go, Rust; and a *reverse* path (Python/JS → AINL).
- **Editor tooling**: syntax highlighting + an LSP built on the AST + source spans.
- **JS int/float fidelity note**: JS has one number type, so AINL float division
  that yields a whole number prints without `.0` (documented limitation).
- **Numeric model unification**: integer overflow behavior diverges across all
  four runtimes today — see [NUMERIC_MODEL.md](NUMERIC_MODEL.md). Closing this
  means either an arbitrary-precision integer type in the (zero-dependency)
  interpreter, or `BigInt`-based codegen for the JS target; currently the
  divergence is documented and pinned by tests rather than fixed.

## Recently completed

**JSON → AST round-trip (deserialization)** — hand-written zero-dependency JSON
parser in `ainl-core` (`deserialize::json_to_forms`, CLI `ainl ast <file>
--json-out <json>`); reads the stable JSON AST back into a `Node` tree with
exact byte-span recovery; `parse → to_json → from_json` round-trips equal
(structural equality including spans) for every example plus a full
node-type/builtin suite. · v0.2.0 shipped · **Linux x86_64 musl static artifact via CI** (release
workflow builds + verifies + attaches it; no Docker/cross needed on the dev
host) · v0.1.0 shipped · M1 interpreter · M2 grammar export · M3 JSON AST + source maps ·
M4 Python transpiler · §1.4 JavaScript + Ruby transpilers (3×3 byte-equal) · M5
zero-dep release pipeline · dual license · token-density benchmark · **forked the
Prompt Auditor to its own repo** · a four-lens multi-agent review (design,
LLM-generation ergonomics, Rust implementation, safety) followed by fixes for
every finding: resource limits + panic fixes, cross-target equality bugs,
`def`/scope semantics documented, numeric-overflow divergence documented and
pinned, transpiler backends de-duplicated, an `Rc` cycle leak fixed, and a
map/record type (`hash`/`get`/`assoc`/`has`/`keys`/`vals`) added across the
interpreter and all three transpilers.
