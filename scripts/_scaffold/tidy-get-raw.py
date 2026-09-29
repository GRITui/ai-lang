#!/usr/bin/env python3
"""Tidy the mechanical `db-get-raw` rename in db_builtins.rs.

Two artefacts of the rename, both cosmetic but both wrong in a way a test would
happily tolerate and a reader would trip over:

  1. `(db-get-raw  h "k")` — a double space where the call used to be.
  2. The *expected error strings* still said `db-get`, because the rename only
     rewrote the call form. The builtin's name is in its own message, so those
     assertions would now fail for the right reason at the wrong time.

Idempotent.
"""
import pathlib
import re

P = pathlib.Path(
    "/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo"
    "/crates/ainl-core/tests/db_builtins.rs"
)
src = P.read_text()

# 1. collapse the double space the rename left behind
src = re.sub(r"\(db-get-raw {2,}", "(db-get-raw ", src)

# 2. the expected messages: a quoted string that names the builtin
src = src.replace(
    '"db-get expects (db-get-raw  handle key)"',
    '"db-get-raw expects (db-get-raw handle key)"',
)
src = src.replace(
    '"db-get expects a str key, got int"',
    '"db-get-raw expects a str key, got int"',
)
src = src.replace(
    '"db-get expects a db handle, got nil"',
    '"db-get-raw expects a db handle, got nil"',
)
src = src.replace('"(db-get-raw 7 \\"k\\")", "db-get"', '"(db-get-raw 7 \\"k\\")", "db-get-raw"')

P.write_text(src)
print("tidied db_builtins.rs")

left = [l for l in src.splitlines() if "db-get" in l and "db-get-raw" not in l]
print("lines still mentioning bare db-get (prose only, expected):")
for l in left:
    print("   ", l.strip()[:100])
