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
| `and` | `(and a b ...)` | Short-circuit; returns first falsey or the last value. It returns an **operand, not a boolean** — `(and 1 2 3)` is `3`, and `(and 1 "x")` is `"x"`. With no operands it is `true`. |
| `or` | `(or a b ...)` | Short-circuit; returns first truthy or `false`. It returns an **operand, not a boolean** — `(or nil 0)` is `0` (0 is truthy, §1), and `(or nil false)` is `false`. With no operands it is `false`. The same rule covers one operand: `(or x)` is `x` when `x` is truthy and `false` when it is falsey, so `(or nil)` is `false` and not `nil`. |
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

**Byte-oriented string functions** — all indices and lengths are **byte offsets**, not character offsets: `(substring s start end)` → the slice `[start, end)`, `(char s i)` → the whole character at byte `i`, `(code s)` / `(code s i)` → a raw byte value, `(starts-with s p)` / `(ends-with s sfx)` → bool, `(index-of s sub)` → the byte index of the first occurrence, or `-1`. A slice that would split a multi-byte character is an **error**, and an out-of-range index is an error rather than a clamp. See §3g for the rules and why.

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

The int/float distinction survives the round trip on **every** backend,
including JS: `json-serialize 1` is `1` and `json-serialize 1.0` is `1.0`
everywhere, and `json-parse` returns a tagged float for a decimal/exponential
literal and a raw int otherwise. JS used to collapse the two (it has one
`Number` type), which made `(json-serialize 1)` print `1.0` there and `1`
everywhere else; the tagged-number fix closed that. §3j describes the fix and
the one divergence that remains (magnitude, not type).

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

### Backend scope: the AOT backend inlines; the transpilers refuse

`import` works with `ainl run` and `ainl repl` — the interpreter/VM **and** the
tree-walking evaluator behind it, which are held to agreeing on it — and with
`ainl compile` (AOT C).

**The AOT backend resolves the import graph and inlines it.** A module's code
is emitted into the program, so the compiled binary reads no source at run time
and keeps working after its `.ainl` files are deleted. This is what makes a
package manager safe to add: `ainl pkg` vendors sources into the tree at build
time, and inlining puts them in the binary. Three rules keep the inlined
program equivalent to the interpreted one, and each is a hard error rather than
a silent difference:

- a module is inlined **once**, so a diamond runs one copy of it;
- two modules defining the same top-level name is an **error** naming the
  collision — a duplicate inside a *single* module is still ordinary AINL;
- a missing module, a nested `import`, or a cycle is refused, with the cycle
  or the candidate paths named.

**The three transpilers (Python / JS / Ruby) still refuse a program containing
`import`**, with an `interpreter-only` error naming the byte offset. This is
deliberate, not an omission: a transpiler emits one source file with no
module-resolution phase, and `import` is a *keyword* in Python, Ruby and
JavaScript — an unhandled directive would lower into the host's own import
machinery and produce a program that builds cleanly and does the wrong thing.
A program with no `import` is unaffected on every backend.

See `ainl pkg` ([§3i](#3i-packages-ainl-pkg)) for how a package name becomes a
resolvable bare import.

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

## 3g. Byte-oriented string primitives: `substring` / `char` / `code` / `starts-with` / `ends-with` / `index-of`

Six builtins for looking *inside* a string. Before them AINL could only take a
string apart with `split` and `replace`: there was no way to slice, no way to ask
what character is at a position, no prefix or suffix test, and no find. A
tokenizer had to `replace` every paren with a space and re-`split` the line —
which cannot report *where* anything was.

### AINL strings are byte strings

**Every index and every length in these six builtins is a byte offset.** Not a
character, not a grapheme cluster. `(index-of "héllo" "llo")` is `3`, because
`"héllo"` is six bytes (`é` is two) and five characters; a character-indexed
answer would be `2`.

This is forced by the 4-backend rule, not chosen for elegance. Each host indexes
its own way — Python's `str.find` and Ruby's `String#index` return a *character*
offset, and JavaScript's `String#indexOf` returns a UTF-16 *code-unit* offset —
so a "natural" implementation returns three different numbers for the same
non-ASCII input. Byte offsets are the one thing all five can be made to agree
on.

The rule already half-existed: `len` on a string counts **characters**, and
`list-dir` sorts by **byte value**. Those coexist deliberately, and the
distinction is worth internalizing — `(len (substring "héllo" 1 3))` is `1`,
because that 2-byte slice is one character.

### The builtins

- `(substring s start end)` → the bytes in `[start, end)`, **`end` exclusive**.
  `(substring "abcdef" 2 4)` is `"cd"`. Both boundaries are legal, so
  `(substring "abcdef" 0 0)` and `(substring "abcdef" 6 6)` are both `""` — an
  empty slice is a value, not an error.
- `(char s i)` → the **character** starting at byte offset `i`, whole. One byte
  for ASCII, the entire multi-byte sequence for anything else. `(char "日本" 3)`
  is `"本"`, because byte 3 is where the second character starts.
- `(code s)` → the byte value of the **first byte** of `s`; `(code s i)` → the
  byte at offset `i`. `(code "A")` is `65`.
- `(starts-with s prefix)` / `(ends-with s suffix)` → bool. An empty prefix or
  suffix is `true` — the empty string is contained in everything, which is the
  same answer `contains` already gives and what makes `(starts-with s "")` a
  useful no-op guard. A needle **longer** than the haystack is `false`, not a
  bounds error.
- `(index-of s sub)` → the byte index of the **first** occurrence of `sub`, or
  `-1`. `(index-of "banana" "na")` is `2`. An empty `sub` is **`0`**, not `-1`:
  the empty needle occurs at offset 0, and that is also what keeps `index-of`
  consistent with `contains`. (It is what all four hosts answer anyway, so it
  costs nothing in parity.)

### A slice that splits a character is an error

There is one consequence every backend has to share, and it is the reason `char`
takes an index at all rather than returning half a sequence: a slice or a
character extraction that would **cut a multi-byte UTF-8 sequence in half** is a
**runtime error** — `substring end index splits a multi-byte character` — not a
replacement character and not a silently short read.

```
(substring "héllo" 0 2)   ; error — byte 2 is the middle of "é"
(substring "héllo" 1 3)   ; "é"    — both ends are boundaries
(char "日本" 1)            ; error — byte 1 is a continuation byte
(char "日本" 3)            ; "本"
```

An AINL string is always valid UTF-8 (`read-file` rejects a file that is not),
so there is no value that could hold half a character. The alternative — quietly
substituting U+FFFD — is exactly what the file and JSON code already refuses to
invent, for the same reason.

`code` is the deliberate escape hatch: it reads a **raw byte** and so needs no
boundary at all. `(code "日本" 1)` is `151`, a continuation byte, for a program
that genuinely wants to walk UTF-8 by hand.

### Out-of-range is an error, not a clamp

`(substring s 0 999)` is `substring end index out of bounds`. Clamping would make
it quietly succeed and turn an off-by-one in a caller's arithmetic into a
silently wrong string — the exact failure these builtins exist to prevent.

```
(substring "abc" -1 2)   ; substring start index out of bounds
(substring "abc" 3 1)    ; substring start index is greater than end index
(char "abc" 3)           ; char index out of bounds
(code "")                ; code expects a non-empty string
(code "abc" 9)           ; code index out of bounds
```

An index operand must be an **int**. A float is rejected, not truncated:
`(substring "abc" 0 1.5)` is `substring expects an int end index, got float`.
The hosts disagree about what a fractional index means (Python raises, JS
coerces to 1, Ruby raises), and truncating would make a caller's arithmetic bug
invisible on three backends and fatal on two.

### Composing them: a tokenizer

The vocabulary these six add is enough to find where a call opens and closes,
which the `replace`/`split` approach cannot do:

```
(index-of "(print 42)" "(")                                  ; 0
(substring "(print 42)" 1 (index-of "(print 42)" ")"))      ; "print 42"
(code (char "(print 42)" (index-of "(print 42)" "(")))      ; 40  — the "("
(ends-with "(print 42)" ")")                                 ; true
```

`tests/byte_strings_tokenizer.ainl` builds a full line tokenizer on this — one
that tracks whether it is inside a quoted string, and so does not shred
`(print "a(b")` into four tokens the way padding parens into whitespace does.

### Backend scope: all four

All six are supported on **all four backends** — the interpreter, the bytecode
VM, the AOT C runtime, and the Python, JS and Ruby transpilers — byte-identical
on stdout and on every error message above.

None of the three transpiler targets can delegate to its host, and the reason
differs in each case, which is why the helpers look the way they do. Python works
on `s.encode('utf-8')` because a `str` slices by character. JavaScript works on
`Buffer.from(s, "utf8")` because a JS string is a sequence of UTF-16 *code
units* — `s.slice(a, b)` slices code units and `s.charCodeAt(i)` will return
half a surrogate pair. Ruby works on `s.bytes` (an Array of Integer) because a
`String` slices by character; note that `s.b` looks equivalent and is not — it
returns a binary-encoded **String**, so `b[i] & 0xC0` raises `NoMethodError`.

`scripts/check-byte-strings.sh` runs `fixtures/byte_strings_parity.ainl` through all
five runners and diffs them against the interpreter, which is the normative
implementation. (The parity program is a *print* program rather than a test
file, so it lives in `fixtures/` — `ainl test` sweeps `tests/` and would count
its output as suite noise.)

## 3h. File system: `mkdir` / `rename` / `copy` / `is-dir` / `file-size`

The Tier 3 file-system operations, completing the cluster that
`read-file`/`write-file`/`delete-file`/`list-dir` started. They exist because the
existing file builtins can only *inspect* and *rewrite*: moving a file meant
read → write → delete, which copies every byte, cannot move a directory at all,
and leaves two half-copies if it is interrupted.

```
(mkdir path)                    ; create one directory
(mkdir path ":recursive")        ; create it and every missing parent
(rename from to)                ; move a file or a directory tree
(copy from to)                   ; duplicate a file
(is-dir path)                    ; true for a directory, nil otherwise
(file-size path)                 ; the size in bytes
```

Each returns `nil` on success. `is-dir` returns `true` for a directory and `nil`
for anything else — including a path that does not exist. `nil`, not `false`, so
that `(= (is-dir p) nil)` is a usable test, which is the same convention
`file-exists` and `list-dir` already follow.

### The option is a string, not a keyword

`":recursive"` is a **string literal**, not a bare word. AINL has no
keyword-argument syntax, and a bare `:recursive` is an ordinary symbol, so:

```
(mkdir "a/b/c" :recursive)
; runtime error: unbound symbol ':recursive' at line 1, col 16 (byte 15)
```

The call fails before `mkdir` is ever reached. The option is therefore the
second positional argument, and an unknown one is an error rather than ignored —
a silently-ignored `:recursive` would be a `FileNotFoundError` three calls later
with nothing pointing at the cause:

```
(mkdir "a/b/c" ":parents")
; mkdir: unknown option ':parents'
```

### mkdir refuses an existing path, in both modes

```
(mkdir "d")                    ; ok
(mkdir "d")                    ; mkdir: cannot create 'd': it exists
(mkdir "d" ":recursive")       ; mkdir: cannot create 'd': it exists
```

This is **stricter** than `mkdir -p`, `os.makedirs(exist_ok=True)`,
`fs.mkdirSync(path, {recursive: true})` and `FileUtils.mkdir_p`, all of which
return quietly on an existing path. It is the right default for a language where
the alternative failure — silently succeeding — means a caller that created a
directory and then wrote into it cannot tell whether *it* created it. The
consequence is that a program which re-runs over its own output must guard the
call, and `file-exists` is what it uses:

```
(if (not (file-exists d)) (mkdir d ":recursive"))
```

Without `":recursive"`, a missing parent is an error. The message names the path
the *caller* wrote, not the parent that was missing — the caller never named it,
and guessing which component was missing would be a worse error message than the
one AINL gives:

```
(mkdir "absent/child")
; mkdir: cannot create 'absent/child'
```

### rename refuses an existing destination

```
(write-file "a.txt" "A")
(rename "a.txt" "b.txt")                          ; nil
(write-file "a.txt" "A")
(write-file "b.txt" "B")
(rename "a.txt" "b.txt")
; rename: cannot move 'a.txt': 'b.txt' exists
(rename "missing.txt" "b.txt")
; rename: cannot move 'missing.txt': it does not exist
```

POSIX `rename(2)` refuses an existing destination. `os.rename`,
`fs.renameSync` and `File.rename` all **overwrite it silently**. AINL pins the
refusal so the same program cannot destroy a file on some backends and preserve
it on others — the worst class of portability bug, because it is invisible until
the data is gone.

`rename` moves a **directory tree**, which read → write → delete cannot express
at all, and does it without reading a byte, so the cost is one syscall rather
than a full copy and an interrupted move cannot leave two half-copies.

A cross-device move is a distinct error, not the generic one, because
`rename(2)` reports `EXDEV` when the source and destination are on different
filesystems (a separate volume, a tmpfs mount, a container boundary) and the fix
is a copy, not a retry:

```
(rename "/mnt/a" "/tmp/b")
; rename: cannot move '/mnt/a' to '/tmp/b': different filesystems
```

### copy refuses a directory

```
(write-file "a.txt" "A")
(copy "a.txt" "b.txt")                            ; nil
(copy "a.txt" "b.txt")                            ; nil — copy DOES overwrite
(mkdir "src")
(copy "src" "dst")
; copy: cannot copy 'src': it is a directory
```

`copy` is the one operation here that *does* overwrite an existing destination,
matching `cp` and every host. That is intentional and consistent: copying is
explicitly a "make another one" operation, so a pre-existing destination is not
data loss. The asymmetry with `rename` is the point — `rename` refuses because
the destination is a thing the caller still has, and `copy` does not.

A recursive directory copy is deliberately **not** offered. It is a different
operation from copying a file, it needs its own rules about what happens to
symlinks and permissions, and `rename` covers the case a program usually wants
(it moves a tree, and costs one syscall). A program that genuinely wants a copy
can walk the tree.

### file-size counts bytes

`(file-size path)` is the size in **bytes**, not characters. For ASCII they
agree; for anything else they do not, and a character-based answer would make
every non-ASCII file the wrong size:

```
(write-file "u.txt" "héllo")   ; 6 bytes, 5 characters
(file-size "u.txt")            ; 6
```

A directory is an **error**, not a number. POSIX reports a directory's *inode*
size, which is 4096 on ext4, 60 on APFS and 0 on tmpfs — three different answers
for the same directory, none of them meaningful:

```
(file-size "d")
; file-size: cannot read 'd': it is a directory
```

### Symlinks are not followed

All five probe with `lstat`, not `stat`. A symlink to a directory is therefore
**not** a directory as far as AINL is concerned:

```
(is-dir "link-to-dir")   ; nil  — the link itself is not a directory
```

This is the one rule that needed care to keep portable. `os.path.isdir`,
`fs.statSync` and `File.directory?` all follow the link and would answer `true`;
only `lstat`/`symlink_metadata` matches. A broken symlink is still a directory
entry, so it exists for `file-exists` and blocks `mkdir`.

A trailing separator is trimmed before the query, because `"f/"` is not a legal
way to name a non-directory on any of the four hosts:

```
(is-dir "d/")   ; true — the same answer as (is-dir "d")
```

The trim is guarded on length so a lone `"/"` survives.

### Error messages

One message per failure mode, byte-identical on every backend, and every one of
them is a `try`/`catch`-able AINL error:

```
(mkdir 1)                        ; mkdir expects a str path, got int
(mkdir)                          ; mkdir expects (mkdir path) or (mkdir path option)
(mkdir "a" "b" "c")              ; mkdir expects (mkdir path) or (mkdir path option)
(rename 1 2)                     ; rename expects a str path, got int
(rename "a")                     ; rename expects (rename from to)
(copy 1 2)                       ; copy expects a str path, got int
(copy "a")                       ; copy expects (copy from to)
(is-dir 1)                       ; is-dir expects a str path, got int
(is-dir)                         ; is-dir expects (is-dir path)
(file-size 1)                    ; file-size expects a str path, got int
(file-size)                      ; file-size expects (file-size path)
```

The type name is AINL's, not the host's: a Python port that rendered
`<class 'int'>`, or a Ruby one that rendered `Integer`, would be a message the
program could not match on.

### Backend scope: all four

All five are supported on **all four backends** — the interpreter, the bytecode
VM, the AOT C runtime, and the Python, JS and Ruby transpilers — byte-identical
on stdout, on stderr and on return codes.

Every backend implements the rules above explicitly rather than delegating to
its host, because on each of the three the delegation would be *wrong* in a way
that shows up as data loss or as a host-specific number. Python checks the
destination before `os.rename` and uses `os.makedirs` only for the parents;
JavaScript checks before `fs.renameSync` and omits the `recursive` flag rather
than passing `false`; Ruby builds the parents one component at a time with a
local `_fs_mkdir_p` rather than requiring `fileutils`, which would also be quiet
on an existing leaf. All three use `lstat` throughout, and each catches its
host's exception class to re-raise an AINL error — a `TypeError`, `ArgumentError`
or `SystemCallError` is not an `_AinlError`, so it would escape an AINL `catch`
and print a host backtrace to stderr.

`scripts/check-fs-builtins.sh` runs `fixtures/fs_builtins_parity.ainl` through
all five runners and diffs them against the interpreter, which is the normative
implementation. Each runner gets a **fresh scratch tree**, because the program
creates, moves and deletes files and is not idempotent. (The parity program is a
*print* program rather than a test file, so it lives in `fixtures/` — `ainl test`
sweeps `tests/` and would count its output as suite noise.)

## 3i. Packages: `ainl pkg`

Modules (§3b) let one program read files next to itself. Packages let it read
files that are **declared, pinned, and checked in** — and, because the AOT
backend inlines imports, let the compiled binary carry them with no source at
all.

Three files, each with one job:

| file | job |
|---|---|
| `ainl.pkg` | what the project **wants** — hand-written, never generated |
| `.ainl-lock` | the **exact** resolved graph — generated, committed |
| `.ainl-vendor/` | the package **sources** — generated, committed |

The lockfile and the vendor dir are both committed, so a build resolves the
same way on a laptop, in CI, and on a machine that has never seen the
dependency. Nothing is fetched implicitly at build time.

### The manifest

`ainl.pkg` is line-oriented key/value data, with a `[deps]` section:

```
name: app
version: 0.1.0

[deps]
name: greet
version: 1.2.0
source: ../greet
```

A `source` is a **local path** (`./dir`, `../dir`, `/abs`) or
`git:<url>@<rev>`. An `https://` tarball is **refused** with a message saying
there is no package registry in this tier — a silent fallback to a local path
would be a dependency that resolves to the wrong thing.

### The five commands

```
ainl pkg init [--name <name>]              write ainl.pkg here
ainl pkg get <name>[@<version>] <source>   add a dep, resolve and vendor it
ainl pkg install                           vendor everything the manifest declares
ainl pkg list                              print the resolved graph
ainl pkg verify                            check .ainl-vendor against .ainl-lock
```

Every command resolves from the **project root** — the nearest ancestor with an
`ainl.pkg` — not the working directory, so `ainl pkg verify` means the same
thing from anywhere in the tree. That is what makes it usable as a CI step that
does not have to know where it was invoked from.

`init` refuses to overwrite an existing manifest. It is a scaffolding command,
and a command that silently overwrites work eventually destroys it.

`get` and `install` write the lockfile; **`verify` never does**. That split is
deliberate: a check that repairs as it checks cannot fail a build, and a build
that repairs as it builds is not reproducible. `verify` is the CI gate — it
exits non-zero on any difference, including an edited vendored file (the
lockfile records a digest per file, not just a file list) and a missing one.

### Importing a package by name

A vendored package is imported by **bare name**, exactly like a module that
happens to be in the working directory:

```lisp
(import "greet")        ; -> .ainl-vendor/greet.ainl
                        ;  or .ainl-vendor/greet/greet.ainl
```

Ordinary files are searched **first**. A real file always beats a package of
the same name, so `(import "greet")` cannot silently change meaning because
someone ran `ainl pkg install`. Only *bare* specifiers consult the vendor dir;
a path-like specifier means "next to me" and keeps meaning that.

### What is refused

These are errors, not policies chosen by the resolver:

- **a dependency cycle** — the message names the cycle (`a -> b -> a`);
- **two versions of one package** in a graph. The lockfile resolves to exactly
  one version per name, and inventing a selection policy is not this tier's
  job. Two packages that agree on name *and* version are the same package, and
  the first one read wins.
- **a version range** like `^1.0.0`. Versions are exact; a range would have to
  be resolved against something this tier does not have.
- **a name collision across modules** (§3b) — which the AOT inliner also
  enforces, since it flattens every module into one namespace.

The root project itself is recorded in the lockfile as `root:` and is **not**
copied into its own `.ainl-vendor/`.

`scripts/check-pkg.sh` is the gate. It resolves a two-package graph, runs it in
the interpreter, compiles it, deletes every `.ainl` file, and re-runs the
binary — which is the only way to prove the inlining rather than assert it.

## 3j. The JS int/float distinction: now carried in the value

The JavaScript target used to have one numeric type. AINL has two — `int` and
`float` — and the difference did not survive the crossing: a whole float lost
its `.0` on display, a float index was silently accepted where AINL rejects it,
a whole float was *named* an int in error text, and `json-serialize` turned an
int into a float. This section used to state the whole of that collapse,
because the scope was routinely under-stated as a JSON-only quirk, and a reader
who believed that would write a program that behaved differently on JS without
being able to predict which part.

**The collapse is now closed on JS.** The transpiler carries the int/float
distinction in the *value* rather than the type: a float literal is emitted as
`new _Float(…)` (a thin wrapper with a `valueOf()`), and an int stays a raw
`Number`. The tag drives **display and type-checks only, not `=`** —
`(= 1 1.0)` is still `true` on every backend, because numeric equality is
numeric. With the tag in place, all five backends agree on every case that used
to diverge:

| program | interpreter, AOT, Python, Ruby, JS |
|---|---|
| `(print 3.0)` | `3.0` |
| `(print (/ 4 2))` | `2.0` |
| `(print (+ 1.5 1.5))` | `3.0` |
| `(print (+ 1 2))` | `3` |

A **non-whole float** was never affected — `(print 0.5)` is `0.5` on all five —
and a whole **int** stays a bare `3` on all five (the tag does not leak into
ints). The three consequences that used to follow from the missing type are all
gone:

- **A float index is rejected on every backend.** `(substring "abc" 0 1.0)` is
  a type error on all five (`expects an int end index, got float`); JS now sees
  the tag and refuses it instead of returning `"a"`.
- **A whole float is *named* a float in error messages.**
  `(json-serialize (hash 1.0 "v"))` says `got float` on all five.
- **`json-serialize` preserves the distinction.** `json-serialize 1` is `1` and
  `json-serialize 1.0` is `1.0` on all five, and `json-parse` returns a tagged
  float for a decimal/exponential literal and a raw int otherwise, so the
  round trip is byte-identical across backends. §3's JSON note is updated
  accordingly.

### The one divergence that remains

The tag closes the *type* dimension. It does **not** close the *magnitude*
one: JS still computes in IEEE-754 doubles, so it does not reproduce the
interpreter's i64-overflow promotion (e.g. `(* 9223372036854775807 2)` is
`18446744073709552000` on JS and Python/Ruby's exact
`18446744073709551614`). That is the accepted divergence documented in
[NUMERIC_MODEL.md](NUMERIC_MODEL.md) and pinned by
`crates/ainl-transpile/tests/numeric_divergence.rs`.

### How it is pinned

Every measurement above is pinned by
`crates/ainl-transpile/tests/js_number_collapse.rs` (re-pinned to the fixed
behaviour) and the JSON round-trip by
`crates/ainl-transpile/tests/json_parity.rs`, so the day a target starts
disagreeing about something here, the suite says so.


## 3k. Storage: `db-open` / `db-put` / `db-get-raw` / `db-flush` / `db-close`

Five builtins, a whole durable store, and no dependency:

```ainl
(def h (db-open "notes.ainl-db"))
(db-put h "todo" "buy milk")
(db-put h "todo" "buy oat milk")   ; overwrites, does not erase
(db-flush h)
(print (db-get-raw h "todo"))          ; buy oat milk
(print (db-get-raw h "absent"))        ; nil
(db-close h)
```

Keys and values are **strings**. `db-open` returns a **handle** — an ordinary
int — and `db-get-raw` returns the latest stored text for a key or `nil`.

This section's reader is called `db-get-raw` because §3l takes the name
`db-get` for the value-level read. Everything else here is unchanged, and a
program written against §3k needs exactly one edit: `db-get` → `db-get-raw`.

### The rules

- **`db-open path` → int.** Creates the file if it is missing, replays it if it
  is not.
- **`db-put handle key value` → nil.** Appends a record. An existing key is
  *not* rewritten in place: the new value is appended and the last write wins.
- **`db-get-raw handle key` → str or nil.** Never fails for an open handle. A
  missing key is `nil`, so a caller can probe without a `try`.
- **`db-flush handle` → nil.** `fsync`: the data is on the device, not in a
  buffer.
- **`db-close handle` → nil.** Flushes, then releases the handle.
- **At most 64 databases** may be open at once. The 65th is refused with
  `db-open: too many open databases (max 64)` — and the check runs *before* the
  file is created, so a refused open leaves no file behind.
- Handles are **1-based, lowest free slot first**, and the number comes back
  after a close.
- A handle that is not open — stale, closed, zero, negative, or never issued —
  is refused as `db-get-raw: handle 7 is not open`.

### Why an append-only log

A `db-put` **appends**. It never seeks back to rewrite a record, and it never
rewrites the file to apply an update. That is the whole design, and it is what
buys the durability:

If a process dies mid-write, the file is a **prefix** of complete records
followed by at most one partial one. Every record carries a length and a
checksum, so the reader can tell the two apart exactly: it replays the prefix,
discards the tail, and **truncates the file back to the last good record** so
the next append lands on a clean boundary. A store that updated in place would
instead have a *hole* — the old value half-overwritten — and no checksum could
tell a hole from a value.

The consequence worth knowing: the file **grows**. Overwriting a key a thousand
times leaves a thousand records on disk. The log is not compacted; that is a
future tier, and it needs a rule about when it is safe (only when a single
writer has the file), so it is not in this one.

### The format

A 16-byte header, then records. Every integer is little-endian.

```text
header   "AINLDB" | version u8 | 0 u8 | header_len u32 | 0 u32
record   key_len u32 | val_len u32 | crc32 u32 | key bytes | val bytes
```

The CRC is CRC-32/ISO-HDLC — the reflected IEEE polynomial `0xEDB88320`, the one
zlib's `crc32()` computes, with the standard `123456789 → 0xCBF43926` check
value. It covers **`key ++ value`**, the record's body and not its header; the
two length fields are what frame the key/value split, so `("a","b")` and
`("ab","")` are different records even though their checksums are equal.

A file that is not an AINL database is **refused**, not repaired:
`db-open: 'x.ainl-db' is not an AINL database`. So is a future version, and the
message names both: `is database version 2, but this AINL reads version 1`.

Replay stops at the first record it cannot trust — torn, mis-checksummed, not
UTF-8, or holding a NUL. Everything before it survives.

### Backend scope: interpreter, VM and AOT C — **not** the transpilers

This is the first tier where the four backends genuinely differ, so the
difference is the point rather than a gap.

| backend | `db-*` | why |
|---|---|---|
| interpreter | **supported** | the normative implementation |
| bytecode VM | **supported** | the same Rust code, reached through a different entry point |
| AOT C | **supported** | a hand-port of the engine in `runtime.c`, libc only |
| Python, JS, Ruby | **refused** | a host file API cannot reproduce the log |

`crates/ainl-core/src/db.rs` is normative — it defines the behaviour above and
the format. `runtime.c` is a **hand-port**, the same relationship the C JSON
implementation has to the Rust one, and it is checked for real rather than by
inspection: `crates/ainl-cc/tests/db_crash.rs` compiles a program, runs it, and
compares the resulting `.ainl-db` **byte for byte** against the interpreter's
for the same program. A file written by the interpreter opens in a compiled
binary and vice versa.

The AOT port is not a wrapper around a host library, and it adds no dependency:
`fopen`, `read`, `write`, `ftruncate`, `fsync` and a local CRC table. The
`aot-standalone` CI job still links `musl-static`, which is the property the
whole project is selling.

The transpilers **refuse** at transpile time:

```text
ainl transpile --to python: `db-open` is transpiler-only (found at byte 0) — …
```

Note the wording: **transpiler-only**, not interpreter-only. `ainl compile` runs
these programs, and a message saying "interpreter-only" would send a user
looking for the wrong runner.

The refusal is deliberate over an emulation. A Python `open()`, a JS `fs` handle
and a Ruby `File` could all be made to *look* like this API, and each would be
quietly wrong: no append-only log, so an update rewrites the file; no per-record
checksum, so a torn write is served as data; no recovery, so a crashed write
poisons the file for good; and no single uniform meaning for "durable". A
program that transpiled cleanly and then read a *different file* than the one
the interpreter wrote is a far worse outcome than a build that stops and says
why. The refusal names the reason, the byte offset, and the two runners that do
work.

### Reading a handle

A handle is an int, not a distinct type. It has to be: adding a `Value` variant
would need a new arm in `print`, `=`, `json-serialize` and every other consumer
on every backend, for no gain. So a handle is comparable, printable and
arithmetic-able like any other number — `(= h 1)` is how you test one.

The consequence is that a handle can be *wrong* rather than absent, so the rules
above are all about catching that: a handle is checked against the open table on
every call, and a bad one is a clear error naming the number.

### Two rules that exist only because two ports have to agree

Most of the rules above would be the same on a single implementation. These two
are not — they exist because `runtime.c` is a *separate* implementation of the
same format, and a rule that is right on one side and absent on the other is a
silent divergence rather than a bug.

**The checksum is pinned to an external constant, not to the other port.** The
CRC-32 test vector is `crc32("123456789") == 0xCBF43926`, and both sides assert
that value. Comparing the C port against the Rust one would be worthless here: if
the C table had a typo, the two would still agree with each other and every
recovery test would still pass, over a format whose checksums nothing outside the
project could produce. Pinning to zlib's published vector is what makes it an
integrity check instead of a private convention.

**A record holding a NUL is dropped, and the record has a correct CRC when it
is.** Values travel as a `char *` through the C runtime, so a record containing
a NUL would read back *truncated* there while Rust kept the bytes. Both sides
therefore reject such a record during replay. The test for this builds a record
with a **valid** checksum and objectionable content, precisely so that a replay
checking only the CRC would accept it — otherwise the test would pass for the
wrong reason.

Both tests are in `crates/ainl-cc/tests/db_crash.rs`, and both were confirmed to
*fail* when the corresponding C code was deliberately broken: flipping the
polynomial to `0xEDB88321` fails the CRC test, and dropping the NUL check makes
the C port return `"a"` where Rust returns `nil`. A test never seen failing is
not known to work.

## 3l. Key-value storage: `db-set` / `db-get` / `db-get-raw` / `db-del` / `db-keys` / `db-count`

A **value** store on top of §3k's byte store. Five more builtins, and a program
can now remember a number, a list or a boolean between runs:

```ainl
(def h (db-open "settings.ainl-db"))
(db-set h "theme" "dark")                 ; a string
(db-set h "columns" 80)                   ; a number
(db-set h "recent" (list "a.ainl" "b.ainl"))  ; a list
(db-set h "onboarded" true)               ; a boolean
(db-flush h)
(db-close h)

(def h (db-open "settings.ainl-db"))      ; a new process, same file
(print (db-get h "theme"))                ; dark
(print (db-get h "columns"))              ; 80
(print (db-get h "missing"))              ; nil
(db-del h "theme")
(print (db-count h))                      ; 3
(print (db-keys h))                       ; ("columns" "onboarded" "recent")
(db-close h)
```

### The two layers, and the one name they share

§3k stores **bytes**: `db-put` takes a string. This section stores **values**:
`db-set` takes any AINL value. They are the same file, the same log, the same
checksums, the same handle table — this layer is a second way to read and write
the storage §3k already opened.

One name belongs to both, and it is worth being precise about which one you get:

| you write | reads | returns |
|---|---|---|
| `db-put` | bytes | — |
| `db-get-raw` | bytes | the stored text, exactly |
| `db-set` | values | — |
| `db-get` | values | the AINL value, decoded |

`db-get` is the **value** read. §3k's byte-level read kept its behaviour and
moved to **`db-get-raw`**, which is its exact former self: same argument checks,
same `nil` for a missing key, and it reports its errors under its own name
(`db-get-raw: handle 7 is not open`). If you are porting a §3k program, that is
the one-word change.

### The rules

- **`db-set handle key value` → nil.** Stores any value JSON-encoded. A value
  JSON cannot represent — a function, a symbol, a non-finite float, a map with a
  non-string key — is refused with **the JSON writer's own message**, e.g.
  `json-serialize: cannot serialize a fn`. That is the same answer
  `json-serialize` gives, on every backend, because it is the same code.
- **`db-get handle key` → the value, or nil.** `nil` for a key that was never
  written *and* for one that was deleted — the two are the same answer by
  design, and `db-keys` is how you tell them apart.
- **`db-get-raw handle key` → str or nil.** The stored text with no decoding,
  including a delete's tombstone. This is how you see the bytes.
- **`db-del handle key` → true or false.** `true` if the key was live, `false`
  if it was not. Deleting an absent key is not an error, so
  `(if (db-del h k) ...)` is safe to run twice.
- **`db-keys handle` → a list of the live keys, sorted.** Sorted by byte value,
  the same order `list-dir` uses.
- **`db-count handle` → an int.** How many keys are **live** — a deleted key is
  already out of the count.
- Last write wins, exactly as in §3k: `db-set` on an existing key appends a new
  record, and a `db-set` after a `db-del` brings the key back.

### What a value is stored as

JSON, through the **existing** `json-serialize` / `json-parse` machinery. There
is no new format: the JSON text rides inside §3k's record envelope, so a file
written by this layer is a §3k file, and `(db-get-raw h "theme")` on the value
`"dark"` returns the eight characters `"dark"` — quotes included, because that
is what JSON says a string is.

Every value type round-trips exactly, **including the int/float distinction**:

```ainl
(db-set h "i" 1)     (db-get h "i")     ; 1    — an int
(db-set h "f" 1.0)   (db-get h "f")     ; 1.0  — still a float
```

### Why deletion is a log record

An append-only log cannot remove a record, so `db-del` **appends a tombstone**
and the replay applies records in order: a tombstone drops the key from the
index, and a later `db-set` for that key adds it back.

That is not a shortcut. It is what makes a delete survive a crash exactly the
way a write does — the recovery path is the *same* replay, not a second mechanism
that has to be kept in agreement with the first. The cost is the cost §3k already
documents: **the file grows.** A key written five times and deleted once leaves
six records. Compaction is a later tier's problem, and pretending otherwise
would mean a second write path.

### The sharp edge: `db-put` text is not a value

The two layers share a file, so a key can be written as bytes and read as a
value. If the bytes happen to be valid JSON, you get the value they spell:

```ainl
(db-put h "k" "42")
(db-get h "k")            ; 42 — an int, because "42" is a JSON number
```

If they are not, you get an **error**, not a bare `nil`:

```ainl
(db-put h "note" "buy milk")
(db-get h "note")
; runtime error: db-get: 'note' holds text that is not an AINL value (buy milk);
;   store it with db-set rather than db-put
```

`nil` would be the wrong answer twice over: it is indistinguishable from a
missing key, and it leaves a program that mixed the layers with no way to find
out which of the two went wrong. The message names the text and the fix, and
`(db-get-raw h "note")` returns `buy milk` — which is what you want if you meant
the bytes all along.

### The rules that exist only because two ports have to agree

As in §3k, some rules are not about the language but about keeping
`runtime.c` and the Rust engine from drifting apart.

**`db-keys` is sorted, and that is a parity requirement, not a nicety.** The two
engines index keys differently — a `HashMap` in Rust, a chained hash table in C —
and neither has a defined iteration order. Returning the index in its natural
order would print *the same keys in a different sequence* on each backend, which
is the one thing byte-identical output forbids.

**A key written twice occupies two entries in the C index and one in the Rust
one.** The C index chains rather than replacing, so `db-keys` and `db-count`
there deduplicate by testing whether an entry is the newest for its key. Skipping
that made the C port list an overwritten key twice and count it twice while the
interpreter listed it once — found by the parity test, and it is exactly the kind
of bug that a single-backend test cannot see.

**The value layer calls the C runtime's own JSON writer.** A `db-set` refusal
and a `json-serialize` refusal are the same string on both engines not because
the text was copied but because both sides call one writer.

### Backend scope: the interpreter, the VM, and the AOT C binary

`db-set`, `db-get`, `db-get-raw`, `db-del`, `db-keys` and `db-count` work in
**three** backends — the interpreter, the bytecode VM, and the compiled AOT
binary, whose C runtime carries a hand-port of this layer.

The **Python / JavaScript / Ruby transpilers refuse** all nine `db-*` names,
with `transpiler-only`:

```
$ ainl transpile --to python kv.ainl
runtime error: ainl transpile --to python: `db-set` is transpiler-only
  (found at byte 34) — this backend emits one source file for one host language,
  and a host file API has no append-only log, no per-record checksum, and no
  crash-tail recovery — a program that ran here would read a different file than
  the one the interpreter wrote. Run the program with `ainl run` instead, or
  `ainl compile` for the AOT C binary.
```

The label is **transpiler-only**, not interpreter-only, because `ainl compile`
runs these programs perfectly well. §3k explains the reasoning at length; the
refusal is the same contract, on the same terms.

**The round-trip claim is scoped to those three backends, deliberately.**
`db-*` is refused by the transpilers outright, so it only runs where the
interpreter and AOT carry the store — the int/float guarantee above is stated
for exactly those backends. (JS used to be excluded on the strength of its
one-`Number`-type collapse, which would have turned an AINL int into `1.0`; the
tagged-number fix closed that, so the scoping is now purely about which
backends support `db-*` at all.)

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
