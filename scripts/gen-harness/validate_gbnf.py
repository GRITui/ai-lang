#!/usr/bin/env python3
"""Reproduce the exact stage-2 line from docs/PIPELINE.md."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gbnf_fast import ainl_gbnf_accepts
print(ainl_gbnf_accepts(open(sys.argv[1]).read()))
