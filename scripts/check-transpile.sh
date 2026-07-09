#!/usr/bin/env bash
# Verify each transpiler target byte-for-byte: for every example, the interpreter
# and the transpiled Python/JS/Ruby must produce identical stdout. Exits non-zero
# on any mismatch (used by CI and locally).
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

# target -> runner command (skip a target if its runtime is absent)
runner_for() {
  case "$1" in
    python) echo "python3" ;;
    js)     echo "node" ;;
    ruby)   echo "ruby" ;;
  esac
}
fail=0

for ex in examples/*.ainl; do
  name=$(basename "$ex" .ainl)
  expected=$("$BIN" run "$ex") || { echo "FAIL: interpreter errored on $name"; fail=1; continue; }
  for target in python js ruby; do
    runner=$(runner_for "$target")
    if ! command -v "$runner" >/dev/null 2>&1; then
      echo "skip $name/$target ($runner not installed)"; continue
    fi
    # Write to a temp file and run that — portable. (Piping via `runner
    # /dev/stdin` breaks on Linux Node, which can't readFileSync a pipe.)
    tmp=$(mktemp)
    "$BIN" transpile "$ex" --to "$target" > "$tmp"
    out=$("$runner" "$tmp" 2>&1)
    rm -f "$tmp"
    if [ "$out" == "$expected" ]; then
      echo "ok   $name/$target"
    else
      echo "FAIL $name/$target — output differs:"
      diff <(printf '%s\n' "$expected") <(printf '%s\n' "$out") | sed 's/^/    /'
      fail=1
    fi
  done
done

[ "$fail" -eq 0 ] && echo "all transpiler targets byte-equal" || echo "transpiler equivalence FAILED"
exit "$fail"
