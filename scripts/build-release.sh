#!/usr/bin/env bash
# Build the `ainl` runtime as a zero-dependency binary for release (§1.2).
#
# Always builds a native release binary. Additionally builds fully-static Linux
# musl binaries when the tooling is available (Docker + `cross`, or a musl
# cross-toolchain), and otherwise clearly reports what to install.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> native release build"
cargo build --release
BIN=target/release/ainl
printf "    built %s (%s)\n" "$BIN" "$(ls -lh "$BIN" | awk '{print $5}')"

# Report the native binary's dynamic dependencies (should be system-only).
if command -v otool >/dev/null 2>&1; then
  echo "    deps:"; otool -L "$BIN" | tail -n +2 | sed 's/^/      /'
elif command -v ldd >/dev/null 2>&1; then
  echo "    deps:"; ldd "$BIN" 2>/dev/null | sed 's/^/      /' || echo "      (static)"
fi

TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)

build_static() {
  local t="$1"
  local arch="${t%%-*}"
  if command -v cross >/dev/null 2>&1; then
    echo "==> cross (Docker) static build: $t"
    cross build --release --target "$t"
  elif rustup target list --installed 2>/dev/null | grep -q "^$t$" \
       && command -v "${arch}-linux-musl-gcc" >/dev/null 2>&1; then
    echo "==> native static build: $t"
    cargo build --release --target "$t"
  else
    echo "!! skipping $t"
    echo "   install Docker + 'cargo install cross', OR a musl cross-toolchain"
    echo "   ('${arch}-linux-musl-gcc') and 'rustup target add $t'"
    return 0
  fi
  local out="target/$t/release/ainl"
  printf "   static binary: %s (%s)\n" "$out" "$(ls -lh "$out" | awk '{print $5}')"
  command -v file >/dev/null 2>&1 && file "$out" | sed 's/^/     /'
}

for t in "${TARGETS[@]}"; do build_static "$t"; done
echo "==> done"
