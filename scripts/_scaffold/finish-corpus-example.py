#!/usr/bin/env python3
"""Finish the db-get-raw rename in examples/corpus/storage.ainl.

The call sites were already renamed. Three things were left, and a reader hits
all three:

  1. `(db-get-raw  h "k")` — a double space the mechanical rename left behind.
  2. The `@teaches` header still lists `db-get`, which is now the *value*-level
     read. The corpus is the few-shot material, so a stale name here teaches the
     wrong thing to whatever model reads it.
  3. Two comment paragraphs explain `db-get`'s behaviour — that an absent key is
     nil and that it is a total function. Those are true of `db-get-raw` and
     worth keeping, but the prose has to name the builtin it is describing.

Idempotent.
"""
import pathlib
import re

P = pathlib.Path(
    "/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo/examples/corpus/storage.ainl"
)
src = P.read_text()
out = src

# 1. the double space
out = re.sub(r"\(db-get-raw {2,}", "(db-get-raw ", out)

# 2. the @teaches header: the byte layer teaches db-get-raw, not db-get
out = out.replace(
    "; @teaches   db-open, db-put, db-get, db-flush, db-close, handles, append-only",
    "; @teaches   db-open, db-put, db-get-raw, db-flush, db-close, handles, append-only",
)

# 3. the prose. Both sentences describe the byte layer's reader, so both get the
#    new name — and the second one now also says which of the two it is, since
#    "a total function" is true of `db-get-raw` and is *not* quite the interesting
#    property of `db-get` (which can error on a `db-put` string).
out = out.replace(
    "; A `db-get` on a key that was never written is `nil`, not an error, so a probe\n"
    "; needs no `try`. That is worth seeing early: it makes `db-get` a total function.",
    "; A `db-get-raw` on a key that was never written is `nil`, not an error, so a probe\n"
    "; needs no `try`. That is worth seeing early: it makes `db-get-raw` a total\n"
    "; function. (The value-level reader, `db-get`, is a different builtin and can\n"
    "; error — see SYNTAX.md §3l.)",
)

P.write_text(out)
print("storage.ainl finished")
left = [
    (i, l)
    for i, l in enumerate(out.split("\n"), 1)
    if "db-get" in l and "db-get-raw" not in l
]
if left:
    print("lines still naming bare db-get:")
    for i, l in left:
        print(f"  {i}: {l.strip()[:90]}")
else:
    print("no bare db-get remains")
