#!/usr/bin/env python3
"""Cross-validate spike/dense_fast.py (fast recursive-descent) against the
reference Earley (scripts/gbnf-conformance.py::gbnf_accepts) — the soundness
pin, same methodology that pins scripts/gen-harness/gbnf_fast.py.

The Earley is O(n^3) in practice (the `ws` star spawns O(n) origins), so we
only cross-validate on SHORT strings (<= --max-len chars, generated with a
depth-capped walker). On that bounded set the fast parser and the Earley MUST
agree on every string. A single disagreement is a soundness bug.

Usage:
    python3 spike/crossvalidate_dense.py [--n 60] [--max-len 48] [--seed 1]
"""
import argparse
import importlib.util
import random
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))

# Reference Earley (hyphenated filename -> importlib).
_spec = importlib.util.spec_from_file_location(
    "gbnf_conformance", ROOT / "scripts" / "gbnf-conformance.py")
if _spec is None or _spec.loader is None:
    sys.exit("FATAL: could not load scripts/gbnf-conformance.py")
_gbf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_gbf)
parse_gbnf, gbnf_accepts = _gbf.parse_gbnf, _gbf.gbnf_accepts
Lit, CharClass, Ref, Group, Rep = _gbf.Lit, _gbf.CharClass, _gbf.Ref, _gbf.Group, _gbf.Rep

from dense_fast import dense_gbnf_accepts  # noqa: E402

GBNF = (HERE / "dense-ainl" / "dense.ainl.gbnf").read_text()


def gen_bounded(items, rules, rng, out, depth=0, max_depth=24, max_len=48):
    """Depth- + length-capped walker. Raises StopGen on any cap (the string is
    then discarded) so we never recurse deep enough to segfault the C stack."""
    if depth > max_depth or len("".join(out)) > max_len:
        raise StopGen
    for it in items:
        if isinstance(it, Lit):
            out.append(it.s)
        elif isinstance(it, CharClass):
            if it.negated:
                pool = [chr(c) for c in range(33, 127)]
                for ch in it.chars:
                    if ch in pool:
                        pool.remove(ch)
                out.append(rng.choice(pool))
            else:
                out.append(rng.choice(it.chars))
        elif isinstance(it, Ref):
            gen_bounded(rng.choice(rules[it.name]), rules, rng, out,
                        depth + 1, max_depth, max_len)
        elif isinstance(it, Group):
            gen_bounded(rng.choice(it.alts), rules, rng, out,
                        depth + 1, max_depth, max_len)
        elif isinstance(it, Rep):
            if it.kind == "*":
                k = rng.choice([0, 0, 0, 1, 1, 2])
            elif it.kind == "+":
                k = rng.choice([1, 1, 2])
            else:
                k = rng.choice([0, 1])
            for _ in range(k):
                gen_bounded([it.item], rules, rng, out,
                            depth + 1, max_depth, max_len)


class StopGen(Exception):
    pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=60)
    ap.add_argument("--max-len", type=int, default=48)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    rules = parse_gbnf(GBNF)
    rng = random.Random(args.seed)

    checked = 0
    agree = 0
    mismatches = []
    t0 = time.time()
    attempts = 0
    while checked < args.n and attempts < args.n * 30:
        attempts += 1
        out = []
        try:
            gen_bounded(rng.choice(rules["root"]), rules, rng, out,
                        max_len=args.max_len)
        except StopGen:
            continue
        s = "".join(out)
        if not s.strip():
            continue
        fast = dense_gbnf_accepts(s)
        earley = gbnf_accepts(rules, s)
        checked += 1
        if fast == earley:
            agree += 1
        else:
            mismatches.append((s, fast, earley))
        if len(mismatches) >= 5:
            break

    dt = time.time() - t0
    print(f"cross-validation: {checked} short strings "
          f"(max-len={args.max_len}, seed={args.seed}, {dt:.1f}s)")
    for s, fast, earley in mismatches:
        print(f"  MISMATCH  fast={'A' if fast else 'R'} "
              f"earley={'A' if earley else 'R'}  {s!r}")
    if mismatches:
        print(f"RESULT: FAIL — {len(mismatches)} disagreement(s)")
        sys.exit(1)
    print(f"RESULT: PASS — fast parser agrees with the reference Earley on "
          f"{agree}/{checked} strings")


if __name__ == "__main__":
    main()
