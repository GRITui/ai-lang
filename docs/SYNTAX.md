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

**String functions**: `(split s sep)` → a list, `(join list sep)` → a string, `(trim s)`, `(replace s old new)`, `(upcase s)` / `(downcase s)`, `(contains hay needle)` → bool.

`upcase`/`downcase` fold **ASCII only** (`a`–`z`), and `trim` strips only the ASCII whitespace set — space, tab, newline, carriage return, form feed, vertical tab. This is deliberate: the alternative (Unicode-aware case folding and whitespace) is not implementable in the AOT C runtime without pulling in a Unicode library, and the four backends have to agree exactly. A consequence worth knowing: `"héllo"` and `"日本"` are unaffected by `upcase`, and a non-breaking space is not trimmed.

Two edge cases are **rejected rather than guessed**, because the hosts disagree about them and a language that behaves differently in a compiled binary than in its interpreter is worse than one that refuses:

- `(split s "")` → error. (JavaScript splits into individual characters, Rust and C split into a trailing empty field, Python raises.)
- `(replace s "" new)` → error. (Python inserts `new` at every position, JavaScript and C return the input unchanged, Ruby raises.)

`split` keeps **trailing empty fields**: `(split "a,b," ",")` is `("a" "b" "")`, not `("a" "b")`.

**File IO**: `(read-file path)` → the file's contents as a string. `(write-file path content)` creates or **truncates**. `(append-file path content)` appends. All three take a string path and string content and error otherwise; a missing file, or a path that cannot be opened, is a runtime error. Opening is binary on every backend, so a file's bytes are exactly what `read-file` returns — including `\r\n`, which is not translated. A very common idiom is `(split (read-file path) "\n")`, which is why `split` keeps trailing empties above.

`write-file` and `append-file` **do not create parent directories**. `(write-file "new/dir/f.txt" "x")` is a `write-file: cannot write` error when `new/dir` does not exist. This is deliberate: silently `mkdir -p`-ing a typo'd path would put the file somewhere the script never named, and a program that means to build a tree can say so explicitly (`list-dir` also tells it what is already there).

`read-file` requires the file to be **valid UTF-8**. A file whose bytes are not well-formed UTF-8 (a stray `0xFF`, a truncated multi-byte sequence, an encoded surrogate half) is the same `read-file: cannot read` error as a missing file, because its bytes cannot become a string. This rule is enforced in all four backends — the AOT C runtime carries a UTF-8 validator for it, because it has no `String` type to fail the conversion in.

**Filesystem**: `(file-exists p)` → `true` if anything exists at `p` — a file *or* a directory — and `nil` otherwise. It is `nil` and not `false` so that absence is tested the same way as a missing map key: `(= (file-exists p) nil)`. A path that cannot be examined (a non-directory component, a permission error) is also `nil`, since "is this there?" has one negative answer. A trailing separator is ignored, so `(file-exists "notes.txt/")` is `true`.

`(delete-file p)` → `nil`, removing the file or symlink at `p`. A **directory is an error** (`delete-file: cannot delete '…': it is a directory`), not a silent no-op: AINL has no recursive delete, and quietly refusing would leave a caller believing the delete had happened. A **missing path is an error too**, for the same reason `read-file` errors rather than returning `""` — check `(file-exists p)` first when absence is acceptable.

`(list-dir p)` → the names in directory `p`, as a list of strings, **sorted by byte value**. The sort is the point: a raw directory read returns entries in filesystem order, which differs per filesystem and per host, so `(list-dir ".")` would otherwise print different output depending on where it ran. Byte order also puts `"Capital.txt"` before `"beta.txt"`, where a case-insensitive or locale-aware collation would not — which is why the AOT C runtime uses an unsigned-byte comparison (`strcoll` is locale-dependent) and the transpiler targets sort by encoded bytes rather than by their own string order (JS's `Array#sort` compares UTF-16 code units, which orders an emoji *before* `U+E000`). Entries are **names, not paths** (`"notes.txt"`, never `"./notes.txt"`); a hidden file **is** included, since AINL has no concept of one; and `"."` / `".."` are **not**, because they are artifacts of the directory rather than entries. A missing path, or one that is not a directory, is an error.

**Path functions** — pure string operations, no filesystem access:

- `(path-join a b ...)` → the parts joined with `/`. One or more arguments; `(path-join)` is an error.
- `(path-base p)` → the final component, or `""` when there is none (`(path-base "/")`).
- `(path-dir p)` → everything before the final component: `"."` when `p` has no separator, `"/"` for a top-level name.

These three do **not** defer to the host's own path library, and that is a deliberate decision rather than an oversight. Measured on the same inputs, the hosts disagree on every edge case that matters:

| input | Python `os.path` | Node `path` | Ruby `File` |
| --- | --- | --- | --- |
| `(path-join "a//b" "d")` | `a//b/d` | `a/b/d` | `a//b/d` |
| `(path-dir "a//b")` | `a` | `a/` | `a` |
| `(path-base "a/b/")` | `""` | `b` | `b` |
| `(path-join "" "b")` | `b` | `b` | `/b` |
| `(path-dir "x")` | `""` | `.` | `.` |

With four different answers, deferring to the host would mean the language behaving differently depending on which backend runs the program — the exact failure the four-backend rule exists to prevent. So AINL defines **its own** rules, POSIX-flavoured and spelled out identically in all five implementations (`crates/ainl-core/src/eval.rs` is normative):

- The separator is always `/`. AINL does not model the host separator, so a backslash is an ordinary filename character.
- Runs of `/` collapse to one, and `.` segments are dropped — **except a trailing `.`**, which is kept, because `"a/."` names a directory and dropping the dot would make `(path-dir "a/.")` return `"a"`, i.e. the *parent*.
- A leading `/` is preserved, and there is no current-directory normalization.
- `..` is **never resolved**: it can legitimately name a path that does not exist, and resolving it would make a function documented as pure touch the disk.
- An empty part contributes nothing, so `(path-join "" "b")` is `/b` and `(path-join "a" "" "b")` is `a/b` — never Ruby's `/b`-for-an-empty-first-part.

**Environment / process**: `(env-get name)` → the value of the environment variable, or `nil` if unset. `(exit code)` ends the process with that status. `code` must be an integer.

**Time**: `(now)` → whole Unix-epoch seconds as an integer. `(sleep secs)` pauses for `secs` seconds; a fractional value is allowed. `(sleep)` and a zero duration return immediately. A negative or NaN duration is an error.

**Math**: `(abs n)`, `(floor n)` → an integer, `(sqrt n)`. `(min a b...)` / `(max a b...)` take **one or more** numbers and return the smallest/largest; mixed ints and floats compare numerically, a tie returns the *first* of the tied values, and a non-numeric argument is an error. They are deliberately numeric-only — not generic "compare anything" — because the hosts disagree about comparing lists and strings (`[1] < [2]` is a type error in Python but fine in JavaScript, and Ruby's `Comparable` will happily compare a String against an Integer). `(sqrt -1)` is an error, not `NaN`.

**Why each of these has a hand-written rule per backend.** AINL has four execution backends — the interpreter/VM, the AOT-compiled C binary, and the Python, JavaScript and Ruby transpiler targets — and a builtin is only real when all five agree. Most of them do, because they map to the host's own facility. The ones above don't, and each of those cases has a test pinning the AINL answer: ASCII case folding and trimming, an empty split separator, an empty replace target, numeric-only `min`/`max`, the errors for a negative `sqrt` or `sleep`, **valid-UTF-8 `read-file`, and the whole path algebra** (the `path-*` builtins reimplement the rules rather than calling `os.path` / `path` / `File`, because the hosts give different answers for the same input — see the table above). `crates/ainl-core/src/eval.rs` is the normative implementation and the other four are written to match it; `crates/ainl-cc/tests/aot_stdlib.rs` and the three `*_stdlib.rs` transpiler suites are what keep them there.

**JSON**: `(json-parse text)` → a value, `(json-serialize value)` → a string.

The reader and writer are hand-written in every backend rather than mapped to
the host's JSON library. Each host library is wrong here in a way that shows up
in output bytes, not just in types: `json.loads`/`JSON.parse` return a `dict`/
plain object rather than an AINL map (losing insertion order and the
duplicate-key rule), `JSON.parse` accepts `NaN` and `Infinity`, and
`JSON.stringify`/`JSON.generate`/`json.dumps` each print floats their own way
and `\u`-escape non-ASCII. `crates/ainl-core/src/json_value.rs` is normative;
`crates/ainl-transpile/tests/json_parity.rs` runs the same programs on all five
backends and diffs the bytes.

Type mapping, both directions:

| JSON | AINL | notes |
|---|---|---|
| object | `hash` (map) | keys are `str` only; order is **insertion order** |
| array | `list` | |
| string | `str` | |
| integer literal | `int` | a literal that fits `i64`; anything larger is a float |
| `1.5`, `1e3` | `float` | any decimal or exponent form is a float |
| `true` / `false` | `true` / `false` | |
| `null` | `nil` | |

Four rules a reader would not guess:

- **Object keys must be `str`.** `(json-serialize (hash 1.5 "v"))` is an error
  (`object keys must be str, got float`) rather than a coerced `"1.5"`, because
  coercion cannot round-trip: AINL compares a string and a float as different
  keys, so the re-parsed map would not be `=` to the original.
- **Insertion order is preserved**, including for a duplicate key: `{"a":1.5,
  "b":2.5, "a":9.5}` parses to a two-entry map holding `9.5` at `a`'s *first*
  position. That is the same first-position/last-value rule `(hash)` and
  `(assoc)` already use, so a parsed object and a hand-built one behave alike.
- **Floats print fixed-point, never scientific**, using the shortest decimal
  that reads back as the same number, with a mandatory `.0` on a whole value:
  `1.0` → `1.0`, `1e-7` → `0.0000001`, `(/ 1 3.0)` → `0.3333333333333333`,
  `1e21` → `1000000000000000000000.0`. The `.0` is what keeps a float
  distinguishable from an int in the text — without it the two would be
  indistinguishable to any reader. This is *not* `print`'s float rule, which
  prints a whole float's exact binary expansion; see
  [NUMERIC_MODEL.md](NUMERIC_MODEL.md#json-serialize-has-its-own-float-rule).
- **Round trip means value identity, not text identity.** `1.50e2` reads back
  as `150.0`, and re-serializing is then idempotent. Non-finite floats, symbols
  and functions are errors — they have no JSON form.

Rejected as malformed (all with a byte offset): trailing content, a leading
zero, a bare `.5` or `1.`, a lone `+`, a control character inside a string, an
unknown escape, a truncated or unpaired `\u` surrogate, nesting deeper than 512
levels, and a duplicate key is fine but a non-string key is not.

One documented divergence, in JS only: a JS `Number` is a single type, so
`(json-serialize 1)` gives `1.0` there and `1` everywhere else, and an
`int` inside a container likewise prints as `1.0`. Whole *floats* agree, and the
output is valid JSON that re-parses to an equal value in both cases. This is
the same int/float collapse the language already documents for `print`.

## 3a. `ainl repl` — interactive and scripted

The REPL is a front end, not a new language: **every form it accepts is
documented above, and it introduces no syntax of its own.** The grammar is
unchanged, so a program typed at the REPL is the same program a file holds.
(Scope: the REPL runs on the interpreter/VM backend only. The AOT and
transpiler backends are reached with `ainl run` / `compile` / `transpile`,
which are unchanged.)

What the REPL does add is a *submission* rule. Three facts below are the only
ones a model or a script author needs.

**A submission is read until its strings and parens close.** An unclosed `(`
keeps reading (the prompt becomes `…`), and a string literal may span lines:

```lisp
(+ 1          ; unclosed `(` → the REPL waits
   2)         ; `…` prompt; the two lines are ONE form, giving 3
```

- If a `(` has no matching `)`, the REPL keeps reading (prompt `…`). A form
  is routinely written across lines.
- If a string literal has no closing `"`, the REPL keeps reading. AINL strings
  may contain newlines.
- An **extra** `)` is *not* a continuation: it is submitted so the parser
  reports the real `unexpected ')'`. A REPL that waited for a `(` here would
  show a `…` prompt forever with no error.
- A `;` comment never causes a continuation.

**One form is echoed; `def`, `nil` and `()` are silent.** The value of the
*last* form in a submission is printed, except that a symbol (what `def`
returns), `nil`, and the empty list print nothing:

```lisp
(def x 6)      ; prints nothing (returns the symbol x)
x              ; => 6
(print x)      ; prints 6 via `print`, and prints nothing itself (returns nil)
```

**Errors print and the session continues.** An error is reported as
`line N: <message>` on **stderr** and the next line is evaluated normally, so
`ainl repl --stdin < prog.ainl > out.txt` yields a clean result file even when
a line in the middle fails. A failed line does not undo earlier `def`s, and each
line gets a fresh step budget (a runaway `while` fails alone).

`--stdin` is the same loop with the banner and prompts suppressed — it is how
the tests drive it and how you script it. See
[GETTING_STARTED.md](GETTING_STARTED.md) for a worked session.

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
