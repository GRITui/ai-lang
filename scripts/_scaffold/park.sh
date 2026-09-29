#!/usr/bin/env bash
# The one-off migration scripts used to land the db-get -> db-get-raw rename and
# the 3l documentation, gathered here so scripts/ carries only things the project
# runs.
#
# These are NOT deliverables. Each one has already been applied to the tree; the
# edits are in the diff, not in these files. They are kept only as a record of
# *how* the rename was done, because the rename is not a pure search-and-replace
# and the reasoning is worth something:
#
#   rename-get-raw.py   confined the rename to the three byte-layer files and
#                       deliberately skipped db_refusal.rs and db_kv.rs, which
#                       name `db-get` on purpose
#   tidy-get-raw.py     fixed the two artefacts the rename left: a double space
#                       where a call used to be, and expected error strings that
#                       still named the old builtin
#   fix-3k-docs.py      updated 3k's own prose, scoped to the 3k block
#   tidy-3k-docs.py     folded in the note that explains the rename, dropped a
#                       duplicated sentence, and fixed the heading
#   insert-kv-docs.py   inserted section 3l before "## 4.", idempotently
#   probe-aot-error-suffix.sh
#                       measured the pre-existing AOT error-text gap that made
#                       byte-for-byte refusal comparison impossible
#
# Usage: ./scripts/_scaffold/park.sh   (run from the repo root)
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p scripts/_scaffold
for f in rename-get-raw.py tidy-get-raw.py fix-3k-docs.py tidy-3k-docs.py \
         insert-kv-docs.py probe-aot-error-suffix.sh fix-docs-db-script.py \
         finish-corpus-example.py; do
  if [ -f "scripts/$f" ]; then
    mv "scripts/$f" "scripts/_scaffold/$f"
    echo "parked scripts/$f"
  fi
done
echo "done — scripts/_scaffold/README.md explains what each one was for"
