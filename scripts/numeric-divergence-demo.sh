#!/usr/bin/env bash
# Reproduces the numbers in docs/NUMERIC_MODEL.md: runs an i64-overflowing
# program through the interpreter and all three transpiler targets so the
# divergence is visible directly, rather than taken on faith from the docs.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

src=$(mktemp --suffix=.ainl)
cat > "$src" <<'EOF'
(print (* 9223372036854775807 2))
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
EOF

echo "== interpreter =="
"$BIN" run "$src"

for target in js python ruby; do
  runner=$(case "$target" in js) echo node ;; python) echo python3 ;; ruby) echo ruby ;; esac)
  echo "== $target =="
  if ! command -v "$runner" >/dev/null 2>&1; then
    echo "skip ($runner not installed)"; continue
  fi
  tmp=$(mktemp)
  "$BIN" transpile "$src" --to "$target" > "$tmp"
  "$runner" "$tmp"
  rm -f "$tmp"
done

rm -f "$src"
