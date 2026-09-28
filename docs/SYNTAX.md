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
| `import` | `(import "m.ainl")` / `(import "m.ainl" as m)` | Top-level only. Loads a module file and binds its top-level `def`s into this file's scope, or one name `m` holding a map of them. See §3b. |
| `try` | `(try body... (catch (e) handler...))` | Run `body`; on a runtime error bind it to `e` and run `handler`. Opens a scope (§2a). See §3e. |

`import` is the one special form that is not an expression: it produces
bindings, not a value, and it is resolved before any other form runs.

## 2a. Scoping: which forms open a new environment

`def` always writes into the **nearest enclosing scope** — but "nearest
enclosing scope" means the nearest enclosing form that actually opens one, not
just the nearest enclosing form of any kind. Only two forms open a new scope;
every other form evaluates its sub-forms directly in the scope it was itself
evaluated in:

| Form | Opens a new scope? |
|------|-------------------|
| `fn` (a fresh one per call) | **Yes** |
| `let` | **Yes** |
| `try` (each side gets its own, as siblings) | **Yes** |
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

**Testing**: `(test name expr expected)` → `true`, or abort with a message naming the test, the expected value and the actual one. See §3d.

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

## 3b. Modules: `import`

A program can be split across files. `import` binds another file's `def`s into
the current program.

| Form | Meaning |
|------|---------|
| `(import "path/to/mod.ainl")` | Bind **every top-level `def`** in the module into this file's scope. |
| `(import "path/to/mod.ainl" as m)` | Bind **one** name, `m` — a map of the module's exports. |

`import` is **top-level only**. An import inside a `fn`, `let`, `do`, or `while`
body is an error, not a late-binding feature. Imports are resolved before any of
your code runs, so a module's names are always in scope where you use them.

### The namespaced form is a map

`m` is an ordinary AINL map, so it needs no access syntax of its own:

```lisp
(import "lib/math.ainl" as m)
(get m "square")          ; the function
((get m "square") 7)      ; => 49 — call it like any other value
(has m "square")           ; => true
(keys m)                  ; => ("square" "cube") — insertion order
```

### What a module exposes

A module's exports are **the names it `def`s at its own top level** — not
everything in its scope. A module that imports another does *not* re-export it:

```lisp
; math.ainl
(def square (fn (n) (* n n)))

; shapes.ainl
(import "math")            ; uses square…
(def area (fn (s) (* s s))) ; …but exports only `area`

; main.ainl
(import "shapes")
(import "math")            ; REQUIRED: `square` is not re-exported
```

A module body is evaluated with only the builtins in scope: it cannot see the
importing file's names.

### Resolving the path

Given a specifier, in this order:

1. **Absolute** (`/opt/m.ainl`) — used as-is.
2. **Path-like** (contains `/`) — the *importing file's* directory first, then
   the working directory. A module asking for a sibling means "next to me".
3. **Bare name** (`math`) — the working directory first, then the importing
   file's directory.

A candidate with **no extension** gets `.ainl` appended, so `(import "math")`
and `(import "math.ainl")` are the same file. An explicit extension is
respected as written. When nothing resolves, the error lists every path tried.

**A module importing a sibling uses the bare name.** `(import "math")` from
inside `lib/` finds `lib/math.ainl`; `(import "lib/math.ainl")` from there would
look for `lib/lib/math.ainl`.

### Name collisions are an error, never a shadow

A flat `(import ...)` may not bind a name that is already bound — a builtin, an
earlier import, or a name *this file* `def`s (in either order). The error names
the conflict and points at the fix:

```
import: 'len' is already defined (by the prelude), so "lib/shadow.ainl" cannot bind it.
Rename one of them, or import the module under a name: (import "lib/shadow.ainl" as <alias>)
and reach it with (get <alias> "len")
```

A silently shadowed binding is invisible at the call site, so the language
refuses instead. Use `as` when you genuinely want both.

**Re-importing the same file is a no-op**, not a collision. A module is read
and evaluated **once per path**, so a diamond (`a` and `b` both import `c`) runs
`c` exactly once. A file that imports itself, directly or transitively, is a
**circular import** error that names the cycle.

### Backend scope: the interpreter only

`import` works with `ainl run` and `ainl repl` — the interpreter/VM **and** the
tree-walking evaluator behind it, which are held to agreeing on it.

The **AOT C backend and the three transpilers (Python / JS / Ruby) refuse a
program containing `import`**, with an `interpreter-only` error naming the
byte offset. This is deliberate, not an omission: a transpiler emits one source
file with no module-resolution phase, and `import` is a *keyword* in Python,
Ruby and JavaScript — an unhandled directive would lower into the host's own
import machinery and produce a program that builds cleanly and does the wrong
thing. A program with no `import` is unaffected on every backend.

(Scope note: the REPL and `import` are independent. The REPL adds no syntax of
its own — see §3a.)

## 3c. HTTP: `http-get` / `http-post`

| Call | Returns |
|------|---------|
| `(http-get url)` | a response map |
| `(http-get url headers)` | the same, with request headers |
| `(http-post url body)` | a response map |
| `(http-post url body headers)` | the same, with request headers |

`url` must start with `http://`. `body` is a string. `headers` is a map of
`str`→`str`.

```lisp
(def r (http-get "http://127.0.0.1:8080/health"))
(get r "status")                 ; => 200   (an int)
(get r "ok")                    ; => true
(get r "body")                  ; => "…"
(get (get r "headers") "content-type")   ; => "text/plain"

(http-post "http://127.0.0.1:8080/items"
           "{\"name\": \"widget\"}"
           (hash "Content-Type" "application/json"))
```

### The response is an ordinary map

Four keys plus a nested `headers` map. No new value type and no new access
syntax, so `get`, `has`, `keys` and `vals` all work on it and a helper written
once handles every response:

| Key | Type | Meaning |
|-----|------|---------|
| `status` | `int` | the HTTP status code |
| `ok` | `bool` | `true` when `status` is 200–299 |
| `body` | `str` | the response body |
| `headers` | map | `str`→`str`, **names lowercased** |
| `reason` | `str` | the status text (`"Not Found"`) |
| `truncated` | `bool` | present **only** when the body hit the size cap |

### A non-2xx status is a value, not an error

`(get (http-get u) "ok")` is `false` for a 404. The call **succeeds**. This is
deliberate: a 404 is a fact about the server, and a caller that has to
string-match an error message to tell a 404 from a 500 cannot branch on it
properly. Only a *transport* failure (cannot connect, timed out, malformed
response) is a runtime error.

```lisp
(def r (http-get "http://127.0.0.1:8080/nope"))
(if (get r "ok") "found" "missing")           ; => "missing"
```

### Rules a caller has to know

- **Plain HTTP only. `https://` is refused**, with an error naming the fix.
  This is a decision, not a gap — AINL's zero-dependency rule is what keeps its
  AOT binaries standalone, and every TLS stack is a C-transitive dependency
  tree. Use a local TLS-terminating proxy and point AINL at its `http://` side.
  The full reasoning and the two priced ways forward are in
  [HTTP_TLS.md](HTTP_TLS.md).
- **The body must be valid UTF-8.** Otherwise it cannot be an AINL string, and
  it is an error rather than a lossy replacement character — the same position
  `read-file` takes.
- **The body must be framed**: `Content-Length` or chunked
  `Transfer-Encoding`. A close-delimited body is refused, because reading to
  EOF on a connection that does not close never returns.
- **Repeated response headers keep the first value.** `Set-Cookie` arrives
  several times on a real response, and joining with `,` is wrong for every
  header that uses commas as a list separator, so there is no join. Look the
  header up by name; AINL has no way to ask for the second one.
- **No redirects, no cookies, no keep-alive.** A 3xx comes back as a response
  like any other. A client that follows redirects can be walked somewhere the
  caller never named.
- **`Host` and `Content-Length` cannot be set** — AINL computes both from the
  URL and the body, and a caller-supplied value is the request-smuggling
  primitive. Credentials in a URL (`http://u:p@host/`) are refused for the same
  reason: they end up in a request line and get logged.
- **A header name or value containing CR or LF is refused.** It would split one
  request into two.
- **Limits are fixed, not tunable**: 10 s to connect, 30 s to read, 8 MiB body,
  64 KiB headers. A body over the cap is **truncated and flagged** with
  `truncated`, not refused. A builtin that can hang forever or exhaust memory
  makes every networked program untestable.

### Backend scope: the interpreter only

`http-get` / `http-post` work with `ainl run` and `ainl repl` — the
interpreter/VM and the tree-walking evaluator, which are held to agreeing on
them. The **AOT C backend and the three transpilers refuse** a program that
uses either, with the same `interpreter-only` error `import` gives (§3b). A
socket plus an HTTP/1.1 client in the C runtime would break the static-binary
guarantee, and each host language's HTTP library disagrees with the others
about redirects, header casing, timeouts and verification defaults — which is
the "builds cleanly, does something subtly different" failure the four-backend
rule exists to prevent.

## 3d. Testing: `test` and `ainl test`

A test is `expr == expected`. Nothing more: no fixtures, no mocking, no async,
no test objects, no setup/teardown. AINL programs are small enough that the
whole test is the assertion and its expectation.

```lisp
(test "adds two numbers" (+ 1 2) "3")
```

Passes, yielding `true`. Fails, aborting the program with:

```
runtime error: test failed: adds two numbers: expected 4, got 3 at line 1, col 1 (byte 0)
```

`name` and `expected` are both **strings**, and this is the one rule a caller has
to know:

- `name` must be a string, because the message has to name the test.
- `expected` is a string because it is compared against the value's **rendered**
  form — the exact text the failure report prints. That is deliberate: the
  assertion and its own report can then never disagree, and it removes the last
  place a value could print differently on two backends. Write the expectation
  the way `print` would show the value: a list is `"(1 2)"`, a whole float is
  `"1.0"`, `nil` is `"nil"`, a map is `"{\"k\" v}"`.

Note what that implies: `1` and `1.0` are `=` in AINL but are *not* the same
assertion, because they render differently. `(test "t" 1.0 "1")` fails.

### Why a failure is an error, not a printed line

`(test ...)` could have printed `ok`/`FAIL` and returned a bool. It raises
instead, for one reason: the four backends must agree byte-for-byte on stdout
*and* stderr, and a printed failure would make the message a stdout concern —
so its parity would depend on each host's shim flushing and formatting a value
at exactly the same instant. An error reuses the mechanism `error` and
`read-file` already use and that all four backends already agree on: one shared
message body. `crates/ainl-cc/tests/test_parity.rs` enforces it.

### Running a suite

```sh
ainl test              # runs ./tests
ainl test path/        # every *.ainl in a directory
ainl test one.ainl     # a single file
ainl test tests/ -q    # --quiet: summary only
```

A **test file is an ordinary AINL program** — no registration, no naming
convention, no special file type. It runs through the same entry point as
`ainl run`, so `import` resolution and the prelude are identical. A harness
whose runner differs from the thing it tests is a harness that can pass while
the program fails.

Files are discovered in **sorted** order, so a report is the same on every
machine. Each file is independent: a failing test aborts *its* file, and the
runner continues with the next one, so one broken test does not hide every test
after it.

**Exit codes are the contract.** 0 only when every test passed. Non-zero on any
failure, on any file that errored, and on a suite that contains no tests at all —
"no tests found" must not read to CI as "all tests passed".

```
ok   tests/file_io.ainl (30 passed)
FAIL tests/json.ainl — 36 passed, 1 failed: parses an empty array
     runtime error: test failed: parses an empty array: expected [], got  at line 41, col 1 (byte 920)

36 passed, 1 failed, 0 errors across 2 files
```

The per-file line and the summary always agree: the total counts the tests that
passed *before* the failure in each failing file, because a failing test aborts
its file and the ones after it never ran.

A **failed test** and an **errored file** are reported differently, because only
one of them is fixed by editing a test: a file that fails to parse, or that
raises outside a `(test ...)`, is reported as `ERROR`.

### Backend scope: all four

`test` is an ordinary builtin and is supported on **all four backends** — the
interpreter, the bytecode VM, the AOT C runtime, and the Python, JS and Ruby
transpilers. The only difference is the one §5a already documents: the
interpreter and the VM append a position, because they hold the source, and the
AOT binary does not, because it embeds none. The **message body is identical
everywhere**, which is what `test_parity.rs` asserts.

## 3e. Error handling: `try` / `catch`

```lisp
(try
  body-form ...
  (catch (e) handler-form ...))
```

If `body` succeeds, `try` returns the body's last value and the handler never
runs. If any form in `body` raises, `e` is bound to the error and the handler
runs; `try` returns the handler's last value.

```lisp
(print (try (+ 1 2) (catch (e) "unreachable")))        ; => 3
(print (try (/ 1 0) (catch (e) (get e "message"))))    ; => division by zero
(print (try (error "boom") (catch (e) e)))             ; => {"message" "boom" "kind" "runtime"}
```

Both sides are **value sequences**, not single expressions, and `try` itself is
an ordinary expression — `(def status (try … (catch (e) …)))` is the natural
form, and it works. In *expression* position a side may only be one form, the
same limit `let` has; the transpilers say so rather than silently dropping a
form.

### The value `e` binds to

`e` is a **map with exactly two keys, in this order**:

| Key | Value |
|-----|-------|
| `message` | the error message, a `str` |
| `kind` | `"runtime"` |

The key **order is part of the contract**. Every backend prints a map in
insertion order, and `message` before `kind` is what makes a printed caught
value byte-identical across all five backends. Do not build the map yourself
with `hash` and rely on your backend's ordering — use `try`/`catch`.

`kind` is derived from the **error variant**, not from the error instance. Only
`"runtime"` is reachable by `catch`, because a lex or parse error stops the
program before any of it runs.

### Message bodies are AINL's, not the host's

A `catch` binds a message that a program may compare or print, so the bytes
have to be the same on every backend and on every operating system. Two rules
follow, and both are load-bearing:

- **No OS error text.** `http-get` to a dead port reports
  `http: cannot connect to 127.0.0.1:1 failed: connection refused`. The reason
  is AINL's own wording, mapped from the OS error class — *not* `strerror`
  output. `Connection refused (os error 61)` is macOS spelling, and the same
  failure is errno 111 spelled differently on Linux. Parse a caught `message`
  by AINL's text; the OS detail is deliberately not there.
- **No host exception text.** `read-file` on a missing path reports
  `read-file: cannot read 'x.txt'` on all backends, not Python's
  `FileNotFoundError`, JS's `ENOENT` object or Ruby's `Errno::ENOENT`.

### Nesting, scope, and what a `try` is a scope for

Nested `try` works and the **innermost `catch` wins** — the inner handler runs
and the error does not reach the outer one.

`try` opens a scope, like `let` and not like `do`. The body and the handler each
get a fresh child of the *enclosing* scope, and they are **siblings**: a `def`
in the body is not visible to the handler. This matters when the body fails
partway, because a handler then cannot read a half-initialised binding from the
form that threw. A `def` in a *successful* body is likewise invisible outside
the `try`.

An error raised **by the handler** is not caught by that same `try`; it
propagates outward to the next enclosing `try`, and if there is none it ends the
program.

`try` may appear anywhere a form may appear: on its own, inside `fn`, `let`,
`while`, or another `try`.

### An uncaught error is unchanged

An error no `try` handles still prints to stderr as
`runtime error: <message> at line L, col C (byte B)` and exits non-zero. Nothing
about the failure path changed; `catch` only adds a way to handle one.

Each `try` body also gets a fresh step budget, the same rule a top-level run
and a REPL submission get — so an inner runaway loop is reported as a caught
error rather than escaping to the top.

### Backend scope: all four

`try`/`catch` is supported on **all four backends**. The message body of a
caught error is byte-identical across the interpreter, the VM, the AOT C
runtime, and the Python, JS and Ruby transpilers.

**The AOT unwind** is result-threading, not `setjmp`/`longjmp`. The C runtime
already funnels every failure through `set_err` → `g_err`, so the error channel
exists; what a generated `try` adds is a dispatch to the handler when that flag
is set, and `v_error_value()` clears the flag as it builds the caught map — so
code after the dispatch does not re-see it. `setjmp`/`longjmp` was rejected
because it would skip the refcount release of every `Value` temporary in the
body, leaking heap on every caught error — in exactly the loop a health-check
runs in.

**One deliberate difference.** An unbound symbol is a *compile-time* failure in
Python, JS and Ruby, so a `catch` cannot intercept it there; the interpreter
and VM fail at run time and `catch` does see it. A program that relies on
catching an unbound symbol is therefore interpreter/VM-only.

## 3f. Collections: `map` / `filter` / `reduce` / `sort`

```lisp
(map    (fn (x) ...)   lst)        ; new list, fn applied to each element
(filter (fn (x) ...)   lst)        ; new list of the elements the fn finds truthy
(reduce (fn (acc x) ...) init lst)  ; fold left; acc starts at init
(sort   lst)                        ; sorted copy
(sort   (fn (a b) ...) lst)        ; sorted copy, by a comparator
```

All four are **pure**: they return a new list and never touch the input. AINL
lists are shared, immutable cons cells, so this costs nothing to promise.

Empty input: `map`, `filter` and `sort` give `()`; `reduce` gives `init` back
untouched. That last one is a real case, not a degenerate one — summing nothing
is `0`, so `(reduce (fn (acc x) (+ acc x)) 0 xs)` is the sum of `xs` with no
special case at the call site.

Write the empty list as **`(list)`**, not `()`. `()` is the AST for an empty
list *node*, which evaluates to `nil`, so `(push () 1)` is a type error — the
value `()` and the literal `()` are not the same thing.

### `fn` is data here

`map`/`filter`/`reduce` take a function **value**, so a callback can be a
literal or a name:

```lisp
(def double (fn (x) (* x 2)))
(map double (list 1 2 3))          ; (2 4 6)
(map (fn (x) (* x 2)) (list 1 2 3)) ; (2 4 6) — same thing
```

Passing something that is obviously not a function is an error, and the message
names the form you wrote rather than an internal helper:

```
(map 5 (list 1))   ; runtime error: map expects a fn, got int
```

This is worth stating because it is the first place AINL treats a function as
an ordinary value. A **bare symbol is never rejected** — `x` may well be a `def`
holding a closure, and naming the callback is the normal way to write this. The
check catches only operands that cannot possibly be callable.

`sort`'s comparator takes **two** arguments, not the element: `(fn (a b) ...)`.

### `filter` uses AINL truthiness, not the host's

The predicate is asked a yes/no question, and "no" means the one thing AINL calls
falsey:

| value | in a `filter`? |
|---|---|
| `nil` | no |
| `false` | no |
| `0` | **yes** |
| `""` | **yes** |
| `(list)` | **yes** |

`0` and `""` are **truthy**. This trips up every host language — Python, JS and
Ruby between them treat both as falsey — so a `filter` here is `(filter p xs)`
where `p` is kept to a real predicate, and `(if p x y)` rather than `p and x`
(see §2 for what `and`/`or` return).

### `reduce` builds things, not just sums

`acc` is an ordinary value, so a fold can accumulate a list, a string or a map —
the two workhorses for it:

```lisp
(reduce (fn (acc x) (push acc (* x x))) (list) (list 1 2 3))  ; (1 4 9)
(reduce (fn (acc x) (+ acc x)) 0 (list 1 2 3 4))            ; 10
```

Note the accumulator comes **first** and the element second, so an accumulator
that starts as `(list)` stays a list.

### `sort` is stable, and refuses to guess

`sort` returns a **stable** sorted copy: elements that compare equal keep their
input order. A program that sorts by a key that ties will therefore print the
same thing every time, on every backend, and in a later run.

The default order is numbers by value and strings **bytewise**. A list mixing
the two is an **error**, not an arbitrary-but-reproducible order:

```
(sort (list 1 "a"))   ; runtime error: sort expects a list of numbers or of
                      ;   strings, got a list mixing int and str
```

This is deliberate. A `sort` that quietly put every number before every string
would return a stable, reproducible answer to a program that has a bug in it, and
that bug would surface much later as a wrong number instead of here as a type
error. `int` and `float` are **not** a mixed list — they compare by value, so
`(sort (list 1 1.0 0.5))` is fine.

The comparator form returns **negative / zero / positive**, and must return a
number: `(sort (fn (a b) "x") lst)` is an error rather than a list left in input
order, which would read like a working sort. The comparator is type-checked
*before* the list is walked, so `(sort cmp (list 1))` still rejects a
non-function comparator instead of accepting it because there was nothing to
compare.

### Backend scope: all four

All four are supported on **all four backends** — the interpreter, the bytecode
VM, the AOT C runtime, and the Python, JS and Ruby transpilers — byte-identical
on stdout and on the error messages above.

`map`/`filter`/`reduce` are **special forms**, not ordinary builtins, and they
are worth knowing that because it explains two things. A builtin in AINL is
handed its arguments and nothing else, with no way to *call* a function value it
was given, so a builtin `map` could not work in the interpreter or the VM at all
— it would work in C and in all three transpiler targets, where a closure is a
real function pointer or lambda. That is a silent divergence in the one
direction this language's rule exists to prevent, so these three are instead
lowered, once, into the `def` + `while` loop they semantically are, before any
backend sees the program. All six evaluators then run the *same* loop through
the path they already had.

`sort` stays a real builtin: it has no function to call in its default form, and
in the comparator form the comparator is an ordinary value each backend already
knows how to call. It is hand-written per backend rather than delegated to the
host's `sort` for the reasons above — Ruby's `sort_by` is not stable, JS orders
strings by UTF-16 code unit and Python by code point, and none of them rejects
a mixed list. One explicit stable merge sort per backend makes each of those a
property of code that is right there.

## 4. Canonical examples

These are one-liners to fix the shape in your head. For programs that are
worth reading in full — a loop that accumulates, a group-by, a hand-written
sort, a module split across files, a test suite — see
[../examples/README.md](../examples/README.md). Every one of those is a real,
runnable program, and CI runs all of them on every backend each claims to
support, so they cannot be out of date with the language. They are also the
corpus `ainl gen` puts in front of a model, which makes them the closest
thing AINL has to a reference implementation.

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

## 5a. Error messages

When AINL fails, the diagnostic is meant to be *acted on*. Every error answers
three questions, in this order:

1. **What** went wrong — a short phrase naming the thing.
2. **Where** — a 1-based `at line N, col M` pointing into your source. Never a
   bare byte offset alone: a byte offset means nothing to someone reading the
   text. The byte offset is still included, in parentheses, so a tool can
   correlate the error with `ainl ast --json` output.
3. **The likely fix**, when one can be inferred, after an em dash.

Columns count **characters**, not bytes, so they match what you see in an
editor.

```
<kind> error: <what went wrong> at line N, col M (byte B) — did you mean 'x'?
```

### The four shapes

| Shape | Example |
|---|---|
| Unbound symbol | `unbound symbol 'doubl' at line 3, col 8 (byte 38) — did you mean 'double'?` |
| Arity mismatch | `arity mismatch: (add2) takes 2 args, got 1 at line 2, col 1 (byte 29)` |
| Type error | `runtime error: expected a number, got str at line 2, col 1 (byte 12)` |
| Parse error | `parse error: unexpected ')' at line 1, col 10 (byte 9)` |

Real output, verbatim:

```
$ cat broken.ainl
(def double (fn (x) (* 2 x)))
(print (doubl 21))

$ ainl run broken.ainl
runtime error: unbound symbol 'doubl' at line 2, col 9 (byte 38) — did you mean 'double'?
```

### Close-match suggestions

An unbound symbol gets `— did you mean 'x'?` when a name **currently in scope**
is close to the one you wrote. This is the highest-value part of the error: it
turns a failed run into a single edit.

The rule, in full:

- **In scope, not a hardcoded list.** Builtins *and* the program's own `def`s
  and parameters *and* imported names are all candidates. A typo'd local is
  corrected against the local.
- **Case-insensitive**, so `PRINT` suggests `print`.
- **Bounded edit distance**: at most `max(1, len(name) * 0.34)` edits
  (insert, delete, substitute). A longer name tolerates more slips than a
  short one.
- **At least 3 characters.** Every one-character name is distance 1 from every
  other, so without this a bare `f` would be "corrected" to `*`.
- **Never an operator.** A candidate made only of punctuation (`+`, `*`, `<=`)
  is never suggested — someone who typed a word did not mean an operator.
- **One suggestion, not a menu**, and ties break alphabetically so every
  backend prints the same string.
- **Silence when unsure.** If nothing is close enough, there is no suggestion
  at all. A confident wrong suggestion costs a repair more than no suggestion.

### What is *not* given a position

Two errors carry no line, on purpose:

- **Resource limits** — the step budget (`AINL_MAX_STEPS`, default 2,000,000)
  and the recursion depth. These are properties of the whole run, not of any
  one form. Attributing a step-limit failure to the `while` loop that started
  it would be a guess, and a plausible-looking wrong line is worse than none.
- **The AOT C backend and the three transpilers** — see below.

### Backend differences (the 4-backend rule)

The interpreter and the bytecode VM produce **byte-identical** stderr for the
same program; that equality is enforced by `crates/ainl-core/tests/error_format.rs`.

The other two backends differ in one way, and it is structural rather than
incidental:

| Backend | What goes wrong | Position | Suggestion |
|---|---|---|---|
| Interpreter (tree-walk) | yes | yes | yes |
| Bytecode VM | yes | yes | yes |
| AOT C | yes | **no** | **no** |
| Python / JS / Ruby | yes | **no** | **no** |

`ainl compile` emits a **standalone C program**: the source text is not embedded
in the binary, so a line/column is not merely unavailable to the runtime — it
does not exist. The C runtime therefore stops at the description. The
transpilers emit host-language source, and the host raises its own exception
when the AINL-level check does not fire first.

The rule this preserves: **what went wrong** matches byte-for-byte across all
four backends. Only the position and the suggestion are interpreter-only, and
they are never invented where they cannot be known. Run the program with
`ainl run` when you need a diagnostic a model can act on.

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
