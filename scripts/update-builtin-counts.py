#!/usr/bin/env python3
"""Update the two headline builtin numbers in README.md and site/index.html.

The numbers are MEASURED, not computed: `scripts/measure-prelude.sh` installs
the real prelude and reports

    total    = 79
    refused  = 12 (the 10 db-* names + http-get + http-post)
    portable = 67

The portable number is unchanged from 4.1 — every name this card added is in
the refused set, so none of them join the four-backend subset. That is the
point of the layer, and it is why the total moved and the portable count did
not. Writing "68" here because five builtins were added is exactly the
arithmetic error 4.1 made, and `measure-prelude.sh` exists so it cannot be
made twice.

Run this after any change to the prelude; re-running it is a no-op when the
numbers are already right.
"""
import pathlib
import re
import subprocess
import sys

REPO = pathlib.Path("/Users/grit/.hermes/kanban/workspaces/t_d75c7bb4/repo")
README = REPO / "README.md"
SITE = REPO / "site/index.html"

out = subprocess.run(
    ["bash", str(REPO / "scripts/measure-prelude.sh")],
    capture_output=True,
    text=True,
)
if out.returncode != 0:
    sys.exit(f"measure-prelude.sh failed:\n{out.stdout}\n{out.stderr}")

nums = dict(
    re.findall(r"^(total|portable)\s+=\s+(\d+)$", out.stdout, re.M)
)
if set(nums) != {"total", "portable"}:
    sys.exit(f"could not read the counts out of:\n{out.stdout}")
total, portable = nums["total"], nums["portable"]
print(f"measured: total={total} portable={portable}")

# ---- README ----------------------------------------------------------------
src = README.read_text()
before = src
src = src.replace("- **74 builtins.** 67 are byte-identical on all four backends",
                  f"- **{total} builtins.** {portable} are byte-identical on all four backends")
# The storage sentence: 4.1 named five; there are ten, and the value layer is
# a second thing layered on the first rather than a replacement.
src = src.replace(
    "The last five — `db-open` / `db-put` / `db-get` / `db-flush` / `db-close`, a durable\n"
    "  key/value store with an append-only checksummed log that survives a power cut\n"
    "  — run on the interpreter and the AOT binary and are refused by the transpilers,",
    "The last ten — `db-open` / `db-put` / `db-get-raw` / `db-flush` / `db-close`\n"
    "  (a durable byte store with an append-only checksummed log that survives a power\n"
    "  cut) and `db-set` / `db-get` / `db-del` / `db-keys` / `db-count` (a value store\n"
    "  over it, JSON-encoded) — run on the interpreter and the AOT binary and are\n"
    "  refused by the transpilers,",
)
README.write_text(src)
print("README.md updated" if src != before else "README.md already correct")

# ---- site ------------------------------------------------------------------
h = SITE.read_text()
before = h
h = h.replace(
    "<strong>74 builtins</strong> — 67 byte-identical across the interpreter,",
    f"<strong>{total} builtins</strong> — {portable} byte-identical across the interpreter,",
)
h = h.replace(
    "<li><strong>A test runner.</strong>",
    "<li><strong>Ten <code>db-*</code> builtins.</strong> Five for bytes, five for\n"
    "    values over them, all on an append-only checksummed log.</li>\n"
    "    <li><strong>A test runner.</strong>",
    1,
)
h = h.replace(
    "The five <code>db-*</code> storage builtins run on the\n"
    "    interpreter and the AOT binary and are refused by the transpilers, which\n"
    "    cannot reproduce an append-only checksummed log on a host <code>open()</code>.",
    "The ten <code>db-*</code> storage builtins run on the\n"
    "    interpreter and the AOT binary and are refused by the transpilers, which\n"
    "    cannot reproduce an append-only checksummed log on a host <code>open()</code>.",
)
SITE.write_text(h)
print("site/index.html updated" if h != before else "site/index.html already correct")

# ---- check-site.py's own expectation list ---------------------------------
# It asserts the numbers appear in BOTH files, so its list has to move too.
p = REPO / "scripts/check-site.py"
c = p.read_text()
c = c.replace(
    '# "74" and "67" are both required, and both are honest: 74 is the prelude\n'
    '# size, 67 is the portable subset (the other 7 are the 2 HTTP builtins every\n'
    '# backend refuses and the 5 `db-*` builtins the AOT C runtime carries but the\n'
    '# transpilers refuse). Editing one without the other fails here, which is the\n'
    '# whole point of the check.',
    f'# "{total}" and "{portable}" are both required, and both are honest: {total} is the\n'
    f'# prelude size, {portable} is the portable subset (the other {int(total) - int(portable)}\n'
    '# are the 2 HTTP builtins every backend refuses and the 10 `db-*` builtins the\n'
    '# AOT C runtime carries but the transpilers refuse). Both are measured by\n'
    '# scripts/measure-prelude.sh — do not adjust them by hand. Editing one without\n'
    '# the other fails here, which is the whole point of the check.',
)
c = c.replace(f'"561", "74", "67"', f'"561", "{total}", "{portable}"')
p.write_text(c)
print("check-site.py updated")
