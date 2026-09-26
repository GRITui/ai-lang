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
escape    ::= "\\" ("n"|"t"|"r"|"\""|"\\"|"/")   ; any other backslash-letter is a lexer error
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
| `def` | `(def name value)` | Bind `name` in the current scope; returns the name. Re-`def` in the *same* scope overwrites (this is how you mutate) — see §2a for exactly which forms share a scope vs. open a new one. |
| `fn` | `(fn (p1 p2 ... [& rest]) body...)` | Anonymous function (closure). `& rest` collects extra args into a list. Returns the last body form. |
| `if` | `(if cond then [else])` | Evaluate `then` or `else` by truthiness. No `else` → `nil`. |
| `do` | `(do form...)` | Evaluate forms in order; return the last. |
| `let` | `(let ((n v)...) body...)` | Bind locals in a new scope, then run body. |
| `while` | `(while cond body...)` | Loop while `cond` is truthy. Returns last body value or `nil`. |
| `quote` | `(quote form)` | Return `form` as data (symbols/lists) without evaluating. |
| `and` | `(and a b ...)` | Short-circuit; returns first falsey or the last value. |
| `or` | `(or a b ...)` | Short-circuit; returns first truthy or `false`. |

## 2a. Scoping: which forms open a new environment

`def` always writes into the **nearest enclosing scope** — but "nearest
enclosing scope" means the nearest enclosing form that actually opens one, not
just the nearest enclosing form of any kind. Only two forms open a new scope;
every other form evaluates its sub-forms directly in the scope it was itself
evaluated in:

| Form | Opens a new scope? |
|------|---|
| `fn` (a fresh one per call) | **Yes** |
| `let` | **Yes** |
| `if`, `do`, `while`, `and`, `or` | No — they share the caller's scope |

The consequence: `def` inside `if`/`do`/`while`/`and`/`or` **mutates** whatever
scope contains *them* (typically an enclosing `let` or `fn` body, or the
top-level scope). `def` inside `fn` or `let` only ever writes into that fresh
scope, which is discarded when the call/`let` returns — it can never reach out
and mutate an outer scope. This is also why re-`def`ing a name only shadows
(rather than mutating) once you cross a `let`/`fn` boundary: `def` never
searches parent scopes, it always writes into the scope it's evaluated in.

```lisp
; MUTATES — `while` doesn't open a scope, so `def i` writes into the
; enclosing `let`'s own scope; each iteration overwrites the same binding.
(let ((i 0))
  (while (< i 5) (def i (+ i 1)))
  i)                                       ; => 5

; SHADOWS, does not mutate — the inner `let` opens its own scope, so
; `def i` there creates a new local `i` that disappears when the inner
; `let` returns; the outer `i` is untouched.
(let ((i 0))
  (let () (def i 99))
  i)                                       ; => 0 (not 99)

; SHADOWS, does not mutate — `fn` opens a fresh scope per call, so `def`
; inside a closure body can never write back to the defining scope.
(def counter 0)
(def bump (fn () (def counter (+ counter 1)) counter))
(bump)                                     ; => 1
(bump)                                     ; => 1  (not 2 — outer `counter` never changed)
counter                                    ; => 0

; MUTATES — `if` doesn't open a scope either.
(let ((x 1)) (if true (def x 2) nil) x)    ; => 2
```

## 3. Builtin functions (ordinary calls, args evaluated left-to-right)

**Arithmetic** (integer-preserving; promote to float on any float or on integer overflow):
`(+ n...)` `(- n...)` `(* n...)` `(/ n...)` — `/` always yields a float. `(mod int int)` euclidean.

**Comparison / logic** (chained, variadic): `(= a...)` `(< a...)` `(> a...)` `(<= a...)` `(>= a...)` `(not x)`.

**Strings / IO**: `(print v...)` space-joins and prints a line, returns `nil`. `(str v...)` concatenates to one string.

**Lists**: `(list v...)` build. `(len list|str|hash)`. `(first list)`. `(rest list)`. `(nth list i)` (0-based, out-of-range → `nil`). `(cons v list)` prepend. `(push list v...)` append.

**Maps**: `(hash k v k v ...)` build from key/value pairs — a repeated key keeps its *last* value at its *first* position. `(get h k)` look up, `nil` if absent. `(assoc h k v)` a *new* map with `k` bound to `v` (like `cons`/`push`, the original is untouched). `(has h k)` bool. `(keys h)` / `(vals h)` lists in insertion order. Any value can be a key — key comparison is the same `=` used everywhere else, so a quoted symbol and an equal-content string are different keys, same as they're different values. **Equality is insertion-order-sensitive**, exactly like `List` — `(= (hash "a" 1 "b" 2) (hash "b" 2 "a" 1))` is `false`. This is a deliberate simplification (not "real" set-of-pairs equality) that keeps a map's behavior — construction, lookup, equality — identical across the interpreter and all three transpiler targets, the same way it already is for lists.

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

; maps: build, look up, and non-mutating update
(def user (hash "name" "Ada" "age" 36))
(get user "name")                        ; => "Ada"
(get user "email")                       ; => nil
(def user2 (assoc user "age" 37))
(get user "age")                         ; => 36  (unchanged)
(get user2 "age")                        ; => 37
```

## 5. Rules for generating AINL correctly

1. Balance every `(` with a `)`. The whole program is one forest of trees.
2. Prefix notation only: `(+ 1 2)`, never `1 + 2`.
3. `if`/`fn`/`let`/`def` are special forms — do not quote their keyword or add commas.
4. No commas, no semicolons-as-terminators (`;` is a comment), no significant indentation.
5. To "reassign", `def` the same name again in the same scope — but `fn` and
   `let` open a *new* scope (§2a), so `def` inside one of those never mutates
   an outer binding, even one of the same name; it always shadows instead.
6. Prefer the shortest correct form — density is the point.

## 6. Constrained decoding

The grammar above is regular enough to express directly as GBNF/EBNF for
grammar-constrained decoding (llama.cpp GBNF, Outlines, etc.). The canonical
GBNF is exported by `ainl grammar` (source of truth: `crates/ainl-core/src/grammar.rs`):

```gbnf
root    ::= ws form (ws form)* ws
form    ::= list | atom
list    ::= "(" ws (form ws)* ")"
atom    ::= (string | number | symbol) ws
string  ::= "\"" ( [^"\\] | "\\" ["\\/nrt] )* "\""
number  ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?
symbol  ::= sym-char+
sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!" | "?" | "." | "_" | "&"
ws      ::= ( [ \t\n\r] | comment )*
comment ::= ";" [^\n]* "\n"
```

A grammar-constrained decoder using this grammar can only produce AINL that
parses — which is what makes the language a reliable generation target.

### Grammar vs. parser (the GBNF is a strict subset)

The exported GBNF is a **conservative subset** of what the parser accepts:
every string the GBNF accepts is valid AINL, but the parser accepts a few
shapes the GBNF deliberately does not generate. This is the safe direction —
a constrained decoder can never emit something that fails to parse. The known
superset cases (parser accepts, GBNF rejects):

- **Empty / whitespace-only programs.** The GBNF's `root` requires at least
  one form; the parser accepts `""` and whitespace-only input. A decoder
  never needs to emit an empty program, so the GBNF stays conservative.

Two earlier "drift" reports were investigated and resolved:

- *No whitespace between forms* (`(+ 1(+ 2 3))`): **not a real gap.** That
  string is a single list (one top-level form), and `ws` may be empty, so the
  GBNF always accepted it. The parser and GBNF agree.
- *Whitespace-separated top-level forms* (`(def x 1)\n(print x)`): this was a
  **real GBNF defect** — the old `root ::= ws form (form)* ws` could not place
  whitespace after a list-form, so it rejected every multi-form program (all
  the examples in §4). It is fixed by `root ::= ws form (ws form)* ws`, which
  allows optional whitespace before each subsequent top-level form.

This is pinned by `scripts/gbnf-conformance.py` (run in CI): it parses the
exported GBNF with a real GBNF parser (llguidance), fuzzes ≥1000 strings the
grammar accepts and asserts every one parses with `ainl ast`, and cross-checks
the walker against an independent pure-Python GBNF acceptor.
