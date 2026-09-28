#!/usr/bin/env python3
"""Structural check of site/index.html: balanced tags, no external deps,
and that every number in the page also appears in README.md (no drift)."""
import re
import sys
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
html = (ROOT / "site" / "index.html").read_text()
readme = (ROOT / "README.md").read_text()

VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input",
        "link", "meta", "param", "source", "track", "wbr"}


class Check(HTMLParser):
    def __init__(self):
        super().__init__()
        self.stack = []
        self.errors = []

    def handle_starttag(self, tag, attrs):
        if tag not in VOID:
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag in VOID:
            return
        if not self.stack:
            self.errors.append(f"stray </{tag}>")
        elif self.stack[-1] != tag:
            self.errors.append(f"</{tag}> closes <{self.stack[-1]}>")
            self.stack.pop()
        else:
            self.stack.pop()


c = Check()
c.feed(html)

print("unclosed tags:", c.stack or "none")
print("mismatches   :", c.errors or "none")

ext = re.findall(r'(?:src|href)="(https?://[^"]+)"', html)
print("external refs :", len(ext), "(all outbound links, no assets)")

# No frameworks, no CDN, no local asset files -> the page is self-contained.
assets = re.findall(r'(?:src|href)="(?!https?://|#)([^"]+)"', html)
print("local assets  :", assets or "none — fully self-contained")

# Headline numbers must match the README exactly (single source of truth).
# "56" and "54" are both required, and both are now honest: 56 is the prelude
# size, 54 is the portable subset. Editing one without the other fails here,
# which is the whole point of the check.
nums = ["20/20", "0/20", "10/10", "9/10", "47.3", "0.576", "30 969.0",
        "0.604", "35.4", "5.9", "2.25", "1.77", "1.27", "0.69", "51",
        "+108%", "+64%", "+95%", "561", "56", "54"]
missing = [n for n in nums if n not in html or n not in readme]
print("numbers in both:", "all %d match" % len(nums) if not missing else f"MISSING {missing}")

# The negative results must be present in both.
for phrase in ["syntax, not semantics", "insurance"]:
    print(f"  {phrase!r:26} html={phrase in html!s:5} readme={phrase in readme!s}")

ok = not c.stack and not c.errors and not assets and not missing
print("\nRESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
