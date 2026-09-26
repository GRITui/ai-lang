**AINL v0.2.0** — the Linux release.

v0.1.0 shipped a macOS-only artifact even though the README promises a
zero-dependency binary that "installs anywhere". This release closes that gap:
the fully-static **Linux x86_64 (musl)** binary is now built in CI and
attached to the release, alongside a rebuilt macOS aarch64 asset from the same
tag.

### What's new
- **Linux x86_64 musl static binary** — `ainl-v0.2.0-x86_64-unknown-linux-musl.tar.gz`.
  Verified fully static in CI (`ldd` → "not a dynamic executable", `file` →
  "statically linked"): copy one file to any Linux box, container, or edge
  device and it runs. No glibc, no Docker, no cross-toolchain on the host.
- **Release pipeline** — `.github/workflows/release.yml` builds both assets
  natively on tag push (musl on an ubuntu runner, macOS on an arm64 runner),
  verifies staticness, smoke-tests both binaries (`ainl eval '(* 6 7)'` → 42),
  and attaches them to the release. Repeatable: bump the version, push the tag.
- **Rebuilt macOS aarch64 asset** from the same tag (same layout as v0.1.0).

### Install
```sh
# Linux x86_64
curl -LO https://github.com/GRITui/ai-lang/releases/download/v0.2.0/ainl-v0.2.0-x86_64-unknown-linux-musl.tar.gz
tar xzf ainl-v0.2.0-x86_64-unknown-linux-musl.tar.gz
./ainl-v0.2.0-x86_64-unknown-linux-musl/ainl eval '(* 6 7)'   # 42

# or from source (any platform)
cargo install --git https://github.com/GRITui/ai-lang ainl-cli
```

### Docs
[Release & portability](https://github.com/GRITui/ai-lang/blob/main/docs/RELEASE.md) ·
[Getting started](https://github.com/GRITui/ai-lang/blob/main/docs/GETTING_STARTED.md) ·
[Syntax](https://github.com/GRITui/ai-lang/blob/main/docs/SYNTAX.md) ·
[Constrained generation](https://github.com/GRITui/ai-lang/blob/main/docs/GENERATION.md)
