# Architecture

## Phase 1 — the language runtime (implemented)

```
source (.ainl)
   │  lexer::lex            crates/ainl-core/src/lexer.rs
   ▼
tokens  (LParen | RParen | Atom{span} | Str{span})
   │  parser::parse         crates/ainl-core/src/parser.rs
   ▼
AST  (Node: Int|Float|Str|Sym|List, every node carries a byte Span)
   │  eval::eval            crates/ainl-core/src/eval.rs
   ▼
Value  (Nil|Bool|Int|Float|Str|Sym|List|Builtin|Closure)
```

### Crates

- **`ainl-core`** — the whole language as a zero-dependency library:
  - `lexer` — char-level tokenizer, `;` comments, string escapes.
  - `parser` — recursive-descent over the flat token list into a uniform AST.
    Numbers are typed at parse time; every node keeps a `Span` (byte range) so
    downstream tools can build **source maps** back to the original AINL.
  - `value` — runtime value type + `Display`/`repr` printing.
  - `eval` — lexical `Env` (parent-linked scopes over `Rc`), special-form
    dispatch, closures, and the builtin prelude.
- **`ainl-cli`** — the `ainl` binary (`run` / `eval` / `ast` / `repl`).

### Why these choices map to the master plan

- **Zero dependencies** (§1.2): nothing in `Cargo.toml` but the two local
  crates, so the runtime compiles to a small static binary. The release profile
  (`opt-level="z"`, `lto`, `strip`, `panic="abort"`) is tuned for that; a musl
  target produces a fully static binary with no libc dependency chain.
- **AST + spans** (§1.1): the `Span` on every `Node` is the hook for AST mapping
  / source-map projection between AINL and human-readable languages. `ainl ast`
  already emits the tree with spans — the serialization surface a transpiler or
  the Phase 2 auditor schema will consume.
- **Regular grammar** (§1.3): the parser accepts exactly the grammar in
  `SYNTAX.md`, which is small enough to double as a constrained-decoding grammar.

## Phase 1 roadmap hooks (not yet built)

- `ainl ast --json` — stable JSON serialization of the AST (M3), the bridge to
  interop and the auditor schema.
- `ainl-transpile` crate — AINL AST → Python/JS/Ruby source with source maps (M4).
- Release workflow targeting `*-unknown-linux-musl` for static binaries (M5).

## Phase 2 — Prompt Auditor (planned)

A GUI shell drives a local model router that pipelines five sub-10B SLMs
(Orchestrator → Planner → Auditor → Code Engine → Generalist). The Auditor stage
emits AINL under the grammar in `SYNTAX.md` via constrained decoding, so its
output is guaranteed to parse with `ainl-core`. External context arrives through
MCP servers and third-party routers. See `MASTER_PLAN.md` §2.
