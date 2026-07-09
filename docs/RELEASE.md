# Release & portability (§1.2)

AINL's runtime is packaged as a **zero-dependency binary**: nothing in the
dependency graph but the workspace's own crates (`ainl-core`, `ainl-transpile`),
so the linked binary has no third-party runtime dependencies. Build it for any
platform with a single command.

## One command

```sh
./scripts/build-release.sh
```

This always builds the native release binary and reports its dynamic
dependencies, then builds fully-static Linux binaries if the tooling is present.

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
Path A or Path B above; a plain `cargo build --target ...-musl` on a bare macOS
host will fail at the link step by design.
