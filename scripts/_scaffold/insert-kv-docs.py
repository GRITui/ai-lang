#!/usr/bin/env python3
"""Insert the 3l key-value section into docs/SYNTAX.md, before "## 4.".

Kept as a script rather than a hand edit so the insertion point is a *rule*
("immediately before the `## 4.` heading") and re-running it is a no-op, which
matters because the section is long enough that hand-placing it twice is easy.

Idempotent: bails if 3l is already present.
"""
import pathlib
import sys

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
SYNTAX = REPO / "docs/SYNTAX.md"
SECTION = REPO / "docs/kv-section.md"

src = SYNTAX.read_text()

if "## 3l." in src:
    print("3l is already present — nothing to do")
    sys.exit(0)

marker = "\n## 4. Canonical examples"
if marker not in src:
    sys.exit("could not find the '## 4. Canonical examples' heading to insert before")

body = SECTION.read_text().rstrip("\n")
out = src.replace(marker, "\n" + body + "\n" + marker, 1)
SYNTAX.write_text(out)
print(f"inserted 3l ({len(body.splitlines())} lines) before '## 4.'")
