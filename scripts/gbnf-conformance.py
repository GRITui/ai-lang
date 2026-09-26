#!/usr/bin/env python3
"""GBNF conformance check for the AINL grammar.

Three jobs (see docs/SYNTAX.md §6 and the C1 card):

1. Well-formedness: parse the GBNF exported by `ainl grammar` with a real
   GBNF parser (llguidance's GBNF front-end — the parser family llama.cpp's
   grammar support was built from). A negative control (a deliberately
   malformed grammar) must be rejected, proving the check is not vacuous.
   If the exported grammar is malformed, this fails and the grammar must be
   fixed in crates/ainl-core/src/grammar.rs.

2. Fuzz (pins grammar ⊆ parser): walk the exported grammar with a random
   generator to produce N strings the GBNF accepts, and feed each to
   `ainl ast`. Every one must parse. A failure means the parser rejects
   something the grammar promises is valid — a broken thesis.

3. Walker honesty (oracle cross-validation): an independent pure-Python
   GBNF acceptor (Earley parser, `gbnf_accepts`) re-checks every fuzzed
   string. This proves the walker really only emits GBNF-accepted strings,
   so the fuzz genuinely pins "grammar ⊆ parser" rather than
   "walker ⊆ parser". A walker bug that emitted off-grammar strings would
   otherwise be invisible.

The acceptor also documents the known drift facts with concrete
accept/reject evidence (see the drift section it prints and docs/SYNTAX.md).

Usage:
    python3 scripts/gbnf-conformance.py [--count 1000] [--seed 1]

Requires: llguidance (`pip install llguidance`) and a built `ainl` binary
(release preferred; debug used as fallback).
"""

import argparse
import random
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "ainl"
if not BIN.exists():
    BIN = ROOT / "target" / "debug" / "ainl"


def export_gbnf() -> str:
    out = subprocess.run(
        [str(BIN), "grammar", "--gbnf"], capture_output=True, text=True, check=True
    )
    return out.stdout


def check_well_formed(gbnf: str) -> None:
    """Parse the exported GBNF with a real GBNF parser, plus a negative
    control proving the parser actually rejects malformed input."""
    try:
        import llguidance
    except ImportError:
        sys.exit("FATAL: llguidance not installed — run: pip install llguidance")

    llguidance.grammar_from(format="gbnf", text=gbnf)  # raises on malformed

    # Negative control: the same grammar with a dangling rule reference must
    # be rejected. If this passes, the "parser" accepts anything and the
    # well-formedness check above is meaningless.
    broken = gbnf + "\ndangling ::= undefined_rule\n"
    try:
        llguidance.grammar_from(format="gbnf", text=broken)
        sys.exit("FATAL: GBNF parser accepted a malformed grammar — check is vacuous")
    except Exception:
        pass

    print("well-formed: exported GBNF parsed by llguidance (negative control rejected)")


# ---------------------------------------------------------------------------
# Grammar walker: generate random strings accepted by the GBNF.
#
# We walk the *exported* grammar text (parsed by the tiny GBNF-subset parser
# below) rather than hard-coding the rules, so the fuzz stays honest if the
# grammar changes: it always mirrors what `ainl grammar` emits.
# ---------------------------------------------------------------------------


class Item:
    pass


class Lit(Item):
    def __init__(self, s):
        self.s = s


class CharClass(Item):
    def __init__(self, chars, negated):
        self.chars = chars
        self.negated = negated


class Ref(Item):
    def __init__(self, name):
        self.name = name


class Group(Item):
    def __init__(self, alts):
        self.alts = alts


class Rep(Item):
    def __init__(self, item, kind):  # kind: '*' | '+' | '?'
        self.item = item
        self.kind = kind


def _unescape(ch: str) -> str:
    return {"n": "\n", "t": "\t", "r": "\r"}.get(ch, ch)


def _parse_alternatives(s: str) -> list:
    """Parse one rule body: alternatives separated by `|`, each a sequence of
    literals / char classes / refs / parenthesized groups with * + ? suffixes."""
    alts = []
    i, n = 0, len(s)

    def skip_ws(i):
        while i < n and s[i].isspace():
            i += 1
        return i

    def parse_item(i):
        i = skip_ws(i)
        c = s[i]
        if c == '"':
            j = i + 1
            buf = []
            while j < n:
                if s[j] == "\\" and j + 1 < n:
                    buf.append(_unescape(s[j + 1]))
                    j += 2
                    continue
                if s[j] == '"':
                    break
                buf.append(s[j])
                j += 1
            if j >= n:
                raise ValueError(f"unterminated literal in {s!r}")
            item, i = Lit("".join(buf)), j + 1
        elif c == "[":
            j = i + 1
            negated = False
            if j < n and s[j] == "^":
                negated = True
                j += 1
            chars = set()
            while j < n and s[j] != "]":
                if s[j] == "\\" and j + 1 < n:
                    chars.add(_unescape(s[j + 1]))
                    j += 2
                    continue
                # range: X-Y (e.g. [0-9], [a-z]); '-' is an operator only when
                # followed by a non-']' char
                if j + 2 < n and s[j + 1] == "-" and s[j + 2] != "]":
                    lo, hi = s[j], s[j + 2]
                    for code in range(ord(lo), ord(hi) + 1):
                        chars.add(chr(code))
                    j += 3
                else:
                    chars.add(s[j])
                    j += 1
            if j >= n:
                raise ValueError(f"unterminated class in {s!r}")
            item, i = CharClass(sorted(chars), negated), j + 1
        elif c == "(":
            depth, j = 0, i
            while j < n:
                if s[j] == "(":
                    depth += 1
                elif s[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            if j >= n:
                raise ValueError(f"unterminated group in {s!r}")
            item, i = Group(_parse_alternatives(s[i + 1 : j])), j + 1
        else:
            m = re.match(r"[A-Za-z_][A-Za-z0-9_-]*", s[i:])
            if not m:
                raise ValueError(f"cannot parse GBNF at {i} in {s!r}")
            item, i = Ref(m.group(0)), i + m.end()
        # repetition suffix may follow any item (literal, class, ref, group)
        if i < n and s[i] in "*+?":
            kind = s[i]
            return Rep(item, kind), i + 1
        return item, i

    def parse_alt(i):
        items = []
        while True:
            i = skip_ws(i)
            if i >= n or s[i] == "|":
                break
            item, i = parse_item(i)
            items.append(item)
        return items, i

    while True:
        i = skip_ws(i)
        if i >= n or s[i] == "|":
            break
        items, i = parse_alt(i)
        alts.append(items)
        if i < n and s[i] == "|":
            i += 1
    if not alts:
        raise ValueError(f"empty rule body in {s!r}")
    return alts


def parse_gbnf(text: str) -> dict:
    """Parse the small GBNF subset AINL uses. Returns {rule: [alternatives]}."""
    rules = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        name, sep, body = line.partition("::=")
        if not sep:
            raise ValueError(f"not a GBNF rule: {line!r}")
        rules[name.strip()] = _parse_alternatives(body.strip())
    if "root" not in rules:
        raise ValueError("no root rule")
    return rules


def gen(items, rules, rng, out, depth=0):
    if depth > 64:
        raise RecursionError("generator recursion too deep")
    for it in items:
        if isinstance(it, Lit):
            out.append(it.s)
        elif isinstance(it, CharClass):
            if it.negated:
                # AINL uses [^"\\] — pick a printable char outside the class.
                pool = [chr(c) for c in range(33, 127)]
                for ch in it.chars:
                    if ch in pool:
                        pool.remove(ch)
                out.append(rng.choice(pool))
            else:
                out.append(rng.choice(it.chars))
        elif isinstance(it, Ref):
            gen(rng.choice(rules[it.name]), rules, rng, out, depth + 1)
        elif isinstance(it, Group):
            gen(rng.choice(it.alts), rules, rng, out, depth + 1)
        elif isinstance(it, Rep):
            if it.kind == "*":
                k = rng.choice([0, 0, 0, 1, 1, 2, 3])
            elif it.kind == "+":
                k = rng.choice([1, 1, 2, 3])
            else:  # '?'
                k = rng.choice([0, 1])
            for _ in range(k):
                gen([it.item], rules, rng, out, depth + 1)


def generate(rules, rng, max_attempts=50):
    """One random string the grammar accepts (retry on pathological draws)."""
    for _ in range(max_attempts):
        out = []
        try:
            gen(rng.choice(rules["root"]), rules, rng, out)
        except RecursionError:
            continue
        return "".join(out)
    raise RuntimeError("could not generate a sample")


# ---------------------------------------------------------------------------
# Independent GBNF acceptor (pure-Python Earley parser).
#
# This is a second, independent GBNF parser used two ways:
#   * as an *oracle* that cross-validates the walker — every fuzzed sample is
#     asserted to actually be accepted by the GBNF, so the fuzz genuinely pins
#     "grammar ⊆ parser" rather than "walker ⊆ parser";
#   * to document the known drift facts with concrete accept/reject evidence
#     (see docs/SYNTAX.md §6).
# ---------------------------------------------------------------------------


def _build_cfg(rules):
    """Normalize the parsed GBNF into a CFG:
    prod  = {nonterminal: [ (symbol, ...), ... ]}   (one entry per alternative)
    preds = [ predicate(char) -> bool, ... ]        (terminal predicates by id)
    Symbols are ('nt', name) or ('term', pred_id).
    """
    prod = {}
    preds = []
    pred_ids = {}
    counter = [0]

    def new_id(prefix):
        counter[0] += 1
        return f"{prefix}{counter[0]}"

    def term(pred_key):
        if pred_key not in pred_ids:
            pred_ids[pred_key] = len(preds)
            kind = pred_key[0]
            if kind == "char":
                c = pred_key[1]
                preds.append(lambda ch, c=c: ch == c)
            else:
                chars, negated = set(pred_key[1]), pred_key[2]
                if negated:
                    preds.append(lambda ch, chars=chars: ch not in chars)
                else:
                    preds.append(lambda ch, chars=chars: ch in chars)
        return ("term", pred_ids[pred_key])

    def norm_item(item):
        if isinstance(item, Lit):
            return [term(("char", c)) for c in item.s]
        if isinstance(item, CharClass):
            return [term(("class", frozenset(item.chars), item.negated))]
        if isinstance(item, Ref):
            return [("nt", item.name)]
        if isinstance(item, Group):
            name = new_id("g")
            prod[name] = [norm_seq(a) for a in item.alts]
            return [("nt", name)]
        if isinstance(item, Rep):
            inner = tuple(norm_item(item.item))
            iname = new_id("ri")
            prod[iname] = [inner]
            name = new_id("r")
            if item.kind == "*":
                # name -> () | iname name   (zero or more; terminates via ())
                prod[name] = [(), (("nt", iname), ("nt", name))]
            elif item.kind == "+":
                # name -> iname name_star, where name_star -> () | iname name_star
                # (one or more; the star provides the terminating base case)
                star = new_id("rs")
                prod[star] = [(), (("nt", iname), ("nt", star))]
                prod[name] = [((("nt", iname), ("nt", star)))]
            else:  # "?"
                prod[name] = [(), (("nt", iname),)]
            return [("nt", name)]
        raise ValueError(f"unknown item {item!r}")

    def norm_seq(items):
        out = []
        for it in items:
            out.extend(norm_item(it))
        return tuple(out)

    for name, alts in rules.items():
        prod[name] = [norm_seq(a) for a in alts]
    prod = _eliminate_left_recursion(prod)
    return prod, preds


def _eliminate_left_recursion(prod):
    """Remove *immediate* left recursion (rule -> rule ...).

    AINL's `ws` rule is left-recursive (`ws ::= ws ws-char`), which a naive
    Earley predict loop would chase forever. For each rule R with
    left-recursive alternatives, factor them into a fresh nonterminal R' and
    rewrite R to call R' instead. Indirect left recursion cannot remain once
    all immediate left recursion is gone (a cycle must contain an immediate
    left-recursive step), so the result is left-recursion-free.
    """
    out = {}
    for name, alts in prod.items():
        nonlr = [a for a in alts if not (a and a[0] == ("nt", name))]
        lr = [a for a in alts if a and a[0] == ("nt", name)]
        if not lr:
            out[name] = alts
            continue
        # R' -> (rest of each left-recursive alt)
        rprime = name + "'LR"
        out[rprime] = [a[1:] for a in lr]
        # R -> non-lr alts | R'
        new_alts = list(nonlr) + [(("nt", rprime),)]
        out[name] = new_alts
    return out


def gbnf_accepts(rules, s):
    """True iff `s` is in the language of the GBNF (independent Earley parser).

    States are (name, seq, dot, origin): nonterminal `name` with production
    `seq`, dot at `dot`, started at `origin`. A state in chart[i] with
    dot == len(seq) is *complete*: `name` spans [origin, i].

    Correctness: positions are processed left-to-right; each position is
    driven to a fixed point (predict / scan / complete). Completion of a
    state at position i only consults active states at chart[origin] with
    origin <= i, all of which are already settled by the time i is reached,
    so the per-position fixed point is sound and terminates (finite state
    space). This is a second, independent GBNF parser — used as the oracle
    that cross-validates the walker and to document the drift facts.
    """
    prod, preds = _build_cfg(rules)
    prod = dict(prod)
    start = "START"
    prod[start] = [((("nt", "root"),))]
    n = len(s)
    chart = [set() for _ in range(n + 1)]

    def add(i, st):
        if st not in chart[i]:
            chart[i].add(st)
            return True
        return False

    add(0, (start, prod[start][0], 0, 0))
    for i in range(n + 1):
        while True:
            progressed = False
            for (name, seq, dot, origin) in list(chart[i]):
                if dot < len(seq):
                    nxt = seq[dot]
                    if nxt[0] == "nt":  # predict
                        for alt in prod[nxt[1]]:
                            if add(i, (nxt[1], alt, 0, i)):
                                progressed = True
                    elif i < n and preds[nxt[1]](s[i]):  # scan
                        if add(i + 1, (name, seq, dot + 1, origin)):
                            progressed = True
                else:
                    # complete: `name` spans [origin, i]; advance every active
                    # state at chart[origin] whose next symbol is `name`.
                    for (A, Aseq, Adot, Aorigin) in list(chart[origin]):
                        if Adot < len(Aseq) and Aseq[Adot] == ("nt", name):
                            if add(i, (A, Aseq, Adot + 1, Aorigin)):
                                progressed = True
            if not progressed:
                break

    return any(st[0] == start and st[2] == len(st[1]) for st in chart[n])


def drift_evidence(rules):
    """Concrete accept/reject evidence for the grammar-vs-parser gap.

    Returns a list of (label, gbnf_accepts, parser_accepts, exp_gbnf,
    exp_parser) tuples. The parser side is checked against the real
    `ainl ast` binary. These are *informational* — they document the
    remaining GBNF ⊊ parser gap (the GBNF is a conservative subset, the
    safe direction) and are not asserted as pass/fail. See docs/SYNTAX.md.

    Note: the card's original "no whitespace between forms" probe
    (`(+ 1(+ 2 3))`) was a misreading — that string is a *single list*
    (one top-level form), and `ws` may be empty, so the GBNF always
    accepted it. The real multi-form gap (whitespace-separated top-level
    forms) is fixed by `root ::= ws form (ws form)* ws`; the only
    remaining superset case is the empty/whitespace-only program.
    """
    cases = [
        # (label, source, expected_gbnf, expected_parser)
        ("single list, no inner ws", "(+ 1(+ 2 3))\n", True, True),
        ("ws-separated multi-form", "(def x 1)\n(print x)\n", True, True),
        ("adjacent multi-form", "(a b)(c d)\n", True, True),
        ("invalid escape \\q", '(print "a\\qb")\n', False, False),
        ("valid escapes", r'(print "a\nb\t\rc\\d\"e\/f")' + "\n", True, True),
        ("empty program", "", False, True),
        ("whitespace-only program", "\n  \n", False, True),
    ]
    out = []
    for label, src, exp_gbnf, exp_parser in cases:
        g = gbnf_accepts(rules, src)
        p = _real_accepts(src)
        out.append((label, g, p, exp_gbnf, exp_parser))
    return out


def _real_accepts(src: str) -> bool:
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "s.ainl"
        p.write_text(src)
        r = subprocess.run([str(BIN), "ast", str(p)], capture_output=True, text=True)
        return r.returncode == 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=1000)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    if not BIN.exists():
        sys.exit(f"FATAL: no ainl binary at {BIN} — run cargo build --release first")

    gbnf = export_gbnf()
    check_well_formed(gbnf)

    rules = parse_gbnf(gbnf)
    rng = random.Random(args.seed)

    ok = 0
    oracle_ok = 0
    failures = []
    with tempfile.TemporaryDirectory() as td:
        for i in range(args.count):
            s = generate(rules, rng)
            # Walker honesty: the walker must only emit GBNF-accepted strings.
            if gbnf_accepts(rules, s):
                oracle_ok += 1
            else:
                failures.append((i, s, "walker emitted a string the GBNF rejects"))
                if len(failures) >= 5:
                    break
            p = Path(td) / "sample.ainl"
            p.write_text(s)
            r = subprocess.run([str(BIN), "ast", str(p)], capture_output=True, text=True)
            if r.returncode == 0:
                ok += 1
            else:
                failures.append((i, s, r.stderr.strip()))
                if len(failures) >= 5:
                    break

    if failures:
        for i, s, err in failures:
            print(f"FAIL sample {i}: {s!r}\n  {err}")
        sys.exit(f"FATAL: {len(failures)} fuzzed strings failed (oracle or parser)")

    print(
        f"fuzz: {ok}/{args.count} GBNF-generated strings parsed by `ainl ast` "
        f"(seed={args.seed})"
    )
    print(
        f"walker-honesty: {oracle_ok}/{args.count} strings independently "
        f"accepted by the GBNF oracle"
    )

    # Drift evidence (informational): document the GBNF ⊊ parser gap.
    print("\ndrift evidence (GBNF vs parser; GBNF is a conservative subset):")
    for label, g, p, eg, ep in drift_evidence(rules):
        g_ok = "ok" if g == eg else "UNEXPECTED"
        p_ok = "ok" if p == ep else "UNEXPECTED"
        print(
            f"  {label:28} gbnf={'A' if g else 'R'}({g_ok}) "
            f"parser={'A' if p else 'R'}({p_ok})"
        )

    print("\nconformance: PASS")


if __name__ == "__main__":
    main()
