#!/usr/bin/env python3
"""Token-density benchmark for the DENSE AINL spike (card t_4bb53833).

Compares the token count of the *dense AINL* source (spike/dense-ainl/*.ainl)
against idiomatic, hand-written equivalents in Python / JavaScript / Ruby —
the code a competent developer would actually write, NOT transpiler output.
Same method as bench/bench.py (tiktoken, real OpenAI tokenizers).

The key question: does dense AINL go BELOW Python on any/all examples?

Run:  python3 spike/bench_dense.py
"""
import sys
from pathlib import Path

try:
    import tiktoken
except ImportError:
    sys.exit("tiktoken not installed — run: pip install tiktoken")

ROOT = Path(__file__).resolve().parent.parent
DENSE = ROOT / "spike" / "dense-ainl"

# Idiomatic, natural equivalents of the four example programs. Each is what a
# competent developer would hand-write in that language for the same task.
IDIOMATIC = {
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
    "lists": {
        "python": '''\
xs = [1, 2, 3, 4, 5]
print("xs =", xs)
print("len =", len(xs))
print("first =", xs[0])
print("rest =", xs[1:])
print("cons 0 =", [0] + xs)
print("nth 2 =", xs[2])
print("doubled =", [x * 2 for x in xs])
print("symbols =", ["alpha", "beta", "gamma"])
''',
        "javascript": '''\
const xs = [1, 2, 3, 4, 5];
console.log("xs =", xs);
console.log("len =", xs.length);
console.log("first =", xs[0]);
console.log("rest =", xs.slice(1));
console.log("cons 0 =", [0, ...xs]);
console.log("nth 2 =", xs[2]);
console.log("doubled =", xs.map(x => x * 2));
console.log("symbols =", ["alpha", "beta", "gamma"]);
''',
        "ruby": '''\
xs = [1, 2, 3, 4, 5]
puts "xs = #{xs}"
puts "len = #{xs.length}"
puts "first = #{xs.first}"
puts "rest = #{xs[1..-1]}"
puts "cons 0 = #{[0] + xs}"
puts "nth 2 = #{xs[2]}"
puts "doubled = #{xs.map { |x| x * 2 }}"
puts "symbols = #{%w[alpha beta gamma]}"
''',
    },
    "maps": {
        "python": '''\
user = {"name": "Ada", "age": 36}
print("user =", user)
print("name =", user.get("name"))
print("email =", user.get("email"))
print("has age? =", "age" in user)
print("has email? =", "email" in user)
print("keys =", list(user.keys()))
print("vals =", list(user.values()))
user2 = {**user, "age": 37}
print("user age =", user["age"])
print("user2 age =", user2["age"])
print("size =", len(user))
''',
        "javascript": '''\
const user = { name: "Ada", age: 36 };
console.log("user =", user);
console.log("name =", user.name);
console.log("email =", user.email);
console.log("has age? =", "age" in user);
console.log("has email? =", "email" in user);
console.log("keys =", Object.keys(user));
console.log("vals =", Object.values(user));
const user2 = { ...user, age: 37 };
console.log("user age =", user.age);
console.log("user2 age =", user2.age);
console.log("size =", Object.keys(user).length);
''',
        "ruby": '''\
user = { "name" => "Ada", "age" => 36 }
puts "user = #{user}"
puts "name = #{user["name"]}"
puts "email = #{user["email"]}"
puts "has age? = #{user.key?("age")}"
puts "has email? = #{user.key?("email")}"
puts "keys = #{user.keys}"
puts "vals = #{user.values}"
user2 = user.merge("age" => 37)
puts "user age = #{user["age"]}"
puts "user2 age = #{user2["age"]}"
puts "size = #{user.size}"
''',
    },
}

ENCODINGS = [("GPT-4o (o200k)", "o200k_base"), ("GPT-4 (cl100k)", "cl100k_base")]
ORDER = ["hello", "fib", "lists", "maps"]
LANGS = ["python", "javascript", "ruby"]


def count(text, enc):
    return len(enc.encode(text))


def main():
    encoders = [(label, tiktoken.get_encoding(name)) for label, name in ENCODINGS]
    for label, enc in encoders:
        print(f"## {label}\n")
        header = f"{'example':<8} {'dense':>6} " + " ".join(f"{l:>12}" for l in LANGS)
        print(header)
        print("-" * len(header))
        totals = {"dense": 0, **{l: 0 for l in LANGS}}
        for name in ORDER:
            dense_src = (DENSE / f"{name}.ainl").read_text()
            d = count(dense_src, enc)
            totals["dense"] += d
            cells = []
            for lang in LANGS:
                t = count(IDIOMATIC[name][lang], enc)
                totals[lang] += t
                pct = (d - t) / t * 100
                cells.append(f"{t:>5}({pct:+5.0f}%)")
            print(f"{name:<8} {d:>6} " + " ".join(f"{c:>12}" for c in cells))
        tcells = []
        for lang in LANGS:
            pct = (totals["dense"] - totals[lang]) / totals[lang] * 100
            tcells.append(f"{totals[lang]:>5}({pct:+5.0f}%)")
        print(f"{'TOTAL':<8} {totals['dense']:>6} " + " ".join(f"{c:>12}" for c in tcells))
        print("(% = dense-AINL vs that language; negative means dense-AINL uses FEWER tokens)\n")


if __name__ == "__main__":
    main()
