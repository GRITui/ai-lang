#!/usr/bin/env python3
"""Dense-AINL GBNF check (spike t_4bb53833).

Two jobs, mirroring scripts/gbnf-conformance.py's approach:

1. Well-formedness: parse the dense GBNF with a real GBNF parser
   (llguidance's GBNF front-end — the parser family llama.cpp's grammar
   support was built from). A negative control (a deliberately malformed
   grammar) must be rejected, proving the check is not vacuous.

2. Membership (reference oracle): use the independent pure-Python Earley
   parser (gbnf_accepts, imported from scripts/gbnf-conformance.py) to check
   that (a) the four hand-written dense examples ARE members of the dense
   GBNF, and (b) a fuzz of grammar-generated strings is accepted by the
   Earley (walker honesty). This proves the GBNF is a real, non-vacuous
   language and that our examples are valid dense-AINL.

Usage:
    python3 spike/dense_gbnf_check.py [--count 200] [--seed 1]
"""
import argparse
import importlib.util
import random
import sys
from pathlib import Path

sys.setrecursionlimit(50000)

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

# Import the reference GBNF tooling (parse_gbnf, gbnf_accepts, generate) from
# scripts/gbnf-conformance.py (hyphenated filename → importlib).
_spec = importlib.util.spec_from_file_location(
    "gbnf_conformance", ROOT / "scripts" / "gbnf-conformance.py")
if _spec is None or _spec.loader is None:
    sys.exit("FATAL: could not load scripts/gbnf-conformance.py")
_gbf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_gbf)
parse_gbnf, gbnf_accepts = _gbf.parse_gbnf, _gbf.gbnf_accepts
Lit, CharClass, Ref, Group, Rep = _gbf.Lit, _gbf.CharClass, _gbf.Ref, _gbf.Group, _gbf.Rep


def gen_local(items, rules, rng, out, depth=0, max_depth=5000):
    """Local walker (same as the conformance one) but with a higher depth
    limit — the dense grammar nests deeper (ternary/postfix/blocks), so the
    conformance walker's depth-64 cap rejects nearly every draw."""
    if depth > max_depth:
        raise RecursionError("generator recursion too deep")
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
            gen_local(rng.choice(rules[it.name]), rules, rng, out, depth + 1, max_depth)
        elif isinstance(it, Group):
            gen_local(rng.choice(it.alts), rules, rng, out, depth + 1, max_depth)
        elif isinstance(it, Rep):
            if it.kind == "*":
                k = rng.choice([0, 0, 0, 1, 1, 2, 3])
            elif it.kind == "+":
                k = rng.choice([1, 1, 2, 3])
            else:  # '?'
                k = rng.choice([0, 1])
            for _ in range(k):
                gen_local([it.item], rules, rng, out, depth + 1, max_depth)


def generate_local(rules, rng, max_attempts=400):
    for _ in range(max_attempts):
        out = []
        try:
            gen_local(rng.choice(rules["root"]), rules, rng, out)
        except RecursionError:
            continue
        return "".join(out)
    raise RuntimeError("could not generate a sample")

GBNF_PATH = HERE / "dense-ainl" / "dense.ainl.gbnf"
EXAMPLES = HERE / "dense-ainl"
ORDER = ["hello", "fib", "lists", "maps"]


def check_well_formed(gbnf: str) -> None:
    try:
        import llguidance
    except ImportError:
        sys.exit("FATAL: llguidance not installed — run: pip install llguidance")
    llguidance.grammar_from(format="gbnf", text=gbnf)  # raises on malformed
    # Negative control: a dangling rule reference must be rejected.
    broken = gbnf + "\ndangling ::= undefined_rule\n"
    try:
        llguidance.grammar_from(format="gbnf", text=broken)
        sys.exit("FATAL: GBNF parser accepted a malformed grammar — check is vacuous")
    except Exception:
        pass
    print("well-formed: dense GBNF parsed by llguidance (negative control rejected)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    gbnf = GBNF_PATH.read_text()
    check_well_formed(gbnf)

    rules = parse_gbnf(gbnf)
    print(f"parsed {len(rules)} rules: {', '.join(sorted(rules))}")

    # (a) The four dense examples must be members of the dense GBNF.
    print("\nexample membership (reference Earley oracle):")
    all_ok = True
    for name in ORDER:
        src = (EXAMPLES / f"{name}.ainl").read_text()
        ok = gbnf_accepts(rules, src)
        all_ok = all_ok and ok
        print(f"  {'ok  ' if ok else 'FAIL'} {name}.ainl  ({len(src)} chars)")
    if not all_ok:
        sys.exit("FATAL: a dense example is NOT a member of the dense GBNF")

    # (b) Fuzz: generate random dense strings, the Earley must accept all.
    rng = random.Random(args.seed)
    ok = 0
    failures = []
    for i in range(args.count):
        s = generate_local(rules, rng)
        if gbnf_accepts(rules, s):
            ok += 1
        else:
            failures.append((i, s))
            if len(failures) >= 5:
                break
    if failures:
        for i, s in failures:
            print(f"FAIL fuzz {i}: {s!r}")
        sys.exit(f"FATAL: {len(failures)} fuzzed strings rejected by the Earley")
    print(f"\nfuzz: {ok}/{args.count} GBNF-generated strings accepted by the "
          f"Earley oracle (seed={args.seed})")
    print("\ndense GBNF check: PASS")


if __name__ == "__main__":
    main()
