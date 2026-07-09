# AINL Syntax — grammar guide for model ingestion

> **Read this once and you can emit valid AINL.** This document is written for
> an LLM, not a human tutorial. It is exhaustive for the v0.1 core: if a form is
> not listed here, it does not exist.

AINL is an S-expression language. **Every** expression is either an atom or a
parenthesized list `(head arg...)`. There is no infix syntax, no operator
precedence, no statement terminators, and whitespace is insignificant beyond
separating atoms. This makes the grammar a small regular CFG that is trivial to
generate under constrained decoding.

## 1. Lexical grammar

```
program   ::= form*
form      ::= list | atom
list      ::= "(" form* ")"
atom      ::= int | float | string | symbol
int       ::= "-"? digit+
float     ::= "-"? digit+ ("." digit+)? (("e"|"E") "-"? digit+)?   ; must contain "." or "e"
string    ::= '"' ( char | escape )* '"'
escape    ::= "\\" ("n"|"t"|"r"|'"'|"\\")
symbol    ::= any run of non-whitespace chars except ( ) " ;   (that is not a number)
comment   ::= ";" ... end-of-line          ; ignored
```

- `true`, `false`, `nil` are symbols the evaluator resolves to constants.
- A token that parses as a number *is* a number; otherwise it is a symbol. So
  `+`, `-`, `<=`, `fib`, `my-var`, `->x` are all valid symbols.
- Booleans/truthiness: only `nil` and `false` are false. Everything else —
  including `0` and the empty list `()` — is true.

## 2. Special forms (evaluation is not a normal call)

| Form | Shape | Meaning |
|------|-------|---------|
| `def` | `(def name value)` | Bind `name` in the current scope; returns the name. Re-`def` overwrites (this is how you mutate). |
| `fn` | `(fn (p1 p2 ... [& rest]) body...)` | Anonymous function (closure). `& rest` collects extra args into a list. Returns the last body form. |
| `if` | `(if cond then [else])` | Evaluate `then` or `else` by truthiness. No `else` → `nil`. |
| `do` | `(do form...)` | Evaluate forms in order; return the last. |
| `let` | `(let ((n v)...) body...)` | Bind locals in a new scope, then run body. |
| `while` | `(while cond body...)` | Loop while `cond` is truthy. Returns last body value or `nil`. |
| `quote` | `(quote form)` | Return `form` as data (symbols/lists) without evaluating. |
| `and` | `(and a b ...)` | Short-circuit; returns first falsey or the last value. |
| `or` | `(or a b ...)` | Short-circuit; returns first truthy or `false`. |

## 3. Builtin functions (ordinary calls, args evaluated left-to-right)

**Arithmetic** (integer-preserving; promote to float on any float or on integer overflow):
`(+ n...)` `(- n...)` `(* n...)` `(/ n...)` — `/` always yields a float. `(mod int int)` euclidean.

**Comparison / logic** (chained, variadic): `(= a...)` `(< a...)` `(> a...)` `(<= a...)` `(>= a...)` `(not x)`.

**Strings / IO**: `(print v...)` space-joins and prints a line, returns `nil`. `(str v...)` concatenates to one string.

**Lists**: `(list v...)` build. `(len list|str)`. `(first list)`. `(rest list)`. `(nth list i)` (0-based, out-of-range → `nil`). `(cons v list)` prepend. `(push list v...)` append.

**Control**: `(error msg...)` abort with a runtime error.

## 4. Canonical examples

```lisp
; define and call
(def sq (fn (x) (* x x)))
(sq 12)                                  ; => 144

; recursion
(def fib (fn (n)
  (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))
(fib 20)                                 ; => 6765

; local scope + loop (def rebinds in the let scope → in-place update)
(def fib-iter (fn (n)
  (let ((a 0) (b 1) (i 0))
    (while (< i n)
      (def t (+ a b)) (def a b) (def b t) (def i (+ i 1)))
    a)))

; higher-order, defined in AINL
(def map (fn (f xs)
  (if (= (len xs) 0) (list)
    (cons (f (first xs)) (map f (rest xs))))))
(map (fn (x) (* x 2)) (list 1 2 3))      ; => (2 4 6)
```

## 5. Rules for generating AINL correctly

1. Balance every `(` with a `)`. The whole program is one forest of trees.
2. Prefix notation only: `(+ 1 2)`, never `1 + 2`.
3. `if`/`fn`/`let`/`def` are special forms — do not quote their keyword or add commas.
4. No commas, no semicolons-as-terminators (`;` is a comment), no significant indentation.
5. To "reassign", `def` the same name again in the same scope.
6. Prefer the shortest correct form — density is the point.

## 6. Constrained decoding

The grammar above is regular enough to express directly as GBNF/EBNF for
grammar-constrained decoding (llama.cpp GBNF, Outlines, etc.). A minimal GBNF:

```gbnf
root    ::= form+
form    ::= "(" ws form* ")" ws | atom ws
atom    ::= string | number | symbol
string  ::= "\"" ([^"\\] | "\\" .)* "\""
number  ::= "-"? [0-9]+ ("." [0-9]+)?
symbol  ::= [a-zA-Z0-9+\-*/<>=!?._-]+
ws      ::= [ \t\n]*
```

A grammar-constrained decoder using this grammar can only produce AINL that
parses — which is what makes the language a reliable generation target.
