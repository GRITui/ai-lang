#!/usr/bin/env python3
"""Tidy the 3k edit: the double space, the duplicated sentence, the heading.

Three artefacts of the mechanical rename, all cosmetic, all in the same block:

  1. `(db-get-raw  h "todo")` — a double space where the call used to be.
  2. The rename note was inserted *before* a sentence it repeated, so
     "Keys and values are **strings**." now appears twice.
  3. The heading still says `db-get`, which is no longer a name this section
     documents.

Idempotent.
"""
import pathlib
import re

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
SYNTAX = REPO / "docs/SYNTAX.md"

src = SYNTAX.read_text()

# 1. the double space
src = re.sub(r"\(db-get-raw {2,}", "(db-get-raw ", src)

# 2. fold the note into the paragraph it interrupted, and drop the repeat
src = src.replace(
    "Keys and values are **strings**. (The value-level layer in §3l renamed this\n"
    "section's reader to `db-get-raw`; `db-get` there is the value-level read that\n"
    "§3l adds. Everything else in this section is unchanged.)\n\n"
    "Keys and values are **strings**. `db-open` returns a **handle** — an ordinary\n"
    "int — and `db-get-raw` returns the latest stored text for a key or `nil`.\n",
    "Keys and values are **strings**. `db-open` returns a **handle** — an ordinary\n"
    "int — and `db-get-raw` returns the latest stored text for a key or `nil`.\n"
    "\n"
    "This section's reader is called `db-get-raw` because §3l takes the name\n"
    "`db-get` for the value-level read. Everything else here is unchanged, and a\n"
    "program written against §3k needs exactly one edit: `db-get` → `db-get-raw`.\n",
)

# 3. the heading
src = src.replace(
    "## 3k. Storage: `db-open` / `db-put` / `db-get` / `db-flush` / `db-close`",
    "## 3k. Storage: `db-open` / `db-put` / `db-get-raw` / `db-flush` / `db-close`",
)

SYNTAX.write_text(src)
print("tidied 3k")
for l in src.split("\n"):
    if "Keys and values are" in l or l.startswith("## 3k") or "(db-get-raw  " in l:
        print("  ", l.strip()[:100])
