# AINL examples

Ten worked programs (twelve `.ainl` entry points, counting the two multi-file
ones), each real, runnable, and checked on every push. Run any of them
directly:

```
ainl run examples/corpus/countdown.ainl
```

These are not snippets in a document. Every one of them is executed by
`scripts/check-examples.sh` in CI, and an example that stops running — or
stops printing what it claims to print — fails the build. That is the whole
point: this directory is the few-shot source for a model that has never seen
AINL, and a broken example teaches the broken shape.

## What is here

| Example | Demonstrates |
|---|---|
| [`countdown.ainl`](corpus/countdown.ainl) | A `while` loop accumulating into bindings; why a loop body must be written flat. |
| [`flatten-tree.ainl`](corpus/flatten-tree.ainl) | Recursion over a nested structure; `map`/`filter`/`reduce` in AINL; a portable `list?`. |
| [`word-frequency.ainl`](corpus/word-frequency.ainl) | Tokenize, count into a map, and sort by value — selection sort, written by hand. |
| [`csv-report.ainl`](corpus/csv-report.ainl) | Delimited text into records; group-by with `assoc`; what the stdlib does *not* give you. |
| [`json-payload.ainl`](corpus/json-payload.ainl) | Parse an API-shaped payload, extract nested fields, re-serialize, round-trip. |
| [`file-roundtrip.ainl`](corpus/file-roundtrip.ainl) | Read a file, transform it line by line, write it back, clean up. |
| [`storage.ainl`](corpus/storage.ainl) | The `db-*` store: write, overwrite, flush, close, reopen, read back what survived. |
| [`http-get-json.ainl`](corpus/http-get-json.ainl) | An HTTP GET, a JSON response, request headers, and a 404 as a value. |
| [`cli-tool.ainl`](corpus/cli-tool.ainl) | Arguments from the environment, validation, and exit codes as a contract. |
| [`error-handling.ainl`](corpus/error-handling.ainl) | Checking before acting, the shape of a diagnostic, and what `error` is for. |
| [`try-catch.ainl`](corpus/try-catch.ainl) | Catching a failure you expect: the form, the caught value, and a loop that survives. |
| [`aot-compare.ainl`](corpus/aot-compare.ainl) | A compute loop that runs identically interpreted and AOT-compiled; the step cap. |
| [`libmod/main.ainl`](corpus/libmod/main.ainl) | Multi-file programs: flat and namespaced `import`, and both resolution rules. |
| [`testing/run-tests.ainl`](corpus/testing/run-tests.ainl) | The `test` builtin and `ainl test`, with a real suite in [`testing/suite/`](corpus/testing/suite). |

The three directories are not separate examples but parts of one:

- `libmod/` — a `main.ainl` that imports `math.ainl` and `text.ainl`.
- `testing/` — a `run-tests.ainl` plus the suite directory `ainl test` runs.
- `http-fixture/` — the stdlib-Python server `http-get-json.ainl` talks to.

## Scope: what runs where

Every example declares its own `@scope`, and the claim is enforced rather than
hoped for.

- **portable** (8 of them) — verified **byte-for-byte identical** on the
  interpreter, the AOT C binary, and the Python, JavaScript and Ruby
  transpiler targets. The corpus is portable by construction, not by luck.
- **interpreter-only** — `libmod/main.ainl` and `testing/run-tests.ainl` use
  `import`, which the AOT and transpiler backends *refuse* rather than
  silently mishandle. The gate asserts that refusal rather than skipping.
- **aot** — `storage.ainl` uses the `db-*` builtins, which the **AOT C binary
  runs** (the runtime carries a hand-port of the storage engine) and the three
  transpilers *refuse*, because a host `open()` cannot reproduce an
  append-only checksummed log. The gate asserts both halves: the AOT binary's
  output must match the interpreter's byte for byte, and each transpiler must
  refuse with the word `transpiler-only`. This scope exists because the
  `interpreter-only` case above allows AOT to refuse and this one forbids it —
  an AOT refusal here would mean the C port was lost, and every refusal test
  would still pass.
- **server** — `http-get-json.ainl` needs a local HTTP server;
  `scripts/check-examples.sh` starts one on a port the OS picks and asserts
  the program runs against it.

The headers also carry `@teaches` (what pattern the example exists to
demonstrate) and `@expect` (a line of output that must appear, so an example
cannot quietly become a no-op). The gate reads all three.

## Running one yourself

```sh
# any of the portable ones, on any backend
ainl run examples/corpus/countdown.ainl
python3 <(ainl transpile examples/corpus/countdown.ainl --to python)

# the HTTP one needs its fixture
python3 examples/corpus/http-fixture/server.py 8080 &
AINL_PORT=8080 ainl run examples/corpus/http-get-json.ainl

# the module and test ones
ainl run examples/corpus/libmod/main.ainl
ainl test examples/corpus/testing/suite

# check them all
./scripts/check-examples.sh
```

## Using this as a few-shot corpus

`examples/few-shot.txt` is these programs, concatenated, each behind a banner
naming the file, what it teaches and what it demonstrates. It is **generated**
by `scripts/build-few-shot.sh` — do not hand-edit it; CI regenerates it and
fails if it is out of date, so a stale prompt cannot teach syntax that no
longer parses.

`ainl gen` reads it directly:

```sh
ainl gen "count the words in a file"            # 2 examples (the default)
ainl gen --examples all "…"                     # the whole corpus
ainl gen --no-examples "…"                      # language reference alone
```

The default is 2 on purpose. Zero leaves the model with the three tiny
snippets in the built-in language reference, which is enough for a small
program but does not show the rules models actually get wrong — those need a
real program to appear in. All crowds out the task, and the repair loop
re-sends the prompt on every attempt. The index above is in the same order as
the corpus, so you can read it and pass the count you want.

**Selection is a count, not a keyword.** Choosing examples by matching the
spec would need a matcher with no idea what the request is about, and a wrong
guess costs more than it saves — a model shown a file-I/O example when it
asked for arithmetic wastes context and may copy the wrong shape. Read this
table, then pass the number.

The corpus is read from `examples/few-shot.txt` **relative to the working
directory**, so run from an ai-lang checkout. Without it, `ainl gen` still
works — it sends the language reference alone, which is what it did before the
corpus existed — and says so rather than failing.

## Two things this corpus found

Both were found by writing the examples and running them, not by reading the
documentation, and both are now pinned by a test or a gate.

**1. The JS int/float collapse is wider than `docs/SYNTAX.md` recorded.** The
docs used to describe it for `json-serialize` inside a container: a JS `Number`
is one type, so `{"a":1}` prints as `{"a":1.0}`. Measured on this head, the same
collapse happens at top level — `(print 3.0)` prints `3` in JavaScript and
`3.0` on every other backend, and so does `(print (/ 4 2))`. It is also not only
a display rule: a float index is *accepted* where every other backend rejects
it, and a whole float is *named* an int in error text. `check-examples.sh`
recognizes exactly the top-level display divergence and nothing else; a blanket
"strip `.0`" rule would also hide a genuine arithmetic difference between
backends, which is the one class of bug the gate exists to catch.

**Resolved in `docs/SYNTAX.md` §3j**, which now states the true scope, plus
`crates/ainl-transpile/tests/js_number_collapse.rs` pinning every measurement.
The code is unchanged: the fix belongs to the numeric model, not a print rule.

**2. A nested `let` inside a `while` body silently discards every `def` in
it.** This is documented (`docs/SYNTAX.md` §2a) and it is in
`wordcount/lib/text.ainl`, but it is worth restating because it is invisible:
the accumulator is rebound each iteration and vanishes at the end of it, so
the function returns an empty list with no error on any backend. It cost two
debugging rounds while writing `word-frequency.ainl`, and that example is now
built around showing it.

## Adding an example

1. Write `examples/corpus/<name>.ainl` with a header:

   ```lisp
   ; @example   <name>          must match the file stem
   ; @summary   <one line>      for this table
   ; @teaches   <a, b, c>       the patterns it demonstrates
   ; @scope     portable | aot | interpreter-only | server
   ; @expect    <one line>      a line of output that must appear
   ; @run       ainl run examples/corpus/<name>.ainl
   ```

2. Add a row to the table above.
3. `./scripts/build-few-shot.sh && ./scripts/check-examples.sh`

The gate reads the header, so a missing or mismatched `@example` fails it.
Use `@scope portable` unless the program genuinely needs `import`, `http-get`
or a server — a portable example is checked on five backends for free, and
that is worth far more than the three it costs you to write.
