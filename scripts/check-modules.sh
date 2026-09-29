#!/usr/bin/env bash
# Module gate: the multi-file example must run, must COMPILE to a standalone
# AOT binary, and the three transpilers — which cannot resolve modules — must
# REFUSE it rather than emit something wrong.
#
# The refusal half matters more than it looks. `import` is a keyword in Python,
# Ruby and JavaScript, so an unhandled `(import "m")` in the transpilers would
# lower to a call to the host's own import machinery — a program that compiles
# cleanly and does the wrong thing. Refusing is the only honest option there.
#
# The AOT backend used to refuse too, and no longer does: it now resolves the
# graph and INLINES it, because a load-time directive is something a compiler
# already does, and an inlined program needs no runtime load phase at all. The
# two halves below assert opposite things on purpose — AOT must succeed and
# produce a working binary, the transpilers must refuse — because the
# difference is the whole design.
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
echo "== the AOT backend INLINES the imports into a standalone binary =="
# This used to be a refusal. It cannot be one any more without giving up the
# property the whole tier exists for: a compiled AINL program reads no source
# at run time. The assertion is therefore the strong one — it compiles, AND the
# result still works after the sources are deleted, which is the only evidence
# that inlining happened rather than some runtime lookup being left in.
#
# Run in a COPY of the example under $D. Deleting the example in place to prove
# standalone-ness would leave the repo without its own sample on any failure,
# and the later transpiler checks need it intact.
S="$D/standalone-src"
mkdir -p "$S"
cp "$EX" "$S/main.ainl"
mkdir -p "$S/lib"
cp examples/wordcount/lib/*.ainl "$S/lib/" 2>/dev/null
if ! out=$("$BIN" compile "$S/main.ainl" -o "$D/wc.aot" 2>&1); then
  echo "FAIL: ainl compile refused a program with imports:"
  printf '%s\n' "$out" | sed 's/^/    /'
  fail=1
else
  echo "ok   compiled the multi-file example -> wc.aot"
  # Now delete every source in the copy, and run the binary. A wrapper or a
  # runtime loader would fail here; an inlined one cannot notice.
  rm -f "$S/main.ainl" "$S"/lib/*.ainl
  if [ -n "$(find "$S" -name '*.ainl' 2>/dev/null)" ]; then
    echo "FAIL: an .ainl file survived deletion — the proof would be meaningless"
    fail=1
  fi
  if got=$("$D/wc.aot" 2>&1) && printf '%s' "$got" | grep -q 'words:'; then
    echo "ok   the binary still works with every .ainl file deleted"
  else
    echo "FAIL: standalone binary printed '$got' after its sources were deleted"
    fail=1
  fi
fi

echo
echo "== each transpiler REFUSES a program with imports =="
# The opposite of the AOT case, and for a real reason: these emit a single
# source file with no module-resolution phase, and `import` is a keyword in the
# host language.
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
