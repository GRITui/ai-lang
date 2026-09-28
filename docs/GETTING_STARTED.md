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

The REPL keeps a program *running*, so a tweak costs one line instead of
write-file / compile / run — and a `def` is still there when the next line
arrives.

```sh
ainl repl
```

A real session (the `λ` is the prompt; `…` means "your form is still open"):

```lisp
λ (def x 41)
λ (def double (fn (n) (* n 2)))
λ (double x)
82
λ (def x (+ x 1))
λ (double x)
84
```

That is the loop: change one line, see the new answer, with everything else
still bound.

`def` prints nothing — it returns the symbol, and a REPL that echoed that
would make every transcript unreadable. `nil` and `()` are silent for the same
reason; `print` is how you ask for output.

**Scoping, the thing a REPL teaches fastest.** A `fn` body is a *new* scope, so
`def` inside a function cannot reach an outer variable. A REPL is where that is
easiest to get bitten, because you can see both halves of the mistake side by
side — and the second `counter` comes back `1`, not `2`:

```lisp
λ (def counter 0)
λ (def bump (fn () (def counter (+ counter 1)) counter))
λ (bump)
1
λ (bump)
1
λ counter
0
```

Each call to `bump` gets a fresh scope, so `def counter` bound a *new* local
that was thrown away with the call, and the outer `counter` never moved. This
is [SYNTAX.md §2a](SYNTAX.md)'s rule, and it is the same reason `let` (whose
`def`s *do* persist, because `while`/`if`/`do` don't open a scope) is the tool
for a loop that accumulates:

```lisp
λ (def sum-to (fn (n)
…   (let ((i 0) (acc 0))
…     (while (< i n) (def i (+ i 1)) (def acc (+ acc i)))
…     acc)))
λ (sum-to 5)
15
```

**Multi-line input.** An unclosed `(` continues onto the next line, and a
string may span lines too. Write a function the way you would in any editor:

```lisp
λ (def fib (fn (n)
…   (if (< n 2) n
…     (+ (fib (- n 1))
…        (fib (- n 2))))))
λ (fib 20)
6765
```

**Errors do not end the session.** They print as `line N: …` on stderr, where
`N` is the line in your input, and the next line runs normally:

```lisp
λ (+ 1 2)
3
λ (nosuch)
line 2: runtime error: unbound symbol 'nosuch'
λ (+ 1 2)
3
```

Everything you defined before the bad line is still bound, so you can just fix
the typo and carry on.

**Scripting it.** `ainl repl --stdin` is the same loop with the banner and
prompts off, so a session is a plain script you can commit, pipe, and diff:

```sh
$ printf '(def x 41)\n(def x (+ x 1))\nx\n' | ainl repl --stdin
42
```

Results go to stdout and errors to stderr, so a failing line in the middle of a
script does not corrupt the results:

```sh
ainl repl --stdin < session.ainl > out.txt   # errors stay on the terminal
```

The REPL is a front end for the interpreter backend only — it adds no syntax,
and `ainl run` / `compile` / `transpile` are how you reach the other backends.
Full rules: [SYNTAX.md §3a](SYNTAX.md).

Leave with `(exit)`, `exit`, `(quit)`, `quit`, `:q`, or Ctrl-D.

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

## 4b. Splitting a program across files

`(import "lib/math.ainl")` binds another file's `def`s into the one you are
writing. `(import "lib/math.ainl" as m)` binds one name holding a map of them.
Imports are resolved before your code runs, and only at the top level of a file.

A worked three-file program ships in `examples/wordcount/`:

```sh
ainl run examples/wordcount/main.ainl
# words: 17
#   3  ;
#   5  the
#   ...
```

`main.ainl` wires it together, `lib/text.ainl` tokenizes, and `lib/stats.ainl`
counts — and `stats` imports `text` itself, which makes the rule about exports
visible in real code: `stats` uses `words` but does not re-export it, so `main`
has to import `text` too.

Two things to know before you write your own:

- **A module importing a sibling uses the bare name.** From inside `lib/`,
  `(import "math")` finds `lib/math.ainl`; `(import "lib/math.ainl")` would look
  for `lib/lib/math.ainl`. A path-like specifier (one containing `/`) resolves
  against the importing file's directory first; a bare name resolves against the
  working directory first.
- **`import` works in the interpreter only.** `ainl compile` (AOT) and the
  Python/JS/Ruby transpilers refuse a program containing it, by design — see
  [SYNTAX.md §3b](SYNTAX.md#3b-modules-import) for why. Keep a program's
  modules at the interpreter if you want to ship it to a host language.

Full rules — exports, collisions, cycles, and error text: [SYNTAX.md §3b](SYNTAX.md#3b-modules-import).

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
only emit syntactically valid AINL. This is the backbone of the separate Prompt Auditor project (spun out to its
own, currently private repository), which pipelines local models to compile
natural language into a validated AINL schema.

## Where to go next

- [../examples/README.md](../examples/README.md) — **ten complete, runnable
  programs**, each checked in CI on every backend it claims to support. Start
  here rather than at the grammar: `countdown.ainl` is a loop,
  `word-frequency.ainl` is a real program with maps and a hand-written sort,
  and each file's comments explain the one rule in it that is easy to get
  wrong. This is also the few-shot corpus `ainl gen` draws on, so what you
  read is exactly what a model is shown.
- [SYNTAX.md](SYNTAX.md) — the complete grammar (also written for model ingestion).
- [ARCHITECTURE.md](ARCHITECTURE.md) — how the interpreter, serializer, and transpilers fit together.
- [BENCHMARK.md](BENCHMARK.md) — the honest token-count numbers.
- [MASTER_PLAN.md](MASTER_PLAN.md) / [BACKLOG.md](BACKLOG.md) — roadmap and what's next.
