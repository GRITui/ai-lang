#!/usr/bin/env bash
# Cross-process persistence on the COMPILED binary.
#
# Three separate processes, each a different binary: phase 1 and 2 are the
# compiled AOT binary, phase 3 too. This is the card's explicit requirement
# ("set -> get -> close -> reopen -> intact, on the compiled AOT binary as well
# as the interpreter"), and it is the one property a single in-process test
# cannot show: a value that lives in a cached index and never reaches the file
# passes every same-process test.
#
# Usage: persist.sh <p1.ainl> <p2.ainl> <p3.ainl>
set -uo pipefail
REPO=/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo
A=$REPO/target/release/ainl
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
cp "$1" "$2" "$3" "$D/"
cd "$D"

for p in "$1" "$2" "$3"; do
  "$A" compile "$p" -o "${p%.ainl}.bin" >/dev/null 2>&1 || {
    echo "COMPILE FAILED: $p"
    exit 1
  }
done

fail=0
for p in "$1" "$2" "$3"; do
  echo "--- ${p} (compiled AOT binary) ---"
  "./${p%.ainl}.bin" || fail=1
  echo "exit=$?"
done
echo
[ "$fail" -eq 0 ] && echo "three separate compiled processes agreed" || echo "A PHASE FAILED"
exit "$fail"
