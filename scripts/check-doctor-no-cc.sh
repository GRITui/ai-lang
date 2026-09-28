#!/usr/bin/env bash
# Verify doctor's no-cc path: a host without a C compiler must SKIP the cc and
# aot checks (never FAIL them), and still exit 0 — an interpreter-only install
# is a valid install, and a diagnostic that goes red for it is useless.
set -uo pipefail
cd /Users/grit/.hermes/kanban/workspaces/t_3ac20a98/repo

EMPTY=$(mktemp -d)
trap 'rm -rf "$EMPTY"' EXIT

# A PATH with only the shims needed to run the binary; no `cc`, no `sh`-found cc.
mkdir -p "$EMPTY/bin"
for t in cat ls uname; do
  p=$(command -v "$t") && ln -sf "$p" "$EMPTY/bin/$t"
done

echo "== which cc under the stripped PATH =="
PATH="$EMPTY/bin" command -v cc || echo "(cc not found — as intended)"

echo
echo "== ainl doctor with no cc on PATH =="
PATH="$EMPTY/bin" ./target/debug/ainl doctor
rc=$?
echo "EXIT=$rc"

if [ "$rc" -ne 0 ]; then
  echo "RESULT: FAIL — doctor should exit 0 without cc"
  exit 1
fi
if PATH="$EMPTY/bin" ./target/debug/ainl doctor | grep -q 'cc .*FAIL'; then
  echo "RESULT: FAIL — cc reported FAIL instead of SKIP"
  exit 1
fi
echo "RESULT: PASS — no-cc host is reported as SKIP and still exits 0"
