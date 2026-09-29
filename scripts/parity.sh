#!/usr/bin/env bash
# Compare the interpreter against the compiled AOT binary on one program.
# Usage: parity.sh <program.ainl> [label]
# Prints BYTE-IDENTICAL or the first differences, for stdout and stderr.
set -uo pipefail
REPO=/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo
B=$REPO/target/release/ainl
SRC=$1
LABEL=${2:-$(basename "$SRC")}

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
cp "$SRC" "$D/prog.ainl"
cd "$D"

"$B" run prog.ainl >interp.out 2>interp.err
INTERP_RC=$?
rm -f *.ainl-db 2>/dev/null

if ! "$B" compile prog.ainl -o prog 2>compile.err; then
  echo "COMPILE FAILED for $LABEL"
  cat compile.err
  exit 1
fi

./prog >aot.out 2>aot.err
AOT_RC=$?

echo "### $LABEL"
echo "interpreter exit=$INTERP_RC   aot exit=$AOT_RC"
if [ "$INTERP_RC" != "$AOT_RC" ]; then
  echo "*** EXIT CODES DIFFER ***"
fi
if diff -q interp.out aot.out >/dev/null 2>&1 && diff -q interp.err aot.err >/dev/null 2>&1; then
  echo "BYTE-IDENTICAL stdout and stderr"
else
  echo "*** DIFFER ***"
  echo "--- stdout diff ---"
  diff interp.out aot.out | head -30
  echo "--- stderr diff ---"
  diff interp.err aot.err | head -20
fi
echo "--- output ---"
cat interp.out
if [ -s interp.err ]; then
  echo "--- stderr ---"
  cat interp.err
fi
