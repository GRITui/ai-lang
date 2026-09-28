**AINL v0.3.0** — the distribution release.

Since v0.2.0 the language grew an AOT compiler and a standard library. Both
are already in the runtime. This release is about the layer *around* it: one
command to install, a checksum you can verify, and a diagnostic that tells
you whether the thing actually works.

### Install (one line)

```sh
curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
ainl doctor
```

The installer picks the right binary for your platform, verifies its SHA256
against the release's `SHA256SUMS`, and installs to `~/.local/bin` — no root,
no sudo, no build toolchain. **It refuses to install anything it cannot
verify**: a missing `SHA256SUMS` or a checksum mismatch is an error, not a
silent install. Re-run it to upgrade.

### What's new

- **`SHA256SUMS` on every release**, covering both assets, generated once in
  the release job so the two platform builds cannot disagree about what was
  published.
- **`ainl doctor`** — a seven-check self-diagnostic that exits 0 only if
  everything passes: binary provenance, the host C compiler, GBNF export
  validity, the interpreter + stdlib, a real program, all three transpiler
  backends, and the AOT code generator. It runs at the end of every install
  and gates the release pipeline, so a subtly broken binary cannot ship.
- **`ainl --version`** now reports the build target and the source commit, so
  a bug report can name the exact artifact. The release pipeline asserts both
  against the tag, which makes the value load-bearing rather than decorative.
- **The release gate is the real diagnostic.** It used to be
  `ainl eval '(* 6 7)' == 42`; it is now `ainl doctor`, which exercises the
  interpreter, the stdlib, the grammar, all three transpilers, and the
  generated C.

### A note on `cc`

`ainl compile` (AINL → C → native binary) needs a host C compiler. Without
one, `ainl doctor` reports it as **SKIP**, not a failure — the interpreter,
the transpilers, and the grammar export all work without `cc`, so a host
without one has a working install of everything else.

### Verify by hand

```sh
curl -LO …/releases/download/v0.3.0/ainl-v0.3.0-x86_64-unknown-linux-musl.tar.gz
curl -LO …/releases/download/v0.3.0/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing      # macOS: shasum -a 256 -c …
```

### Docs
[Release & portability](https://github.com/GRITui/ai-lang/blob/main/docs/RELEASE.md) ·
[Getting started](https://github.com/GRITui/ai-lang/blob/main/docs/GETTING_STARTED.md) ·
[Syntax & standard library](https://github.com/GRITui/ai-lang/blob/main/docs/SYNTAX.md) ·
[Performance](https://github.com/GRITui/ai-lang/blob/main/docs/PERFORMANCE.md) ·
[Constrained generation](https://github.com/GRITui/ai-lang/blob/main/docs/GENERATION.md)
