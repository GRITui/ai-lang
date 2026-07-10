# Getting started with AINL

A hands-on tour of the AI-Native Language in ~10 minutes. By the end you'll have
run a program, used the REPL, transpiled AINL to Python/JS/Ruby, and seen the
grammar that lets a model generate AINL reliably.

> **What AINL is (and isn't).** AINL is a tiny, uniform language meant to be
> *generated and verified by machines* and *transpiled* to languages you already
> use. It is **not** more token-efficient than Python (it's ~2× heavier — see
> [BENCHMARK.md](BENCHMARK.md)); its value is reliable generation and lossless
> interop.

## 1. Install

Needs the [Rust toolchain](https://rustup.rs).

```sh
cargo install --path crates/ainl-cli    # from a checkout
# or:  cargo install --git https://github.com/GRITui/ai-lang ainl-cli
ainl version
```

## 2. Your first program

AINL is an S-expression language: everything is `(operator arg...)`. Create
`hello.ainl`:

```lisp
(print "hello, ainl")
(def sq (fn (x) (* x x)))
(print "sq 9 =" (sq 9))
```

Run it:

```sh
ainl run hello.ainl
# hello, ainl
# sq 9 = 81
```

## 3. The REPL

```sh
ainl repl
λ (+ 1 2 3)
6
λ (def double (fn (x) (* x 2)))
double
λ (double 21)
42
λ (exit)
```

## 4. The language in five minutes

Everything is prefix-form; there is no infix, no precedence, no statement
terminators.

```lisp
; arithmetic (integer until a float or overflow appears)
(+ 1 2 3)              ; 6
(/ 10 4)              ; 2.5   — `/` always yields a float

; comparisons chain
(< 1 2 3)             ; true

; bindings and functions
(def inc (fn (x) (+ x 1)))
(inc 41)              ; 42

; re-`def` reassigns in the current scope (this is how you mutate)
(def n 0)
(def n (+ n 1))       ; n is now 1

; conditionals — `if` is an expression
(if (< 3 5) "yes" "no")   ; "yes"

; local scope + a loop
(def sum-to (fn (n)
  (let ((i 0) (acc 0))
    (while (< i n)
      (def i (+ i 1))
      (def acc (+ acc i)))
    acc)))
(sum-to 5)            ; 15

; lists and higher-order functions (map defined in AINL itself)
(def map (fn (f xs)
  (if (= (len xs) 0) (list)
    (cons (f (first xs)) (map f (rest xs))))))
(map (fn (x) (* x x)) (list 1 2 3))   ; (1 4 9)
```

Full reference: [SYNTAX.md](SYNTAX.md).

## 5. Transpile to a language you know

The same program projects losslessly to idiomatic-ish Python, JavaScript, or
Ruby — verified to produce identical output, for values within `i64`/float-safe
range (see [NUMERIC_MODEL.md](NUMERIC_MODEL.md) for what happens past that).

```sh
ainl transpile hello.ainl --to python
ainl transpile hello.ainl --to js
ainl transpile hello.ainl --to ruby
```

For example, `examples/fib.ainl` → Python gives a real `def fib(n): ...` you can
run with `python3`. The transpiler carries byte spans through, so tooling can map
generated lines back to the AINL source.

## 6. Inspect the AST

```sh
ainl ast hello.ainl              # human-readable tree with byte spans
ainl ast hello.ainl --json       # stable JSON (span + line/col per node)
```

The JSON form is the interchange format other tools build on.

## 7. The grammar (why models can generate AINL reliably)

```sh
ainl grammar          # GBNF, for grammar-constrained decoding
ainl grammar --ebnf   # EBNF
```

Feed the GBNF to a constrained decoder (llama.cpp, Outlines, …) and a model can
only emit syntactically valid AINL. This is the backbone of the separate
[Prompt Auditor](https://github.com/GRITui/ainl-auditor) project, which pipelines
local models to compile natural language into a validated AINL schema.

## Where to go next

- [SYNTAX.md](SYNTAX.md) — the complete grammar (also written for model ingestion).
- [ARCHITECTURE.md](ARCHITECTURE.md) — how the interpreter, serializer, and transpilers fit together.
- [BENCHMARK.md](BENCHMARK.md) — the honest token-count numbers.
- [MASTER_PLAN.md](MASTER_PLAN.md) / [BACKLOG.md](BACKLOG.md) — roadmap and what's next.
