# AINL — the language an LLM can't get wrong

[![CI](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/GRITui/ai-lang/actions/workflows/ci.yml)

**AINL is a language a constrained decoder cannot emit invalid code in.** Its
entire grammar is 10 GBNF rules and 561 bytes. Hand that file to any
constrained decoder — llama.cpp, vLLM, Ollama, LM Studio — and the output is
valid AINL *by construction*, not by retrying.

The rest of the language exists so that output is worth having: it validates in
linear time, compiles to a standalone native binary, and maps losslessly to
Python, JavaScript, and Ruby when you want out.

```lisp
(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))
(print (fib 10))          ; 55
```

## Project status

**AINL is an educational project.** It was built to explore, from first
principles, what a language designed *for* AI-constrained generation looks like
when the constraint is the reason the language exists at all — the measurements
above are the argument, and the negative ones are part of it. Every stage that
was planned is built and CI-green: the bytecode VM, the AOT compiler, the
standard library, packaging, and the generation experiments. **No further
feature development is planned**; the repository is kept as a reference, and
readers are welcome to take the findings and the grammar and build something
else with them.

## The proof

Two independent runs, each comparing the **same model, same prompts, same
temperature** with and without the grammar applied. The check is GBNF
membership — is the output in the language of the exported grammar? — which is
the *sound* detector; `ainl ast` is not, because the parser is a superset of
the grammar and accepts invalid input.

**When the model is the bottleneck** — a 0.5B model, the case AINL is designed
for. The grammar is the only thing separating the two modes:

| mode | GBNF membership | `ainl ast` parse | `ainl run` |
|---|---:|---:|---:|
| **constrained** | **20/20 (100%)** | 20/20 (100%) | 0/20 (0%) |
| unconstrained | **0/20 (0%)** | 20/20 (100%) | 0/20 (0%) |

**When the model is strong** — Qwen3.8-27B-FP8, 12 checkable-answer tasks
scored by exact stdout match. Ten completed; two were lost to a gateway
timeout storm and are recorded as *not run*, not as failures:

| mode | GBNF membership | runs | **correct** |
|---|---:|---:|---:|
| **constrained** | **10/10 (100%)** | 10/10 | **9/10 (90%)** |
| unconstrained | 10/10 (100%) | 10/10 | 9/10 (90%) |

Two things to read here, and the second one matters more:

1. **The constraint never fails.** 20/20 and 10/10, across two different models
   and two different decoders. That is the guarantee.
2. **On a strong model the constraint is insurance, not an improvement.** Both
   arms scored 9/10 and produced byte-identical output — the model already
   writes valid AINL unprompted, so there is nothing left for the grammar to
   rescue. **Constraint value scales inversely with model capability.** A
   pitch that hid this would be measuring the model, not the language.

### What the constraint does *not* buy you

**It guarantees syntax, not semantics.** The single miss in the 27B run is the
proof, and it is worth reading in full — the model treated `list` as a
two-argument constructor, so `(double-each (list 1 2 3))` printed `(2 (4 (6)))`
instead of `(2 4 6)`. That program is *syntactically perfect*. It passes the
grammar, it runs, and it is wrong. No grammar can fix it; it is a property of
the model.

So the honest claim is narrow and worth stating precisely: **AINL makes invalid
syntax impossible, and makes checking the output cheap. It does not make
programs correct.** Semantics stay the model's job. Every number on this page
is a measurement, and the negative ones are here too — including the 0.5B run
where 0/20 constrained outputs ran at all (the model was degenerate), which is
why the project moved to a 27B rather than declaring victory on syntax.

Full method, per-prompt tables, raw generated text, and the three gateway
quirks that silently corrupt results: **[docs/GENERATION.md](docs/GENERATION.md)**.

## The pipeline

**generate → validate → compile → run**, end to end, on a program a model
actually wrote (the 27B's `fib-10` output, not a hand-written sample):

```console
$ python3 scripts/gen-harness/validate_gbnf.py fib-10.ainl   # is it in the grammar?
True
$ ainl run fib-10.ainl                                       # 55
$ ainl compile fib-10.ainl -o fib10                          # AOT → C → native
compiled fib-10.ainl -> fib10
$ ./fib10                                                    # 55
```

**Validation is O(n) and that is the point.** Measured cost of a full
membership check, across a 625× size range:

| bytes | validate | µs/byte |
|---:|---:|---:|
| 82 | 47.3 µs | 0.576 |
| 410 | 240.2 µs | 0.586 |
| 2 050 | 1 209.8 µs | 0.590 |
| 10 250 | 6 036.4 µs | 0.589 |
| 51 250 | 30 969.0 µs | 0.604 |

Per-byte cost is flat, so validating every generation is ~0.6 µs/byte — a 10 KB
program in about 6 ms. You can afford to check all of them.

**[docs/PIPELINE.md](docs/PIPELINE.md)** is the full walkthrough with every
command and its real output.

## `ainl grammar` is the product

The grammar is a first-class export, not a build artifact:

```console
$ ainl grammar --gbnf
# AINL v0.1 — GBNF grammar for constrained decoding.
# Every string this grammar accepts is a syntactically valid AINL program.
root    ::= ws form (ws form)* ws
form    ::= list | atom
list    ::= "(" ws (form ws)* ")"
atom    ::= (string | number | symbol) ws
string  ::= "\"" ([^"\\] | "\\" ["\\/nrt] )* "\""
number  ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?
symbol  ::= sym-char+
sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!" | "?" | "." | "_" | "&"
ws      ::= ( [ \t\n\r] | comment )*
comment ::= ";" [^\n]* "\n"
```

That is the whole contract, and there is nothing AINL-specific about consuming
it. Pipe it to any deployment that speaks GBNF:

```sh
# llama.cpp — the most direct form: a native constraint, not a prompt
llama-cli -m model.gguf -f prompt.txt --grammar-file <(ainl grammar --gbnf) -st

# vLLM / any OpenAI-compatible gateway
#   -> "structured_outputs": {"grammar": <the same text>}
```

The grammar is the interoperability story. The rest of the language is what
you do with the output afterwards.

## Why the rest of the language exists

A generation target is only useful if the generated program is worth keeping.
AINL's other properties, all measured:

- **One unambiguous parse tree.** Uniform `(op arg...)`, no infix, no operator
  precedence, no statement terminators, no significant whitespace. Every
  program is a single tree, serialized to stable JSON with source spans and
  read back losslessly (`ast --json` / `--json-out`).
- **Lossless interop.** The same AST projects to idiomatic Python, JavaScript,
  and Ruby, verified byte-equivalent across all four backends. Holds for values
  within i64/float-safe range; integer-overflow behavior itself diverges by
  design across targets ([docs/NUMERIC_MODEL.md](docs/NUMERIC_MODEL.md)).
- **AOT to a native binary.** `ainl compile` emits a single self-contained C
  file — the micro-runtime is inlined, so the output links against nothing but
  libc. Needs `cc` to build; **the output needs nothing.**
- **57 builtins.** 55 are byte-identical on all four backends (the interpreter,
  the AOT binary, and the three transpiler targets). The other 2 — `http-get`
  and `http-post` — are **interpreter-only**: the AOT and transpiler backends
  refuse a program that uses them with an explicit `interpreter-only` error
  rather than emit something that behaves differently
  ([docs/SYNTAX.md](docs/SYNTAX.md#3c-http-http-get--http-post)).
- **A test runner.** `(test name expr expected)` asserts, and `ainl test` runs
  a directory of test files and exits non-zero on any failure — so AINL can
  check its own output, and CI can consume the result with no parsing
  ([docs/SYNTAX.md](docs/SYNTAX.md#3d-testing-test-and-ainl-test)).
- **Zero external dependencies.** A small static Rust binary, no crates.io
  runtime deps.

### Speed

40,000-iteration loop, release build, best of 25
([docs/PERFORMANCE.md](docs/PERFORMANCE.md)):

| Engine | 40k sum-to loop | vs tree-walk |
|---|---:|---:|
| AINL tree-walk | 35.4 ms | 1.0× |
| AINL bytecode VM | 5.9 ms | ~6× |
| **AINL AOT (whole process)** | **2.25 ms** | **~16×** |
| native Rust (whole process) | 1.77 ms | ~20× |

The AOT binary lands within **1.27× of hand-written Rust** for the same loop.
Of the AOT binary's 2.25 ms, 1.56 ms is process startup — subtracting the
measured floor, AOT compute is 0.69 ms, about **51× the interpreter**.

### Things that did not work

Kept here because a pitch that only lists wins is not a pitch you can trust.

- **Token density: the goal was not met.** AINL uses **~2× the tokens of
  idiomatic Python** (+108%), and more than JS (+64%) or Ruby (+95%) —
  measured with real tokenizers. The parentheses are structural but each pair
  costs tokens, and explicit `def`/`fn`/`if`/`let` add more. The original
  "token-dense, optimized for LLM context" premise is **disproved**;
  AINL's real advantage is reliability of generation, not token count.
  ([docs/BENCHMARK.md](docs/BENCHMARK.md))
- **The 0.5B model could not write AINL.** 0/20 constrained outputs ran, and
  all 20 were byte-identical degenerate templates. The grammar held at 100%;
  the model had nothing to say. Syntax was never the bottleneck — model
  capability was.
- **`import` is interpreter-only.** Multi-file programs run on the interpreter
  and the tree-walking evaluator; `ainl compile` and the Python/JS/Ruby
  transpilers **refuse** a program containing an import. `import` is a keyword
  in all three hosts, so an unhandled directive would lower into the host's own
  import machinery and produce a program that builds cleanly and does the wrong
  thing. Inlining modules would change their evaluation semantics, so the
  backends decline rather than guess. A program with no import is unaffected on
  every backend. ([docs/SYNTAX.md §3b](docs/SYNTAX.md#3b-modules-import))
- **`http-get` / `http-post` are interpreter-only, and there is no TLS.** The
  HTTP client works on the interpreter and the tree-walking evaluator; the AOT
  and transpiler backends **refuse** a program using it, because a socket in the
  C runtime would break the static-binary guarantee and the three host HTTP
  libraries disagree about redirects, header casing and timeouts. `https://` is
  **refused outright**: every TLS stack is a C-transitive dependency tree, and
  the zero-dependency rule is what makes the AOT binary standalone. Use a local
  TLS-terminating proxy. This is a decision with two priced ways forward, not an
  oversight — [docs/HTTP_TLS.md](docs/HTTP_TLS.md) records the reasoning.
  ([docs/SYNTAX.md §3c](docs/SYNTAX.md#3c-http-http-get--http-post))

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
ainl doctor        # 7 checks; exit 0 only if all pass
ainl eval '(* 6 7)'   # 42
```

`doctor` runs the interpreter, the stdlib, the grammar export, all three
transpilers, and the AOT code generator. The installer **refuses to install
anything it cannot verify**: a release with no `SHA256SUMS`, or an asset whose
checksum does not match, is an error rather than a silent install.

<details>
<summary>Other install paths</summary>

Prebuilt static binaries (Linux x86_64 musl, macOS aarch64) with checksums are
attached to [GitHub releases](https://github.com/GRITui/ai-lang/releases). Or
build from source with the [Rust toolchain](https://rustup.rs):

```sh
cargo install --git https://github.com/GRITui/ai-lang ainl-cli
# or, from a checkout:
cargo build --release && ./target/release/ainl doctor
```

`ainl compile` additionally needs a host C compiler. Without one, `ainl doctor`
reports it as **SKIP**, not a failure — everything else works without `cc`.

</details>

## Quick start

```sh
cargo build --release

ainl run examples/corpus/countdown.ainl         # a loop — start here
ainl run examples/corpus/word-frequency.ainl    # maps, counting, a sort
ainl run examples/wordcount/main.ainl          # multi-file: (import "lib/...")
ainl run examples/http/http-demo.ainl           # HTTP client (interpreter-only; see below)
ainl repl                                      # interactive REPL
ainl repl --stdin < session.ainl > out.txt     # scriptable REPL session
ainl compile examples/fib.ainl -o fib && ./fib # AOT → native binary
ainl transpile examples/fib.ainl --to python  # → python | js | ruby
ainl ast examples/fib.ainl --json             # stable JSON + source spans
ainl grammar --gbnf                          # the constrained-decoding grammar
```

Ten complete programs live in [`examples/`](examples/README.md), each one
runnable, commented, and checked in CI on every backend it claims to support.

## The language in 10 seconds

```lisp
(def sq (fn (x) (* x x)))
(print (sq 12))            ; 144

(def fib (fn (n)
  (if (< n 2) n
    (+ (fib (- n 1)) (fib (- n 2))))))
(print (fib 20))          ; 6765

(def sum (fn (& xs)        ; & collects extra args
  (def total 0)
  (def go (fn (lst acc)
    (if (= (len lst) 0) acc
      (go (rest lst) (+ acc (first lst))))))
  (go xs 0)))
(print (sum 1 2 3 4 5))   ; 15
```

## Layout

```
ai-lang/
├── docs/
│   ├── PIPELINE.md        # generate → validate → compile → run
│   ├── GENERATION.md      # the constrained-decoding experiments + verdict
│   ├── SYNTAX.md          # the grammar, written for model ingestion
│   ├── PERFORMANCE.md     # interpreter / VM / AOT speed
│   ├── NUMERIC_MODEL.md   # the value model and its cross-target divergence
│   ├── BENCHMARK.md       # token density (the negative result)
│   └── RELEASE.md         # release pipeline, checksums
├── crates/
│   ├── ainl-core/         # lexer + parser + AST + VM + evaluator (zero deps)
│   ├── ainl-cc/           # AINL → C codegen (AOT backend)
│   ├── ainl-transpile/    # AINL → Python / JavaScript / Ruby
│   └── ainl-cli/          # the `ainl` binary
├── examples/              # sample .ainl programs
│   ├── README.md          # the worked-example corpus: 10 runnable programs
│   ├── corpus/            # …the programs themselves
│   └── few-shot.txt       # …generated from them; the `ainl gen` prompt source
├── scripts/
│   ├── gen-harness/       # the constrained-decoding harness (not in CI)
│   ├── install.sh         # the one-line installer
│   └── check-*.sh         # the CI gates, runnable locally
└── Cargo.toml             # Rust workspace
```

## Status

The language, the AOT backend, the stdlib, packaging, and the generation
experiments are implemented and CI-green. What is **not** claimed: that AINL
programs are more token-efficient than Python (they are not), that the
constraint makes a model write *correct* code (it does not), or that a small
local model can write AINL (it cannot — use a 27B-class model). The open items
in [docs/BACKLOG.md](docs/BACKLOG.md) are recorded as notes, not as commitments.

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your
option.
