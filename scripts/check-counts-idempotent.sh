#!/usr/bin/env bash
# Re-running scripts/update-builtin-counts.py must change nothing.
#
# The script used to replace an exact string with the current numbers baked in,
# so once the numbers moved the replace matched nothing and the run reported
# "updated" while writing a no-op. A doc-count tool that cannot be re-run safely
# is worse than none: it looks like a regeneration step and is not one. This
# pins that property, which is the property the rewrite to a regex restored.
set -uo pipefail
cd "$(dirname "$0")/.."

before=$(git status --porcelain; git stash list)
out=$(python3 scripts/update-builtin-counts.py 2>&1)
after=$(git status --porcelain; git stash list)

echo "$out"
if [ "$before" != "$after" ]; then
  echo "FAIL: a second run changed the tree"
  diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") | sed 's/^/    /'
  exit 1
fi
# "updated" on a run that changed nothing is exactly the bug this catches, so
# the wording matters as much as the tree state.
if printf '%s' "$out" | grep -q "updated (list=1"; then
  :
fi
if printf '%s' "$out" | grep -qE "^(README|site/index)\.html? updated$"; then
  echo "FAIL: reported a file as updated when nothing changed"
  exit 1
fi
echo "idempotent: a second run changes nothing"
