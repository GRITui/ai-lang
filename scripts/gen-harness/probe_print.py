#!/usr/bin/env python3
"""Probe AINL print formatting to pin down expected stdout for the suite."""
import subprocess
from pathlib import Path

A = Path("/Users/grit/.hermes/kanban/workspaces/t_84469049/ai-lang/target/release/ainl")

cases = {
    "num": '(print 42)',
    "str": '(print "hello, world")',
    "arith": '(print (+ 2 3))',
    "list": '(print (list 1 2 3))',
    "list-len": '(print (len (list 1 2 3 4 5)))',
    "list-first": '(print (first (list 10 20 30)))',
    "list-nth": '(print (nth (list 10 20 30) 2))',
    "list-rest": '(print (rest (list 1 2 3)))',
    "list-cons": '(print (cons 0 (list 1 2 3)))',
    "map-get": '(def u (hash "name" "Ada" "age" 36)) (print (get u "name"))',
    "map-keys": '(print (keys (hash "a" 1 "b" 2)))',
    "map-has": '(def u (hash "name" "Ada")) (print (has u "name"))',
    "bool-true": '(print (= 4 4))',
    "bool-false": '(print (= 4 5))',
    "sq": '(def sq (fn (x) (* x x))) (print (sq 12))',
    "fib10": '(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))) (print (fib 10))',
    "fact5": '(def fact (fn (n) (if (< n 1) 1 (* n (fact (- n 1)))))) (print (fact 5))',
    "max2": '(def max2 (fn (a b) (if (> a b) a b))) (print (max2 7 9))',
    "rem": '(print (- 10 (* 3 (/ 10 3))))',
    "prod": '(def prod (fn (lst) (if (= (len lst) 0) 1 (* (first lst) (prod (rest lst)))))) (print (prod (list 1 2 3 4)))',
    "sum5": '(def sum (fn (& xs) (def t 0) (def go (fn (l a) (if (= (len l) 0) a (go (rest l) (+ a (first l)))))) (go xs 0))) (print (sum 1 2 3 4 5))',
    "even4": '(def even (fn (n) (= (- n (* 2 (/ n 2))) 0))) (print (even 4))',
    "reverse": '(def append (fn (a b) (if (= (len a) 0) b (cons (first a) (append (rest a) b))))) (def reverse (fn (l) (if (= (len l) 0) (list) (append (reverse (rest l)) (list (first l)))))) (print (reverse (list 1 2 3)))',
    "last": '(def last (fn (l) (nth l (- (len l) 1)))) (print (last (list 1 2 3 4)))',
    "count6": '(print (len (list 1 2 3 4 5 6)))',
    "double": '(def dbl (fn (f l) (if (= (len l) 0) (list) (cons (f (first l)) (dbl f (rest l)))))) (print (dbl (fn (x) (* x 2)) (list 1 2 3)))',
    "print2": '(print "a" 1 "b")',
}

for name, src in cases.items():
    p = Path("/tmp/probe.ainl")
    p.write_text(src)
    r = subprocess.run([str(A), "run", str(p)], capture_output=True, text=True)
    print(f"=== {name} [exit {r.returncode}]")
    if r.stdout:
        print(f"  stdout: {r.stdout!r}")
    if r.stderr:
        print(f"  stderr: {r.stderr.strip()[:200]!r}")
