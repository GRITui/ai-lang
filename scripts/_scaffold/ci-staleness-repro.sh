#!/usr/bin/env bash
# Simulate what the `rust` CI job does, from a deliberately STALE release
# binary — the state a restored actions/cache leaves behind.
#
# Reproduces the seven `error_class_parity` failures without waiting on CI:
#   1. backdate target/release/ainl so it is older than every source
#   2. run the job's steps in order
#   3. assert the test file passes
#
# Exits non-zero if the fix (the added release-build step) is not in ci.yml.
set -uo pipefail
cd "$(dirname "$0")/../.."

echo "== does ci.yml build release before testing? =="
if sed -n '/name: build · test · fmt · clippy/,/^  [a-z]/p' .github/workflows/ci.yml \
   | grep -q 'cargo build --release'; then
  echo "ok   the rust job builds release"
else
  echo "FAIL the rust job does not build release, so the CLI that shells out to itself can be stale"
  exit 1
fi

echo
echo "== backdating target/release/ainl =="
B=target/release/ainl
[ -f "$B" ] || { echo "no release binary to backdate — run cargo build --release first"; exit 1; }
python3 - <<'PY'
import os, time, pathlib
p = pathlib.Path("target/release/ainl")
old = time.time() - 3600
os.utime(p, (old, old))
print(f"backdated {p} by 1h")
PY

echo
echo "== the job's steps, in order =="
cargo fmt --all --check && echo "ok   fmt"
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1 && echo "ok   clippy"
cargo build --workspace --quiet && echo "ok   build (debug)"
cargo build --release --workspace --quiet && echo "ok   build (release)"

echo
echo "== the test that was failing =="
cargo test -p ainl-transpile --test error_class_parity 2>&1 | tail -3
