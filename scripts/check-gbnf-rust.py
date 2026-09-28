#!/usr/bin/env python3
"""Cross-validate the Rust GBNF matcher in crates/ainl-cli/src/gbnf.rs against
the shipped Python detector (scripts/gen-harness/gbnf_fast.py).

A third implementation of the same predicate is worth nothing if it can quietly
disagree with the two existing ones. This drives both over a shared corpus —
the real generations committed under scripts/gen-harness/results*, plus fuzzed
and hand-picked edge cases — and requires exact agreement.

Usage:  python3 scripts/check-gbnf-rust.py [--count N]
"""
import argparse
import json
import random
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE / "gen-harness"))
from gbnf_fast import ainl_gbnf_accepts  # noqa: E402


def rust_accepts_many(cases, bin_path):
    """Run the Rust matcher over `cases` through a throwaway harness binary."""
    # The separator is a bare NUL and nothing else — no surrounding newlines.
    # A case is sent *byte for byte*: adding a trailing "\n" would silently
    # change the answer, because an unterminated trailing comment is only a
    # comment if a newline follows it. The first draft framed the cases as
    # "\n\0\n", which appended a newline to every case and made the Rust side
    # accept 9 unterminated-comment strings the Python side rejected — a bug
    # in the harness that looked exactly like a disagreement between the two
    # matchers.
    #
    # A NUL is used because it cannot occur in any of the cases: the corpus
    # filters them, so the framing is unambiguous.
    payload = "\0".join(cases)
    out = subprocess.run(
        [str(bin_path), "gen", "--self-check-gbnf"],
        input=payload, capture_output=True, text=True,
    )
    if out.returncode != 0:
        raise SystemExit(f"FATAL: rust checker failed: {out.stderr[:400]}")
    verdicts = out.stdout.splitlines()
    if len(verdicts) != len(cases):
        raise SystemExit(
            f"FATAL: rust returned {len(verdicts)} verdicts for {len(cases)} cases"
        )
    return [v.strip() == "yes" for v in verdicts]


def corpus(seed, count):
    cases = []

    # 1. Every real generation ever committed to the repo. These are the exact
    #    strings the constrained/unconstrained comparison was measured on, so
    #    the two implementations must agree on all of them.
    for res in sorted((HERE / "gen-harness").glob("results*/**/*.ainl")):
        try:
            cases.append(res.read_text())
        except (OSError, UnicodeDecodeError):
            pass

    # 2. Hand-picked edges: every accept/reject case the self-tests name, plus
    #    the drift facts docs/SYNTAX.md §6 records.
    cases += [
        "(print 1)\n", "(+ 1(+ 2 3))\n", "(def x 1)\n(print x)\n", "(a b)(c d)\n",
        '(print "a\\qb")\n', '(print "a\\nb")\n', "", "\n  \n", "hello\n",
        "# not a comment\n(print 1)\n", "(print 1: 2)\n", "(print 1)\n# trailing\n",
        "(print 1) ; trailing", "(print 1) ; trailing\n", '; only a comment\n',
        "()", "()()", "(())", "((()))", "(print)", "(print )", "  (print 1)  ",
        "1", "-1", "1.5", "1e10", "1E-10", "-0.5e+3", "1.", ".5", "1e", "1e+",
        "((((((((((1))))))))))", '""', '"a"', '"\\""', '"\\\\"', '"\\n"',
        '(print "a:b#c")', "(print 'a')", "(print `a`)", "(a;b)", "(a ;c\n)",
        "x = 1\n(print x)\n", "(print 1)(print 2)", "(print 1)(print 2",
        "héllo", '(print "héllo")', "(print héllo)",
    ]

    # 3. Fuzz: random token soup drawn from the alphabet that matters, so the
    #    matchers are compared on shapes neither a person nor a model writes.
    rng = random.Random(seed)
    alphabet = list('()";\\ \n\t0123456789abzABZ+-*/<>=!?._&:{}[]#`\'~^$%@,')
    for _ in range(count):
        cases.append("".join(rng.choice(alphabet) for _ in range(rng.randint(0, 40))))

    # 4. Fuzz from *valid* fragments: a generator that only makes valid strings
    #    would miss disagreements that only appear in the accept direction.
    frags = ["(print 1)", "(def f (fn (x) (* x 2)))", '"s"', "12", "x", "()",
             "(if a b c)", "(list 1 2)", "; c\n", "\n", " ", "1.5e-2"]
    for _ in range(count):
        cases.append(" ".join(rng.choice(frags) for _ in range(rng.randint(1, 6))))

    return cases


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=400)
    ap.add_argument("--seed", type=int, default=20260928)
    args = ap.parse_args()

    bin_path = ROOT / "target" / "release" / "ainl"
    if not bin_path.exists():
        bin_path = ROOT / "target" / "debug" / "ainl"
    if not bin_path.exists():
        raise SystemExit("FATAL: no ainl binary; run cargo build --release")

    cases = corpus(args.seed, args.count)
    # Deduplicate while preserving order, so the report is stable, and keep both
    # implementations on the same input domain: no NUL (it is the separator) and
    # not empty. An empty case is excluded rather than handled specially,
    # because both matchers skip it — an empty *generation* is a real thing in
    # results/ (a failed request saved nothing) and it proves nothing about
    # membership, while the empty string is already covered explicitly as
    # `""` in the hand-picked edges, which every matcher rejects.
    seen, uniq = set(), []
    for c in cases:
        if c and "\x00" not in c and c not in seen:
            seen.add(c)
            uniq.append(c)
    cases = uniq

    rust = rust_accepts_many(cases, bin_path)
    if len(rust) != len(cases):
        raise SystemExit(
            f"FATAL: rust returned {len(rust)} answers for {len(cases)} cases"
        )

    mismatch = []
    for case, got_rust in zip(cases, rust):
        got_py = ainl_gbnf_accepts(case)
        if got_py != got_rust:
            mismatch.append((case, got_py, got_rust))

    print(f"corpus     : {len(cases)} cases "
          f"(committed generations + hand-picked edges + {args.count}×2 fuzz)")
    print(f"python     : {sum(1 for c in cases if ainl_gbnf_accepts(c))} accept")
    print(f"rust       : {sum(rust)} accept")
    print(f"mismatches : {len(mismatch)}")
    for case, py, rust_v in mismatch[:20]:
        print(f"  {case!r}\n     python={'accept' if py else 'reject'} "
              f"rust={'accept' if rust_v else 'reject'}")
    if mismatch:
        print("\nRESULT: FAIL — the Rust matcher disagrees with the shipped detector")
        sys.exit(1)
    print("\nRESULT: PASS — both implementations agree on every case")


if __name__ == "__main__":
    main()
