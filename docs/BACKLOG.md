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
- **HTTPS for `http-get` / `http-post`**: AINL speaks plain HTTP only, and
  `https://` is refused by name. This was a decision, not an omission — the
  zero-dependency rule is what keeps the AOT binary statically linked, and
  every TLS stack is a C-transitive dependency tree. The two priced paths (a
  local TLS-terminating proxy, or an opt-in `--features tls` build) and the
  reasoning are in [HTTP_TLS.md](HTTP_TLS.md). Revisit only if the
  standalone-binary property stops being a claim.
- **An HTTP client in the AOT C backend and the three transpilers**: both
  builtins are interpreter-only today and the other backends refuse them with
  an `interpreter-only` error. A port means an HTTP/1.1 client in the C runtime
  (a socket plus a parser, and the static-link guarantee has to survive it) and
  three host-library mappings that must agree with
  `crates/ainl-core/src/http.rs` byte for byte — the same contract
  `json_value.rs` has to its three ports. Named in SYNTAX.md §3c.
- **An HTTP server / listening socket**: AINL can fetch, not serve. There is no
  `http-listen`, so two AINL programs cannot talk to each other.

## Recently completed

**Integrated main head (bignum + Tier 4 tables/SQL + B-tree fix + `rmdir`)** —
`main` now carries the union of the four divergent lines: arbitrary-precision
integers (bignum in the interpreter, native `BigInt` in the JS target — all
five backends agree byte-for-byte on any integer, see
[NUMERIC_MODEL.md](NUMERIC_MODEL.md)), KV storage, Tier 4 tables
(`db-create-table`/`db-insert`/`db-select`/`db-all-rows`) and SQL
(`db-query`/`db-query-count`), the PR #23 B-tree release-profile fix, and
`rmdir`/`delete-dir`. The two numeric backlog entries above were retired by
the bignum/BigInt work. 87 prelude builtins (68 portable across all four
backends; the other 19 are the 2 HTTP builtins every backend refuses and the
17 `db-*` builtins the AOT C runtime carries but the transpilers refuse).

**HTTP client (`http-get` / `http-post`)** — a hand-written zero-dependency
HTTP/1.1 client in `ainl-core` (`http.rs`, the normative implementation):
GET/POST against any `http://` URL, request headers, `Content-Length` and
chunked response bodies, and a response that is an ordinary AINL map
(`status`/`ok`/`body`/`headers`/`reason`/`truncated`) so it needs no new access
syntax. A non-2xx status is a *value*, not an error. Fixed limits (10s connect,
30s read, 8 MiB body, 64 KiB headers) rather than an unbounded read, and
`Host`/`Content-Length`/CRLF/userinfo are refused as the request-smuggling
primitives they are. **No TLS** — `https://` is refused with the fix in the
message, before a socket is opened. Interpreter-only, with the AOT backend and
all three transpilers refusing explicitly. 27 end-to-end tests against a real
loopback server, 12 backend-refusal tests, and a doc gate that executes every
claim in SYNTAX.md §3c. ([HTTP_TLS.md](HTTP_TLS.md))

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
