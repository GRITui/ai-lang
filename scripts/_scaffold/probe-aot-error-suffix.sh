#!/usr/bin/env bash
# Is the AOT runtime's error text missing the "at line L, col C (byte B)" suffix?
#
# The interpreter appends a source position to every runtime error. This probe
# checks whether the compiled binary does the same, using a *4.1* builtin
# (`db-put`) so the question is asked of a shipped builtin rather than of
# anything this card added.
set -uo pipefail
R=/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo/target/release/ainl
D=$(mktemp -d)
cd "$D"

printf '(db-put 1 2 3)\n' > t.ainl
echo "=== db-put 1 2 3 — INTERPRETER ==="
"$R" run t.ainl 2>&1
echo "=== db-put 1 2 3 — AOT ==="
"$R" compile t.ainl -o t >/dev/null 2>&1
./t 2>&1

printf '(list-dir 1 2 3)\n' > u.ainl
echo "=== list-dir 1 2 3 — INTERPRETER ==="
"$R" run u.ainl 2>&1
echo "=== list-dir 1 2 3 — AOT ==="
"$R" compile u.ainl -o u >/dev/null 2>&1
./u 2>&1

cd /
rm -rf "$D"
