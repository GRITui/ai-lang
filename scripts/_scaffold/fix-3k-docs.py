#!/usr/bin/env python3
"""Update the 3k section's own text after `db-get` became the value-level read.

3k documents the *byte* layer, and its read is now called `db-get-raw`. The
section must say so, because a reader who arrives at 3k and copies its example
would otherwise get a program that errors on the first read.

Two edits, both mechanical and both scoped to the 3k block (lines from its
heading to the next `## `):

  * `(db-get ` in the example and in the rules -> `(db-get-raw `
  * the two error-message examples, which name the builtin in its own message

Idempotent.
"""
import pathlib
import re
import sys

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
SYNTAX = REPO / "docs/SYNTAX.md"

src = SYNTAX.read_text()
lines = src.split("\n")

# Locate the 3k block: its heading to the next top-level heading.
start = next(i for i, l in enumerate(lines) if l.startswith("## 3k."))
end = next(i for i in range(start + 1, len(lines)) if lines[i].startswith("## "))
block = "\n".join(lines[start:end])

before = block
block = re.sub(r"\(db-get(?![\w-])", "(db-get-raw ", block)
# The error string a stale handle produces now carries the new name.
block = block.replace("`db-get: handle 7 is not open`", "`db-get-raw: handle 7 is not open`")
# And the two rules that name the read in prose.
block = block.replace(
    "and `db-get` returns the latest value for a key or `nil`.",
    "and `db-get-raw` returns the latest stored text for a key or `nil`.",
)
block = block.replace(
    "- **`db-get handle key` → str or nil.**",
    "- **`db-get-raw handle key` → str or nil.**",
)
block = block.replace(
    "so `db-get` is a total function",
    "so `db-get-raw` is a total function",
)

if block == before:
    print("3k block already updated — nothing to do")
    sys.exit(0)

# A pointer at the top of the section, so the rename is explained where it bites.
block = block.replace(
    "Keys and values are **strings**.",
    "Keys and values are **strings**. (The value-level layer in §3l renamed this\n"
    "section's reader to `db-get-raw`; `db-get` there is the value-level read that\n"
    "§3l adds. Everything else in this section is unchanged.)\n\n"
    "Keys and values are **strings**.",
    1,
)

out = "\n".join(lines[:start]) + "\n" + block + "\n" + "\n".join(lines[end:])
SYNTAX.write_text(out)
print("3k updated")
for i, l in enumerate(out.split("\n")[start:start + 40], start=start + 1):
    if "db-get" in l:
        print(f"  {i}: {l.strip()[:96]}")
