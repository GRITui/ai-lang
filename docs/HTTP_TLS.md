# HTTP and TLS — the decision, and what it costs

This is the companion note to `docs/SYNTAX.md` §3c. It records the one real
decision behind `http-get` / `http-post`, why it went the way it did, and what
the two ways out of it would actually cost. It is here because the decision is
the kind that looks like an omission in a year, and a reader deserves to know
it was a choice.

## The decision

**AINL speaks plain HTTP over TCP. There is no TLS. `https://` is refused by
name, with an error that names the fix.**

The Tier 1 HTTP card put it as a choice between (a) plain HTTP now and HTTPS as
a follow-up, and (b) the smallest TLS path that keeps the standalone-binary
promise. This is (a). The reasoning:

**The zero-dependency rule is load-bearing, not stylistic.** AINL's AOT backend
emits a single C file that links against nothing but libc, and the
`aot-standalone` CI job proves it by linking with `x86_64-linux-musl-gcc
-static` and asserting the result is statically linked. That job is not a
formality: "a static binary you can copy to a server and run" is one of the two
properties the project actually claims (the other is the four-backend
guarantee). Every production TLS stack breaks it:

| option | C-transitive deps | static musl link |
|---|---|---|
| `rustls` + `ring` | yes (C + asm) | no — `ring` ships per-target asm objects |
| `rustls` + `aws-lc-rs` | yes (C + cmake) | no — needs cmake + a C toolchain at build time |
| `native-tls` | yes (OpenSSL) | only against a static libssl you build yourself |
| `openssl` crate | yes (vendored OpenSSL, ~500k lines) | buildable, but not zero-dep and not small |

So option (b) is not "add a dependency" — it is "give up the standalone
binary, or take on maintaining a vendored TLS stack." Given the choice, AINL
keeps the property it has and refuses the protocol it cannot do honestly.

**A hand-rolled TLS is not option (c).** It is worth saying plainly, because
"just implement it ourselves" sounds available: a TLS 1.2/1.3 implementation is
~10k lines and has to be right about certificate chain validation, padding
oracles, constant-time comparisons, downgrade protection, session
resumption, and the constant-time requirements that a compiler will happily
undo. A client that gets any of that wrong is not "less secure than no HTTPS" —
it is a client that silently accepts a forged certificate, which is the exact
failure TLS exists to prevent. So: no.

## What the refusal looks like

```
(ainl) (http-get "https://example.com/")

runtime error: https is not supported: AINL speaks plain HTTP only, so its
runtime can stay dependency-free and its AOT binaries stay standalone.
Use http://example.com/ instead, or front the endpoint with a local proxy
that terminates TLS. (docs/HTTP_TLS.md)
```

Two things this does on purpose:

- **It fires before a socket is opened.** No DNS lookup, no connect, no bytes on
  the wire. A call that can never succeed should not cost a network round trip
  to say so, and the refusal is then testable offline and deterministically.
  (`build_request` is a pure function precisely so this is structural rather
  than a promise — see `crates/ainl-core/src/http.rs`.)
- **It names the `http://` form of the same URL.** The fix is in the message,
  because the overwhelmingly likely case is a caller who wrote `https` and
  meant the local dev server.

## The two ways forward, priced

If HTTPS is wanted later, these are the real options. Both are conscious
exceptions to the zero-dep rule, and the card's instruction is that such an
exception must be documented rather than absorbed silently.

**1. Local-proxy escape hatch (what the error message suggests).** No code at
all: the caller runs a TLS-terminating proxy (nginx, `caddy`, `socat`,
`mitmproxy`) on loopback and points AINL at the `http://` side. This is what the
error message recommends, and for the "call a real API from a script" case it
is often the right answer anyway. Cost: one process, and TLS policy lives
outside the language.

**2. A feature-gated TLS client, opt-in at build time.** Add `rustls` behind a
non-default cargo feature (`--features tls`). The default build stays
zero-dep and standalone; the TLS build is a second artifact with an explicit,
documented dependency. Cost: the first dependency in the workspace, a
feature-matrix CI job, and a documented divergence between the two builds —
which AINL's own rules dislike, and which would have to be called out in
`doctor` so nobody is surprised that their binary has no HTTPS.

**Rejected: a documented "no TLS ever" stance.** A language whose HTTP client
cannot reach `https://` is a real limitation. It is a *survivable* one — the
proxy route covers most use — but it is a limitation, and this note is where it
is recorded rather than left to be rediscovered.

## The other half of the decision: interpreter-only

Independently of TLS, `http-get` / `http-post` are **interpreter-only**. The AOT
C backend and the Python/JS/Ruby transpilers refuse a program that uses them
with the same `interpreter-only` contract `import` uses
(`crates/ainl-cc/tests/http_refusal.rs`).

That is a scope decision, not a TLS one, and the reason is the four-backend
rule. AINL's central claim is that a program means the same thing on every
backend. HTTP is where the hosts disagree most:

- `requests`, `urllib` and `httpx` differ on redirect limits, header casing,
  TLS verification defaults, and what counts as a timeout.
- `fetch` follows redirects by default and re-derives `Host`; the interpreter's
  client does not follow redirects at all.
- Each of those differences is exactly the "builds cleanly, does something
  subtly different" failure the project is organized against.

Refusing is the honest answer, and it is reversible — when a backend does get a
client, it has to be one that agrees with `crates/ainl-core/src/http.rs` byte
for byte, the same contract `json_value.rs` has to its three ports.

## What AINL deliberately does not do in HTTP

Small, but each one is a rule rather than an omission:

- **No redirects.** A 3xx is returned as a response like any other, and the
  caller decides. A client that follows redirects can be walked somewhere the
  caller never named.
- **No cookies.** No jar, no `Set-Cookie` memory. AINL has one process and one
  request unless the caller writes the loop.
- **No keep-alive.** Every request sends `Connection: close`. There is no
  connection pool to reuse, so advertising otherwise would leave servers
  holding sockets open for a client that is about to exit.
- **No `Host` or `Content-Length` override.** They are the request-smuggling
  primitives; AINL sets both from the URL and the body.
- **No credentials in a URL.** `http://user:pw@host/` is refused — userinfo
  goes into a request line and gets logged by everything in the middle.
- **Fixed limits, not tunables.** 10s connect, 30s read, 8 MiB body, 64 KiB
  headers. A builtin that can hang forever or exhaust memory makes every
  program that touches the network untestable. These match the step-counter
  philosophy: a hard bound, documented, not a knob.
