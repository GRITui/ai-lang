#!/usr/bin/env python3
"""Verify: is the constrained 1B prose a GBNF member? (the sound detector)"""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gbnf_fast import ainl_gbnf_accepts

t = open("/tmp/g64.txt").read().rsplit("Assistant:\n", 1)[-1]
print("repr:", repr(t[:160]))
print("GBNF member:", ainl_gbnf_accepts(t))

# Also test a few other strings for sanity.
for label, s in [
    ("clean AINL", '(print 42)\n'),
    ("prose with **", "**AINL Program**\n\nHere is a simple AINL program in Python.\n"),
    ("prose with ---", "----------------\nThis program uses the built-in Python functions.\n"),
]:
    print(f"{label!r}: GBNF member = {ainl_gbnf_accepts(s)}")
