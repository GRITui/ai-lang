#!/usr/bin/env bash
# Verify doctor's no-cc path: a host without a C compiler must SKIP the cc and
# aot checks (never FAIL them), and still exit 0 — an interpreter-only install
# is a valid install, and a diagnostic that goes red for it is useless.
#
# Runs against whichever profile binary exists, so it works both after a plain
# `cargo build` and in CI, which builds release only.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."; cargo build -q --release || exit 1
  BIN=target/release/ainl
fi
if [ ! -x "$BIN" ]; then
  echo "FAIL: no ainl binary at $BIN"; exit 1
fi
# The stripped PATH has no `sh` shim, so the binary is invoked by absolute path.
AINL_ABS="$PWD/$BIN"

EMPTY=$(mktemp -d)
trap 'rm -rf "$EMPTY"' EXIT

# A PATH with only the shims needed to run the binary; no `cc`.
mkdir -p "$EMPTY/bin"
for t in cat ls uname; do
  p=$(command -v "$t") && ln -sf "$p" "$EMPTY/bin/$t"
done

echo "== which cc under the stripped PATH =="
PATH="$EMPTY/bin" command -v cc || echo "(cc not found — as intended)"

echo
echo "== ainl doctor with no cc on PATH =="
out=$(PATH="$EMPTY/bin" "$AINL_ABS" doctor 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
echo "EXIT=$rc"

if [ "$rc" -ne 0 ]; then
  echo "RESULT: FAIL — doctor should exit 0 without cc"
  exit 1
fi
if echo "$out" | grep -qE '^cc[[:space:]]+FAIL'; then
  echo "RESULT: FAIL — cc reported FAIL instead of SKIP"
  exit 1
fi
# The aot check must be skipped too: with no compiler it cannot be verified,
# and reporting it green would be claiming something untested.
if echo "$out" | grep -qE '^aot[[:space:]]+ok'; then
  echo "RESULT: FAIL — aot reported ok without a compiler"
  exit 1
fi
echo "RESULT: PASS — no-cc host is reported as SKIP and still exits 0"
