#!/usr/bin/env bash
# Module gate: the multi-file example must run, and the three backends that
# cannot resolve modules must REFUSE it rather than emit something wrong.
#
# The refusal half matters more than it looks. `import` is a keyword in Python,
# Ruby and JavaScript, so an unhandled `(import "m")` in the transpilers would
# lower to a call to the host's own import machinery — a program that compiles
# cleanly and does the wrong thing. The AOT backend would instead fail at
# runtime with a `scope_lookup` error naming `import`, which reads like a
# codegen bug. Both are silent-wrong or misleading; refusing is the only honest
# option, and this script is what keeps that honest.
#
# Runs in CI and locally. Exits non-zero on any failure.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."
  cargo build --release --quiet || exit 1
  BIN=target/release/ainl
fi

EX=examples/wordcount/main.ainl
fail=0

echo "== the multi-file example runs =="
if ! out=$("$BIN" run "$EX" 2>&1); then
  echo "FAIL: $EX did not run:"
  printf '%s\n' "$out" | sed 's/^/    /'
  fail=1
else
  # The sample is three lines of prose, so a correct run must report a
  # non-trivial word count and one line per distinct word. Asserting on the
  # shape rather than exact counts keeps this from breaking on an edit to the
  # sample text, while still failing if tokenization silently returns nothing —
  # which is exactly the regression this example exists to catch.
  words=$(printf '%s\n' "$out" | head -1 | tr -dc '0-9')
  lines=$(printf '%s\n' "$out" | tail -n +2 | grep -c '[^[:space:]]')
  if [ -z "$words" ] || [ "$words" -lt 5 ]; then
    echo "FAIL: expected a plausible word count, got: $(printf '%s\n' "$out" | head -1)"
    fail=1
  elif [ "$lines" -lt 3 ]; then
    echo "FAIL: expected one line per distinct word, got $lines"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok   ran: $words distinct words over $lines lines"
  fi
fi

echo
echo "== the example really is multi-file (its imports resolve, not ignored) =="
# A program that silently ignored `import` would still print something if the
# defs happened to be in the file. Prove they are NOT by running a file that
# only uses an imported name and nothing else.
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/lib"
printf '(def only-here 42)\n' > "$D/lib/dep.ainl"
printf '(import "lib/dep.ainl")\n(print only-here)\n' > "$D/main.ainl"
if got=$("$BIN" run "$D/main.ainl" 2>&1) && [ "$got" == "42" ]; then
  echo "ok   an imported def is usable from the importing file"
else
  echo "FAIL: imported def not usable (got: $got)"
  fail=1
fi

echo
echo "== the AOT backend REFUSES a program with imports =="
# Before this gate existed, `ainl compile` emitted a C program that failed at
# runtime with `unbound variable: import`.
if out=$("$BIN" compile "$EX" -o "$D/wc.aot" 2>&1); then
  echo "FAIL: ainl compile accepted a program with imports"
  fail=1
elif ! printf '%s' "$out" | grep -q 'interpreter-only'; then
  echo "FAIL: refused, but not for the right reason:"
  printf '%s\n' "$out" | sed 's/^/    /'
  fail=1
else
  echo "ok   refused: $(printf '%s' "$out" | head -1)"
fi

echo
echo "== each transpiler REFUSES a program with imports =="
for target in python js ruby; do
  if out=$("$BIN" transpile "$EX" --to "$target" 2>&1); then
    echo "FAIL: transpile --to $target accepted a program with imports"
    fail=1
  elif ! printf '%s' "$out" | grep -q 'interpreter-only'; then
    echo "FAIL: --to $target refused, but not for the right reason:"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok   $target refused"
  fi
done

echo
echo "== a program with NO import is unaffected on every backend =="
printf '(print (+ 1 2))\n' > "$D/plain.ainl"
if got=$("$BIN" run "$D/plain.ainl" 2>&1) && [ "$got" == "3" ]; then
  echo "ok   interpreter still fine"
else
  echo "FAIL: interpreter broke on a plain program (got: $got)"
  fail=1
fi
if "$BIN" transpile "$D/plain.ainl" --to python > "$D/plain.py" 2>"$D/err"; then
  echo "ok   transpiler still fine"
else
  echo "FAIL: transpiler broke on a plain program:"
  sed 's/^/    /' "$D/err"
  fail=1
fi

echo
[ "$fail" -eq 0 ] && echo "module gate PASSED" || echo "module gate FAILED"
exit "$fail"
