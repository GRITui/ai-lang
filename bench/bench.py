#!/usr/bin/env python3
"""Token-density benchmark for AINL (the core "optimized for LLM context" claim).

Compares the token count of AINL source against *idiomatic, hand-written*
equivalents in Python / JavaScript / Ruby — i.e. what a developer would actually
write, NOT the mechanical transpiler output (which carries a runtime shim and
would unfairly favor AINL). Uses OpenAI's real tokenizers via tiktoken.

Honest by construction: whatever the numbers say, they get printed. Run:
    python3 bench/bench.py
"""
import sys
from pathlib import Path

try:
    import tiktoken
except ImportError:
    sys.exit("tiktoken not installed — run: pip install tiktoken")

ROOT = Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "examples"

# Idiomatic, natural equivalents of the example programs. Each implements the
# same algorithm a competent developer would write by hand in that language.
IDIOMATIC = {
    "fib": {
        "python": '''\
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)
print("fib 10 =", fib(10))
print("fib 20 =", fib(20))
def fib_iter(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a
print("fib-iter 20 =", fib_iter(20))
''',
        "javascript": '''\
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
console.log(`fib 10 = ${fib(10)}`);
console.log(`fib 20 = ${fib(20)}`);
function fibIter(n) {
  let a = 0, b = 1;
  for (let i = 0; i < n; i++) [a, b] = [b, a + b];
  return a;
}
console.log(`fib-iter 20 = ${fibIter(20)}`);
''',
        "ruby": '''\
def fib(n)
  n < 2 ? n : fib(n - 1) + fib(n - 2)
end
puts "fib 10 = #{fib(10)}"
puts "fib 20 = #{fib(20)}"
def fib_iter(n)
  a, b = 0, 1
  n.times { a, b = b, a + b }
  a
end
puts "fib-iter 20 = #{fib_iter(20)}"
''',
    },
    "hello": {
        "python": '''\
print("hello, ai-native world")
sq = lambda x: x * x
print("sq 12 =", sq(12))
def total(*xs):
    return sum(xs)
print("sum 1..5 =", total(1, 2, 3, 4, 5))
''',
        "javascript": '''\
console.log("hello, ai-native world");
const sq = x => x * x;
console.log(`sq 12 = ${sq(12)}`);
const total = (...xs) => xs.reduce((a, b) => a + b, 0);
console.log(`sum 1..5 = ${total(1, 2, 3, 4, 5)}`);
''',
        "ruby": '''\
puts "hello, ai-native world"
sq = ->(x) { x * x }
puts "sq 12 = #{sq.(12)}"
def total(*xs)
  xs.sum
end
puts "sum 1..5 = #{total(1, 2, 3, 4, 5)}"
''',
    },
}

ENCODINGS = [("GPT-4o (o200k)", "o200k_base"), ("GPT-4 (cl100k)", "cl100k_base")]


def count(text, enc):
    return len(enc.encode(text))


def main():
    encoders = [(label, tiktoken.get_encoding(name)) for label, name in ENCODINGS]
    langs = ["python", "javascript", "ruby"]

    print(f"AINL token-density benchmark (source tokens; lower is better)\n")
    for label, enc in encoders:
        print(f"## {label}")
        header = f"{'example':<8} {'ainl':>6} " + " ".join(f"{l:>10}" for l in langs)
        print(header)
        print("-" * len(header))
        totals = {"ainl": 0, **{l: 0 for l in langs}}
        for name in ("hello", "fib"):
            ainl_src = (EXAMPLES / f"{name}.ainl").read_text()
            ainl_tok = count(ainl_src, enc)
            totals["ainl"] += ainl_tok
            cells = []
            for lang in langs:
                t = count(IDIOMATIC[name][lang], enc)
                totals[lang] += t
                pct = (ainl_tok - t) / t * 100
                cells.append(f"{t:>4}({pct:+3.0f}%)")
            print(f"{name:<8} {ainl_tok:>6} " + " ".join(f"{c:>10}" for c in cells))
        # totals
        tcells = []
        for lang in langs:
            pct = (totals['ainl'] - totals[lang]) / totals[lang] * 100
            tcells.append(f"{totals[lang]:>4}({pct:+3.0f}%)")
        print(f"{'TOTAL':<8} {totals['ainl']:>6} " + " ".join(f"{c:>10}" for c in tcells))
        print("(% = AINL vs that language; negative means AINL uses fewer tokens)\n")


if __name__ == "__main__":
    main()
