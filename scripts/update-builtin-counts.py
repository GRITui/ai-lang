#!/usr/bin/env python3
"""Update the two headline builtin numbers in README.md and site/index.html.

The numbers are MEASURED, not computed: `scripts/measure-prelude.sh` installs
the real prelude and reports

    total    = 80
    refused  = 12 (the 10 db-* names + http-get + http-post)
    portable = 68

Neither number is a function of how many builtins a card added: `rmdir` is a
portable builtin, so it moved both (79/67 -> 80/68), while the ten `db-*` names
and the two HTTP ones moved only the total. `measure-prelude.sh` exists because
guessing this arithmetic is how 4.1 got it wrong once already, and a wrong count
in the README is a claim no test can check.

Run this after any change to the prelude; re-running it is a no-op when the
numbers are already right.
"""
import pathlib
import re
import subprocess
import sys

# Relative to this file, not an absolute path. An absolute REPO constant is
# silently wrong the moment a second worktree exists: the script then measures
# and rewrites a *different checkout* and reports success while doing it, which
# is how one run of this edited another card's worktree.
REPO = pathlib.Path(__file__).resolve().parent.parent
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
# Regex, not a literal with the current numbers baked in: the previous version
# replaced the exact string "- **74 builtins.** 67 are byte-identical…", so once
# the numbers had moved the replace silently matched nothing and the script
# still printed "README.md updated" on the next run's no-op. The numbers now come
# from the measurement; the sentence shape is what stays pinned.
src, n = re.subn(
    r"- \*\*\d+ builtins\.\*\* \d+ are byte-identical on all four backends",
    f"- **{total} builtins.** {portable} are byte-identical on all four backends",
    src,
)
if n == 0:
    sys.exit(
        "README.md: no 'N builtins' line matched — the wording changed, so the "
        "number has to be updated by hand (and check-site.py will confirm it)"
    )
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
h, n = re.subn(
    r"<strong>\d+ builtins</strong> — \d+ byte-identical across the interpreter,",
    f"<strong>{total} builtins</strong> — {portable} byte-identical across the interpreter,",
    h,
)
if n == 0:
    sys.exit(
        "site/index.html: no '<strong>N builtins</strong>' matched — the wording "
        "changed, so the number has to be updated by hand"
    )
# The db-* sentence was rewritten once already (74/67 -> 79/67, five -> ten).
# It is not rewritten again here: a second insert would duplicate the paragraph
# that already follows it. `check-site.py` is what keeps these numbers honest.
SITE.write_text(h)
print("site/index.html updated" if h != before else "site/index.html already correct")

# ---- check-site.py's own expectation list ---------------------------------
# It asserts the numbers appear in BOTH files, so its list has to move too.
p = REPO / "scripts/check-site.py"
c = p.read_text()
c, n_comment = re.subn(
    r'# "\d+" and "\d+" are both required, and both are honest: \d+ is the\s*\n'
    r"# prelude size, \d+ is the portable subset \(the other \d+\s*\n",
    f'# "{total}" and "{portable}" are both required, and both are honest: {total} is the\n'
    f"# prelude size, {portable} is the portable subset (the other "
    f"{int(total) - int(portable)}\n",
    c,
)
# The expected-values list is the actual gate, so a replace that matches nothing
# there is a failure rather than a no-op: it would leave check-site.py asserting
# the OLD numbers while this script printed "updated".
c, n_list = re.subn(
    r'"561", "\d+", "\d+"', f'"561", "{total}", "{portable}"', c
)
if n_list == 0:
    sys.exit(
        "scripts/check-site.py: its expected-values list no longer has the "
        '"561", "<total>", "<portable>" shape — update it by hand'
    )
p.write_text(c)
print(
    f"check-site.py updated (list={n_list}, comment={n_comment})"
    if (n_list or n_comment)
    else "check-site.py already correct"
)
