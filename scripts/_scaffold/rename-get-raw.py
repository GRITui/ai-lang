#!/usr/bin/env python3
"""Rename the byte-layer read in the Tier 4 card-1 tests and example.

`db-get` became the value-level read in card 2, so every card-1 fixture that
writes with `db-put` and reads back with `db-get` must say `db-get-raw`. The
rename is confined to files that are *only* about the byte layer:

  crates/ainl-cc/tests/db_crash.rs
  crates/ainl-core/tests/db_builtins.rs
  examples/corpus/storage.ainl

and is skipped inside db_refusal.rs, which deliberately probes the refusal
matrix by name, and db_kv.rs, which is the value layer's own suite.

Idempotent: a second run changes nothing.
"""
import pathlib
import re
import sys

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
TARGETS = [
    "crates/ainl-cc/tests/db_crash.rs",
    "crates/ainl-core/tests/db_builtins.rs",
    "examples/corpus/storage.ainl",
]

# Only the *call* form: `(db-get ` followed by whitespace. The negative
# lookahead keeps a name that merely starts with "db-get" (there is none today)
# from being renamed twice, and the trailing space is what distinguishes the
# call from the bare word in prose.
CALL = re.compile(r"\(db-get(?![\w-])")
ERR = re.compile(r"\bdb-get: handle")

total = 0
for rel in TARGETS:
    p = REPO / rel
    src = p.read_text()
    out = CALL.sub("(db-get-raw ", src)
    out = ERR.sub("db-get-raw: handle", out)
    n = len(CALL.findall(src)) + len(ERR.findall(src))
    if out != src:
        p.write_text(out)
    print(f"{rel}: {n} replacement(s)")
    total += n

print(f"total {total}")
