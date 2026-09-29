#!/usr/bin/env bash
# Run every package's tests to completion and summarise.
#
# `cargo test --workspace` stops at the first failing target, so one
# pre-existing failure hides everything after it — and the interesting question
# ("did *my* change break anything?") is exactly the one that gets lost. This
# runs each package separately with no early exit, so the whole picture shows up
# in one go.
set -uo pipefail
cd "$(dirname "$0")/../.."
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

for p in ainl-core ainl-cc ainl-transpile ainl; do
  cargo test -p "$p" >>"$OUT" 2>&1
  printf 'ran %s\n' "$p"
done

echo
echo "== test counts per package =="
awk '
  /^test result:/ {
    ok = ($0 ~ /ok\./)
    n = $4
    if (ok) { passed += n; targets_ok++ } else { targets_bad++; print "  FAILED target: " prev }
    total += n
  }
  /^     Running/ { prev = $2 }
  END {
    printf "  %d targets ok, %d targets failed\n", targets_ok, targets_bad
    printf "  %d tests passed of %d\n", passed, total
  }
' "$OUT"

echo
echo "== failing tests =="
grep -E '^test .* FAILED$' "$OUT" | sort -u || true

if grep -qE 'test result: FAILED' "$OUT"; then
  echo
  echo "RESULT: FAIL"
  exit 1
fi
echo
echo "RESULT: PASS"
