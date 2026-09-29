#!/usr/bin/env python3
"""Update check-docs-db.sh for the `db-get` -> `db-get-raw` rename.

The 4.1 doc-verification script asserts §3k's claims against the real binary,
and §3k's reader is now `db-get-raw`. Every `(db-get ` call in it has to follow,
and so do the two expected error strings, which name the builtin.

Scoped to that one file: the value layer's own script (check-docs-kv.sh) uses
`db-get` deliberately and must not be touched.

Idempotent.
"""
import pathlib
import re

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
P = REPO / "scripts/check-docs-db.sh"

src = P.read_text()
out = re.sub(r"\(db-get(?![\w-])", "(db-get-raw ", src)
out = out.replace("db-get: handle", "db-get-raw: handle")
out = out.replace(
    "Verify every claim made in docs/SYNTAX.md section 3k (db-open/db-put/db-get/\n"
    "# db-flush/db-close) against the real binary.",
    "Verify every claim made in docs/SYNTAX.md section 3k (db-open/db-put/\n"
    "# db-get-raw/db-flush/db-close) against the real binary.",
)
out = out.replace(
    "an append-only log, last-write-wins, a missing key as nil, a 64-handle cap, a",
    "an append-only log, last-write-wins, a missing key as nil, a 64-handle cap, a",
)
if out != src:
    P.write_text(out)
    print("check-docs-db.sh updated")
else:
    print("check-docs-db.sh already updated")

for l in out.split("\n"):
    if "db-get" in l and "db-get-raw" not in l:
        print("   still bare:", l.strip()[:90])
