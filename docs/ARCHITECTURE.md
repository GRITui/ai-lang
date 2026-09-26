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
  already emits the tree with spans — the serialization surface transpilers and
  other downstream tooling consume.
- **Regular grammar** (§1.3): the parser accepts exactly the grammar in
  `SYNTAX.md`, which is small enough to double as a constrained-decoding grammar.

### AST JSON interchange (M3, implemented)

`serialize::forms_to_json` (crate API `parse_to_json`, CLI `ainl ast --json`)
emits a stable, pretty-printed JSON document:

```json
{ "version": "0.1", "source": "…",
  "forms": [ { "t": "list", "span": [s,e], "loc": [line,col], "items": [ … ] } ] }
```

Every node carries both a byte `span` and a 1-based `loc` (line, char-column)
computed by `serialize::LineIndex`. That `span`/`loc` pair is the source-map
primitive: a projected human-readable line maps back to the exact AINL bytes.
The serializer is hand-written (no serde) to preserve the zero-dependency
guarantee. Deterministic field order makes the output diffable in VCS.

### Transpiler plugins (M4, Python implemented)

`ainl-transpile` projects the AST into traditional languages (master plan §1.4).
The **Python** target (`ainl transpile <file> --to python`, API
`transpile_python`) lowers AINL's expression-oriented forms into idiomatic
Python using two emission contexts:

- **expression context** — forms with a natural Python expression: `if`→`a if c
  else b`, chained comparisons (`(< 1 2 3)`→`(1 < 2 < 3)`), `fn`→`lambda`,
  single-body `let`→IIFE, `quote`→data literals.
- **statement context** — bodies and the module top level, where `def`/`while`/
  `let`/`do`/multi-branch `if` become real statements and a function's tail form
  is `return`ed. Forms with no expression form (e.g. `while`) error in
  expression position with the offending source span.

Only the runtime helpers a program actually uses are emitted (a small `_disp`/
`_print`/list-op shim), and a `# ainl:<line>` source-map comment precedes each
top-level definition. **Verified**: `hello`/`fib`/`lists` transpile to Python
whose stdout is byte-identical to the AINL interpreter. New targets (JS, Ruby)
follow the same `Node`→`String` shape.

## Phase 1 roadmap hooks (not yet built)

- JSON → AST deserialization (round-trip) so tools can author/rewrite AST as JSON.
- JavaScript + Ruby transpiler targets (§1.4 rollout).
- Release workflow targeting `*-unknown-linux-musl` for static binaries (M5).

## Prompt Auditor — separate project

The multi-model Prompt Auditor that generates AINL from natural language has
been spun out to its own repository, which is currently private.
It consumes this project — the Auditor stage emits AINL under the exported GBNF
grammar and validates it with `ainl-core`'s parser — but is developed and
released independently. Nothing in `ai-lang` depends on it.
