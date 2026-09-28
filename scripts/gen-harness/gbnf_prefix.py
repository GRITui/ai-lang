#!/usr/bin/env python3
"""Valid-prefix analysis for truncated constrained generations.

Why this exists
---------------
A grammar-constrained decoder never emits a token that cannot continue a
valid program, so a *truncated* output is not a constraint failure: it is a
valid PREFIX of the language. The card's spec is explicit —

    "Truncation handled as an artifact (valid-prefix guarantee), not a
     constraint failure."

so this module answers: of the text the model actually produced, how much of
it is a legitimate AINL program? A long output that runs out of budget after
five complete forms is a *different* result from one that emits garbage.

The soundness direction matters. `ainl_gbnf_accepts` is exact, so anything it
accepts is genuinely in the language (no false positives). We search cut
points at whitespace boundaries, because a complete program can only end
where a form ended, and forms are separated by the grammar's `ws` rule.

We do NOT modify gbnf_fast.py: it is the cross-validated sound detector and
other work depends on its behaviour.
"""
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from gbnf_fast import ainl_gbnf_accepts  # noqa: E402

# A program boundary must follow a closing paren / a bare atom, and the
# grammar's `ws` may only contain spaces, tabs, newlines, carriage returns and
# ';' comments. Any other punctuation is a symbol char and belongs to a form.
_WS_CHARS = set(" \t\n\r")


def cut_points(s: str):
    """Yield candidate prefix lengths: end-of-string plus every offset that
    follows a run of whitespace (a form boundary)."""
    n = len(s)
    yield n
    i = 0
    while i < n:
        if s[i] in _WS_CHARS:
            j = i
            while j < n and s[j] in _WS_CHARS:
                j += 1
            # j is just past the whitespace run; a form boundary may sit here.
            yield j
            yield i
            i = j
        else:
            i += 1


def longest_valid_prefix(s: str):
    """Return (length, text) of the longest GBNF-valid prefix of `s`.

    Returns (0, "") when no non-empty prefix is a valid program.
    """
    best_len, best_txt = 0, ""
    for c in cut_points(s):
        if c <= best_len or c > len(s):
            continue
        cand = s[:c]
        if ainl_gbnf_accepts(cand):
            if c > best_len:
                best_len, best_txt = c, cand
    return best_len, best_txt


def prefix_report(generated: str, truncated: bool):
    """Summarise how much of a generation is a real AINL program.

    Returns a dict with:
      complete      - the WHOLE output is a valid program
      valid_prefix  - the output is a strict prefix of one (truncation only)
      length        - chars in the longest valid prefix
      coverage      - that length as a fraction of the output
      truncated     - the caller said the stop reason was 'length'
    """
    whole = ainl_gbnf_accepts(generated) if generated else False
    n, txt = longest_valid_prefix(generated or "")
    if whole:
        kind = "complete"
    elif n > 0:
        kind = "valid_prefix"
    else:
        kind = "no_valid_prefix"
    return {
        "complete": whole,
        "valid_prefix": kind == "valid_prefix",
        "kind": kind,
        "length": n,
        "coverage": (n / len(generated)) if generated else 0.0,
        "truncated": truncated,
        "prefix_text": txt,
    }


if __name__ == "__main__":
    CASES = [
        ("(print 1)\n(print 2)\n(print 3", "truncated mid-run"),
        ("(print 1)\n(print 2)\n", "complete two forms"),
        ("(print 1)\nhello: this is prose", "valid then garbage"),
        ("```python\nprint(1)\n```", "markdown fence"),
        ("# comment only\n", "comment-only, no form"),
    ]
    print("=" * 74)
    print("valid-prefix detector self-test")
    print("=" * 74)
    for src, why in CASES:
        r = prefix_report(src, truncated=True)
        print("%-14s len=%-3d cov=%5.1f%%  %-26s %r"
              % (r["kind"], r["length"], 100 * r["coverage"], why, src))
    print()
    ok = (prefix_report("(print 1)\n(print 2)\n", False)["kind"] == "complete"
          and prefix_report("(print 1)\n(print", True)["kind"] == "valid_prefix"
          and prefix_report("```python\n", True)["kind"] == "no_valid_prefix")
    print("self-test:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
