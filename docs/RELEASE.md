# Release & portability (§1.2)

AINL's runtime is packaged as a **zero-dependency binary**: nothing in the
dependency graph but the workspace's own crates (`ainl-core`,
`ainl-transpile`, `ainl-cc`), so the linked binary has no third-party runtime
dependencies. Build it for any platform with a single command.

## Install (one line)

```sh
curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
ainl doctor
```

The installer detects the platform, picks the matching release asset,
downloads it **and** the release's `SHA256SUMS`, verifies the checksum, and
only then unpacks and installs to `~/.local/bin` (or `/usr/local/bin` with
sudo). It is idempotent — re-run it to upgrade.

| variable | effect |
|---|---|
| `AINL_VERSION=0.3.0` | pin a version (with or without the leading `v`) |
| `AINL_BIN_DIR=/path` | install somewhere else (used by the CI test) |
| `AINL_RELEASE_BASE=…` | override the download base (used by the CI test) |

**The installer never installs an unverified binary.** A release with no
`SHA256SUMS`, or an asset whose checksum does not match, exits non-zero and
leaves nothing behind. Those refusal paths are asserted in CI by
`scripts/check-install.sh`.

## Verifying a download by hand

```sh
curl -LO …/releases/download/v0.3.0/ainl-v0.3.0-x86_64-unknown-linux-musl.tar.gz
curl -LO …/releases/download/v0.3.0/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing   # macOS: shasum -a 256 -c …
```

## `ainl doctor` — the self-diagnostic

`ainl doctor` answers "is this install actually working?" and exits 0 only if
nothing failed.

| check | what it proves |
|---|---|
| `version` | version, build target, and source commit |
| `cc` | a host C compiler is present (needed only by `ainl compile`) |
| `grammar` | the GBNF export is *usable*: a `root` rule exists and every referenced rule resolves |
| `eval` | interpreter + stdlib produce a computed value |
| `run` | a real program (recursion, control flow) evaluates correctly |
| `transpile` | all three backends emit non-trivial code |
| `aot` | the generated C is accepted by a real compiler |

A **missing** `cc` is reported as `SKIP`, not `FAIL`: everything except
`ainl compile` works without a C compiler, so that host has a working install
and must not be told otherwise. `scripts/check-doctor-no-cc.sh` pins that
behaviour by running doctor under a stripped `PATH`.

## One command

```sh
./scripts/build-release.sh
```

This always builds the native release binary and reports its dynamic
dependencies, then builds fully-static Linux binaries if the tooling is present.

## Releases (v0.2.0+)

Releases are cut by the **CI release pipeline** (`.github/workflows/release.yml`),
triggered by pushing a `v*` tag:

1. `linux-musl` (ubuntu-latest): `rustup target add x86_64-unknown-linux-musl`
   + `apt-get install musl-tools`, then `cargo build --release --target
   x86_64-unknown-linux-musl`. Verifies the artifact is fully static
   (`file` → "static-pie linked", `ldd` → "statically linked" / "not a
   dynamic executable"), runs `ainl doctor`, and asserts `ainl --version`
   names this commit and this target triple.
2. `macos` (macos-latest, arm64): native `cargo build --release`, same
   `doctor` + provenance assertions.
3. `release`: downloads both artifacts, generates a single `SHA256SUMS` over
   both assets, asserts the asset count matches the sum count, verifies its
   own output, and attaches everything with `gh release create` (notes from
   `RELEASE_NOTES.md`).

Checksums are generated **once**, in the release job, so the two platform
jobs cannot disagree about what was published. `scripts/check-sums.sh`
reproduces that step locally against a fake `dist/` tree.

To cut a new release: bump `version` in `Cargo.toml`, update
`RELEASE_NOTES.md`, commit, `git tag v<version> && git push origin v<version>`.

| asset | target |
|---|---|
| `ainl-v<version>-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, fully static (musl) |
| `ainl-v<version>-aarch64-apple-darwin.tar.gz` | macOS arm64 |
| `SHA256SUMS` | checksums for both assets above |

The binary itself is ~400 KB (release profile: `opt-level="z"`, `lto`, `strip`,
`panic="abort"`); each tarball adds README + both licenses.

## What "zero dependency" means, verified

The native macOS build links only the base system library:

```
$ otool -L target/release/ainl
target/release/ainl:
	/usr/lib/libSystem.B.dylib
```

`libSystem` is macOS's libc-equivalent and is present on every Mac — there is no
third-party dependency chain. The release profile (`opt-level="z"`, `lto`,
`strip`, `panic="abort"`) keeps the binary small (~400 KB).

## Fully-static Linux binaries (install anywhere)

For a binary with **no** dynamic dependencies at all — installs on any Linux
box, container, or edge device by copying one file — build against a musl target.
musl links statically by default in Rust. Two supported paths:

### Path A — Docker (recommended, no host toolchain)

```sh
cargo install cross
cross build --release --target x86_64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl
```

### Path B — musl cross-toolchain on the host

```sh
# macOS example (Homebrew):
brew install messense/macos-cross-toolchains/x86_64-unknown-linux-musl
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

`.cargo/config.toml` already wires these targets to their cross-linkers
(`x86_64-linux-musl-gcc`, `aarch64-linux-musl-gcc`).

### Verify it's static

```sh
$ file target/x86_64-unknown-linux-musl/release/ainl
ELF 64-bit LSB executable, x86-64, ... statically linked, ...
$ ldd target/x86_64-unknown-linux-musl/release/ainl
        not a dynamic executable
```

## Note on cross-compiling from macOS

Building a Linux/musl target on macOS **compiles** fine but **links** only when a
GNU/Linux cross-linker is available (Apple's `ld` rejects GNU link options). Use
Path A or Path B above, or — simplest — let CI do it: pushing a `v*` tag runs
the release pipeline, which builds the musl artifact natively on an ubuntu
runner (see "Releases (v0.2.0+)" above). A plain `cargo build --target ...-musl`
on a bare macOS host will fail at the link step by design.
