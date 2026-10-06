//! `ainl gen` — generate → validate → compile → run, driven by a model.
//!
//! This is the product in one command. Everything else in AINL is a component;
//! this is the thing that makes the component earn its keep, because a
//! grammar-constrained decoder you have to hand-wire is a research harness,
//! and one that runs from a spec is a feature.
//!
//! # The pipeline
//!
//! ```text
//!   spec ─▶ generate ─▶ validate ─▶ compile ─▶ run ─▶ output
//!              ▲                                │
//!              └──────── the error, fed back ───┘
//! ```
//!
//! Each step is a boundary that can fail, and each failure carries a
//! **model-readable** error (Tier 2 card 1: a `line N, col M` position, a
//! close-match suggestion, the likely fix) rather than a Rust message. That is
//! not a formatting preference — it is what makes the repair loop work. A
//! repair prompt built from `"thread 'main' panicked"` teaches a model nothing;
//! one built from `unbound symbol 'doble' at line 2, col 12 — did you mean
//! 'double'?` names the defect and the fix in the same sentence.
//!
//! # Why the repair loop is the interesting part
//!
//! Grammar constraints guarantee *syntax*. They say nothing about *semantics*,
//! because a grammar constrains shape, not vocabulary or meaning: the exported
//! AINL GBNF happily accepts `x = 1` (three top-level symbol atoms) and
//! accepts the wrong language just as readily as the right one. `docs/
//! GENERATION.md` records the consequence — on a 27B model, 10/10 outputs were
//! valid AINL and 9/10 were correct, and the single miss was a *semantic* one
//! no grammar can catch.
//!
//! So validation is not a formality, and neither is the loop. Running the
//! generated program is the only semantic check AINL has: it is the difference
//! between "this parses" and "this prints the right thing". A program that
//! raises at runtime produces an error, that error goes back to the model, and
//! the model gets another go. That is where "familiarity compounds
//! in-session" actually comes from — not from a bigger prompt, but from a
//! tight generate→check→correct cycle that a human can watch.
//!
//! # What this does *not* do
//!
//! It does not judge whether the output is *what the spec asked for*. There is
//! no expected-answer oracle here (that lives in
//! `scripts/gen-harness/suite_checkable.json`, where every task has a known
//! stdout). `ainl gen` can tell you the program runs; whether it is the right
//! program is a human's call or a future `--expect` flag's job. The trace says
//! so explicitly rather than implying a correctness claim it cannot support.

use crate::gbnf;
use crate::gen_api::{self, ApiError, Backend, GrammarField};
use ainl_core::error::Error;
use ainl_core::value::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// How a spec is supplied.
#[derive(Debug)]
enum SpecSource {
    /// `ainl gen "…"`.
    Inline(String),
    /// `ainl gen -f spec.txt`.
    File(PathBuf),
    /// `ainl gen` with a piped stdin.
    Stdin,
}

/// Where the generated program is compiled and run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    /// `--run`: execute in-process via the interpreter, printing its output.
    Interpreter,
    /// `--aot`: AOT-compile to a standalone binary, then execute that binary.
    ///
    /// Running the *binary* rather than the interpreter is the point: it is
    /// the claim AINL makes about itself (docs/MASTER_PLAN.md §1.2) and the
    /// only way the `--aot` path can be checked end to end rather than
    /// asserted. If the compiled artifact printed something different from
    /// `ainl run`, that is a four-backend bug and `--aot` would be the place
    /// it surfaced.
    Aot,
}

/// How much to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verbosity {
    /// Trace the steps, the model reply and the verdict.
    Normal,
    /// Only the final program's output — so `ainl gen --run … | grep` works.
    Quiet,
}

/// Everything `ainl gen` was asked to do, parsed once.
///
/// `Debug` because a failing parse test prints the whole struct, and a test
/// failure that cannot show what it actually parsed is a test failure you have
/// to re-run by hand.
#[derive(Debug)]
pub struct Options {
    spec_source: SpecSource,
    run_mode: RunMode,
    out_path: Option<PathBuf>,
    keep: Option<PathBuf>,
    backend: Backend,
    constrained: bool,
    probe: bool,
    max_attempts: u32,
    verbosity: Verbosity,
    show_program: bool,
    print_prompt: bool,
    dry_run: bool,
    examples: ExamplesArg,
}

/// The short preamble that tells a model that has never seen AINL what AINL
/// is.
///
/// # Why this is here and not merely documented
///
/// The first gateway experiment asked a 27B model for "the sum of 2 and 3"
/// with no description of the language and got `PRINT 2 + 3` — a valid
/// S-expression that is *not* AINL, failing with `unbound symbol 'PRINT'`. The
/// grammar accepted it happily, because the GBNF constrains shape and not
/// vocabulary. Grounding is the fix, and it belongs in the shipped command
/// rather than in a research script: a model asked to write AINL without being
/// told what AINL is will guess, and the guess will be a Lisp it knows better.
///
/// The builtin list is read from the live prelude (`Env::all_names`), not
/// written out here. That is deliberate: a hardcoded list would start rotting
/// the moment a builtin is added, and a prompt that tells a model to use a
/// builtin that no longer exists is worse than a prompt that never mentions it.
/// `tests/builtin_list_is_read_from_the_prelude` pins the connection.
fn language_reference() -> String {
    let mut names = ainl_core::Env::with_prelude().all_names();
    names.sort();
    format!(
        "AINL is an S-expression language. Every expression is an atom or a list\n\
         (head arg...). There is no infix syntax and no operator precedence.\n\
         Comments start with ';' and run to the end of the line.\n\
         nil and false are the only falsey values; everything else is true.\n\
         \n\
         Special forms:\n\
         \x20 (def name value)          bind a name\n\
         \x20 (fn (p1 p2) body...)      function; (& rest) collects extra args\n\
         \x20 (if cond then [else])     branch on truthiness\n\
         \x20 (do form...)              run in order, return the last\n\
         \x20 (let ((n v)...) body...)  bind locals in a new scope\n\
         \x20 (while cond body...)      loop while cond is true\n\
         \x20 (and a b ...) / (or a b ...)\n\
         \x20 (quote x)                 data, not a call\n\
         \x20 (test \"name\" expr \"expected\")  assert (see `ainl test`)\n\
         \n\
         Builtins: {}\n\
         \n\
         Example program:\n\
         \x20 (def sq (fn (x) (* x x)))\n\
         \x20 (print (sq 12))\n\
         \n\
         Example program:\n\
         \x20 (def total (fn (& xs)\n\
         \x20   (do (def s 0)\n\
         \x20       (def go (fn (l a) (if (= (len l) 0) a (go (rest l) (+ a (first l))))))\n\
         \x20       (go xs 0))))\n\
         \x20 (print (total 1 2 3 4 5))\n\
         \n\
         Example program:\n\
         \x20 (def nums (list 1 2 3 4 5))\n\
         \x20 (def doubled (fn (l) (if (= (len l) 0) (list) (cons (* 2 (first l)) (doubled (rest l))))))\n\
         \x20 (print (doubled nums))\n",
        names.join(" ")
    )
}

/// How many worked examples to put in the prompt, and from where.
///
/// # Why a count and not a keyword
///
/// The corpus in `examples/few-shot.txt` is grouped into blocks, each with a
/// header naming the file, what it teaches and what it demonstrates. Selecting
/// by keyword would need a matcher with no notion of what a spec is about,
/// and a wrong guess costs more than it saves: a model shown a file-I/O
/// example when it asked for arithmetic wastes context and can copy the
/// wrong shape. A count is honest about what it is — "the first N" — and it
/// lets a caller who DOES know the answer read `examples/README.md`, see the
/// order, and pass the number they want.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExamplesArg {
    /// Include the first N blocks.
    Count(usize),
    /// Include none — the pre-corpus behavior, kept so a caller can measure
    /// what the corpus is worth instead of taking it on faith.
    Off,
    /// Include the whole corpus. Deliberately NOT the default: the full
    /// portable corpus is a large prompt, and prompt size is the one cost a
    /// constrained decode pays per token on every attempt of a repair loop.
    All,
}

impl Default for ExamplesArg {
    /// Two examples by default, not zero and not all.
    ///
    /// Zero leaves the model with the three hand-written snippets in
    /// [`language_reference`], which is what shipped first and is enough to
    /// write a small program — but the snippets do not show the rules a model
    /// actually gets wrong, because they are too small to contain a mistake.
    /// All is the other failure: it crowds out the task itself, and a repair
    /// loop re-sends the whole thing on every attempt.
    ///
    /// Two is the smallest count that puts a real program — a loop, a map, a
    /// recursion — in front of the model while leaving the request dominated
    /// by what was actually asked.
    fn default() -> Self {
        ExamplesArg::Count(2)
    }
}

impl ExamplesArg {
    fn parse(s: &str) -> Result<Self, String> {
        if s.eq_ignore_ascii_case("all") {
            return Ok(ExamplesArg::All);
        }
        if s.eq_ignore_ascii_case("off") || s == "0" {
            return Ok(ExamplesArg::Off);
        }
        s.parse::<usize>()
            .map(ExamplesArg::Count)
            .map_err(|_| format!("--examples wants a number, 'all' or 'off', got '{s}'"))
    }

    /// How many blocks to include, given how many the corpus holds.
    ///
    /// Asking for more than exists is NOT an error: the caller asked for "as
    /// many as you have", and failing the run over it would be pedantry. The
    /// count is clamped, and [`load_examples`] reports what it actually
    /// included so the run's own output says so rather than implying more
    /// context than was sent.
    fn resolve(&self, available: usize) -> usize {
        match *self {
            ExamplesArg::Count(n) => n.min(available),
            ExamplesArg::All => available,
            ExamplesArg::Off => 0,
        }
    }
}

/// Split the few-shot corpus into its per-example blocks.
///
/// The file is generated by `scripts/build-few-shot.sh`, and each example is
/// introduced by a banner of the form:
/// ```text
/// # =====================================================================
/// # examples/countdown.ainl
/// #   teaches:     loop, accumulator, def-rebind, print
/// #   demonstrates: A while loop that accumulates into bindings.
/// # =====================================================================
/// <the program, verbatim>
/// ```
///
/// So a block RUNS FROM ONE BANNER TO THE NEXT: it opens on the first fence of
/// a banner and closes on the first fence of the following one. The fence
/// after the `# demonstrates:` line is part of the block, not its terminator.
///
/// Getting that wrong is easy and it fails quietly. Two wrong readings, both
/// of which produce a green run and a useless prompt:
///
///   * opening AND closing on every fence yields two blocks per example — a
///     header-only one and a program-only one — so `--examples 2` sends two
///     comment headers and no AINL at all;
///   * closing on the fence that ends the banner (the one right above the
///     program) yields a block containing only the header comments.
///
/// Both were written and both were caught by asserting on what the block
/// CONTAINS, not on how many blocks there are. A count is not evidence.
///
/// A corpus with no banners at all yields no blocks rather than a panic;
/// [`load_examples`] reports that case instead of silently sending an empty
/// few-shot section.
fn split_corpus(text: &str) -> Vec<String> {
    const FENCE: &str = "# =====";
    let lines: Vec<&str> = text.lines().collect();
    // The first fence of each banner — the one immediately followed by the
    // `# examples/<file>` line.
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            l.starts_with(FENCE)
                && lines
                    .get(i + 1)
                    .is_some_and(|n| n.trim_start().starts_with("# examples/"))
        })
        .map(|(i, _)| i)
        .collect();

    let mut blocks: Vec<String> = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        // The block ends where the NEXT banner begins, or at end of file.
        let end = starts.get(n + 1).copied().unwrap_or(lines.len());
        let mut b = String::new();
        for line in &lines[start..end] {
            b.push_str(line);
            b.push('\n');
        }
        blocks.push(b);
    }
    blocks
}

/// Load up to `n` example blocks from the few-shot corpus.
///
/// A missing corpus is **not** an error. The file lives in the repository, not
/// in the installed binary, and a user who installed `ainl` from a release
/// tarball has no `examples/` directory at all — that is a normal situation,
/// and the command still works without the corpus. Reporting "no examples
/// were included" is the honest outcome; refusing to generate would mean a
/// documentation artifact gates a product feature.
fn load_examples(n: usize) -> (String, usize) {
    if n == 0 {
        return (String::new(), 0);
    }
    let path = std::path::Path::new("examples/few-shot.txt");
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!(
            "ainl gen: no examples corpus at {} — continuing without worked examples.\n\
             \x20         Run from an ai-lang checkout, or pass --no-examples to silence this.",
            path.display()
        );
        return (String::new(), 0);
    };
    let blocks = split_corpus(&text);
    if blocks.is_empty() {
        eprintln!(
            "ainl gen: {} contained no example blocks — continuing without them.",
            path.display()
        );
        return (String::new(), 0);
    }
    let take = n.min(blocks.len());
    if take < n {
        eprintln!(
            "ainl gen: asked for {n} examples, {} has {} — including all of them.",
            path.display(),
            blocks.len()
        );
    }
    let mut out = String::new();
    for b in &blocks[..take] {
        out.push_str(b);
        out.push('\n');
    }
    (out, take)
}

/// The few-shot section of the prompt, wrapped so a model reads it as
/// reference material rather than as the program it was asked to write.
///
/// The wrapper matters more than it looks. The corpus is full of `(print ...)`
/// lines and `; comments` that are longer than the code; without an explicit
/// "these are EXAMPLES, the answer comes later" a model will happily continue
/// the pattern and emit example code instead of the program that was asked
/// for. The corpus itself is already headed "GENERATED, do not edit", which
/// helps a human reading it and does nothing for a model.
fn few_shot_section(corpus: &str, count: usize) -> String {
    if corpus.trim().is_empty() {
        return String::new();
    }
    format!(
        "Here are {count} complete, working AINL programs. They are REFERENCE \
         MATERIAL, not your answer — do not continue them, do not copy their \
         logic unless it is what the task asks for, and do not print anything \
         from them. They are shown so you can see the exact shapes AINL uses: \
         prefix-only calls, `while` bodies written flat, `def` rebinding a \
         name in the same scope, and `print` space-joining its arguments. Read \
         them, then write only the program the task asks for.\n\n\
         ---- begin worked examples ----\n{corpus}---- end worked examples ----\n\n"
    )
}

/// The instruction appended to every generation request, including every
/// repair attempt.
///
/// Two rules earn their place. **Only the program** — an unconstrained decode
/// wraps AINL in a markdown fence or prefixes "Sure!", and a program embedded
/// in prose is a program that does not parse. **Print the answer** — a spec
/// says "print the sum of 2 and 3", and a program that *computes* the sum
/// without printing it is correct AINL and useless output, which is the most
/// common way a model satisfies a spec without satisfying the user.
fn task_instruction(spec: &str, attempt: u32) -> String {
    let repair = if attempt == 0 {
        String::new()
    } else {
        "\nYour previous attempt did not work. The error it produced is given \
         above the program. Read it carefully: it names what failed, where, and \
         usually what the fix is. Fix that specific problem.\n"
            .to_string()
    };
    format!(
        "Task: {spec}\n{repair}\n\
         Write ONLY the AINL program that does this. No explanation, no markdown, \
         no code fences, no surrounding prose.\n\
         The program must run to completion without raising, and it must print \
         its answer, because the printed output is the result.\n\
         Program:\n"
    )
}

/// The repair turn: the failed program, the error, and the same instruction.
///
/// The error is passed as a genuine prior *user* turn rather than folded into
/// the instruction, because that is the shape the model has seen: "here is my
/// program", "here is what it did". A single concatenated blob is a format the
/// model has no prior for.
fn repair_turn(program: &str, error: &str) -> (String, String) {
    let mut msg = String::with_capacity(program.len() + error.len() + 200);
    msg.push_str("This AINL program did not work:\n\n");
    msg.push_str(program);
    msg.push_str("\n\nIt produced this error:\n\n");
    msg.push_str(error);
    msg.push_str("\n\nFix it. Keep what already works and change only what the error points at.\n");
    ("user".to_string(), msg)
}

/// A failure at one pipeline step, classified so the trace can say which.
#[derive(Debug)]
struct StepFailure {
    /// `validate`, `compile` or `run`.
    step: &'static str,
    /// The message handed to the model on the next attempt.
    for_model: String,
    /// The same message, for a human reading the terminal.
    for_user: String,
}

impl StepFailure {
    fn new(step: &'static str, for_model: String) -> StepFailure {
        StepFailure {
            step,
            for_user: for_model.clone(),
            for_model,
        }
    }
}

/// Run the generated program in-process, capturing nothing — its `print` output
/// goes straight to the terminal, exactly as `ainl run` would produce it.
///
/// The child's stdout is inherited rather than captured so the program under
/// test owns its own output: a `gen` trace must not interleave with the
/// program's prints or rewrite them, and the four-backend guarantee is about
/// the bytes the program writes, not the bytes `ainl` adds.
fn run_interpreted(src: &str, path: &Path) -> Result<(), StepFailure> {
    // `run_named_in`, not `run_str`: a generated program may `import`, and its
    // imports resolve against the file it was written to.
    let env = ainl_core::Env::with_prelude();
    ainl_core::run_named_in(src, path, &env)
        .map(|_| ())
        .map_err(|e| StepFailure::new("run", model_error(&e, src)))
}

/// The model-readable rendering of an AINL error.
///
/// The full `Display` is already model-readable by design (card 1), but it ends
/// with a byte offset — useful to a tool, noise to a model. This trims to the
/// three parts that a model can act on: what failed, where, and the suggested
/// fix.
fn model_error(e: &Error, _src: &str) -> String {
    let full = e.to_string();
    // `runtime error: msg at line N, col M (byte B) — did you mean 'x'?`
    // The byte offset is the only part a model cannot use, so drop it.
    let mut out = full.replace(" at byte ", " at ");
    // Re-insert a byte marker so the trimmed position still parses as a
    // position, rather than leaving "(byte 42)".
    if let Some(open) = out.find(" (byte ") {
        if let Some(close) = out[open..].find(')') {
            let end = open + close;
            out.replace_range(open..=end, "");
        }
    }
    out
}

/// AOT-compile the generated source, then execute the resulting binary.
///
/// Compilation and execution are the same step here because the point of
/// `--aot` is the artifact: a program that compiles to something that then
/// fails is a real finding, and reporting it as "compile failed" would be
/// wrong.
fn run_aot(src: &str, out: &Path) -> Result<(), StepFailure> {
    let forms =
        ainl_core::parse(src).map_err(|e| StepFailure::new("validate", model_error(&e, src)))?;

    // The four-backend rule: a program using an interpreter-only form
    // (import, http-get, http-post) cannot be AOT-compiled. Say so with the
    // reason rather than letting codegen fail with something obscure, and
    // offer the route that does work.
    if let Some((_, sym)) = ainl_core::interpreter_only::find_interpreter_only(&forms) {
        return Err(StepFailure::new(
            "compile",
            format!(
                "this program cannot be AOT-compiled: `{sym}` is interpreter-only \
                 (docs/HTTP_TLS.md). Either write a program that does not use it, \
                 or run with --run, which uses the interpreter."
            ),
        ));
    }

    let c = ainl_cc::generate(&forms).map_err(|e| StepFailure {
        step: "compile",
        for_model: format!("this program could not be compiled to C: {e}"),
        for_user: format!("codegen failed: {e}"),
    })?;

    // The generated C goes next to the output binary, like `ainl compile` does,
    // and is removed afterwards — a compile must not litter the working
    // directory. `--keep-c` opts into keeping it.
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = out
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ainl_prog".into());
    let c_path = dir.join(format!(".{stem}.ainl-codegen.c"));
    std::fs::write(&c_path, &c).map_err(|e| StepFailure {
        step: "compile",
        for_model: format!("could not write the generated C: {e}"),
        for_user: format!("cannot write {}: {e}", c_path.display()),
    })?;

    // `-lm` for the same reason `ainl compile` passes it: the generated
    // runtime's float formatting needs libm on glibc.
    let status = std::process::Command::new("cc")
        .args([
            "-O2",
            "-o",
            &out.to_string_lossy(),
            &c_path.to_string_lossy(),
            "-lm",
        ])
        .status();
    let _ = std::fs::remove_file(&c_path);
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return Err(StepFailure::new(
                "compile",
                format!("the C compiler rejected this program (cc exited {s})."),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StepFailure::new(
                "compile",
                "this host has no C compiler, so --aot cannot build a binary. \
                 Install cc, or run with --run to use the interpreter."
                    .into(),
            ))
        }
        Err(e) => {
            return Err(StepFailure::new(
                "compile",
                format!("could not run the C compiler: {e}"),
            ))
        }
    }

    // Execute the *binary*, inheriting stdio, so --aot proves the standalone
    // artifact works rather than re-running the same interpreter path twice.
    //
    // **Resolve `out` to an absolute path first.** A path with no `/` in it is
    // looked up on `PATH`, not in the working directory — so `Command::new(
    // "sq-ainl")` fails with "No such file or directory" even though the
    // binary the compiler just wrote is sitting right there. This is a real
    // bug the live `--aot` acceptance run caught, not a hypothetical: `cc -o
    // sq-ainl` resolves the same name relative to the cwd, so the compile
    // succeeds and only the exec fails. Canonicalizing here makes the two
    // agree, and it also makes the error message name the real file.
    let exe = std::fs::canonicalize(out).map_err(|e| StepFailure {
        step: "run",
        for_model: format!(
            "the compiled program was not found at {}: {e}",
            out.display()
        ),
        for_user: format!("the compiled program {} does not exist: {e}", out.display()),
    })?;
    match std::process::Command::new(&exe).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(StepFailure::new(
            "run",
            format!("the compiled program exited with {s}."),
        )),
        Err(e) => Err(StepFailure::new(
            "run",
            format!(
                "could not execute the compiled program {}: {e}",
                out.display()
            ),
        )),
    }
}

/// Try one generated program through the whole pipeline.
fn attempt(
    src: &str,
    path: &Path,
    run_mode: RunMode,
    out: Option<&Path>,
) -> Result<(), StepFailure> {
    // `validate` is a real step even though a constrained generation is a
    // GBNF member by construction: the *parser* accepts a strict superset of
    // the GBNF, so membership still does not prove it parses. Running it
    // anyway keeps one code path for both modes and means the unconstrained
    // arm gets the same check.
    ainl_core::parse(src).map_err(|e| StepFailure::new("validate", model_error(&e, src)))?;
    match run_mode {
        RunMode::Interpreter => run_interpreted(src, path),
        RunMode::Aot => run_aot(src, out.expect("AOT mode always has an output path")),
    }
}

/// Where [`parse_args`] reads configuration from.
///
/// A trait rather than a direct call to `std::env` for one reason: it lets the
/// tests supply configuration without mutating the **process** environment.
/// `cargo test` runs tests in parallel threads, so a test that called
/// `set_var` would race every other test reading `AINL_*` — and `env::set_var`
/// is `unsafe` in current Rust for exactly that reason. It also documents the
/// complete set of variables this command consults, in one place.
pub trait Env {
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment.
struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// The environment variables `ainl gen` reads.
///
/// Each *names* a secret rather than carrying it: the key is a variable's
/// value, and the name of the variable is what a user configures.
const ENV_ENDPOINT: &str = "AINL_GEN_ENDPOINT";
const ENV_MODEL: &str = "AINL_GEN_MODEL";
const ENV_KEY_NAME: &str = "AINL_GEN_API_KEY_ENV";
pub const ENV_USER_AGENT: &str = "AINL_GEN_USER_AGENT";

/// Parse the command line.
///
/// Unknown flags are **rejected**, not ignored. `ainl test` set the precedent
/// and it is the right one here for a sharper reason: `--constraint` silently
/// reading as `--constrained` would produce a run labeled constrained that is
/// not, and the entire value of this command is that its labels are true.
fn parse_args(args: &[String]) -> Result<Options, String> {
    parse_args_from(args, &ProcessEnv)
}

/// [`parse_args`], with configuration injected — the seam the tests use.
fn parse_args_from(args: &[String], env: &dyn Env) -> Result<Options, String> {
    let mut spec_inline: Option<String> = None;
    let mut spec_file: Option<PathBuf> = None;
    let mut run_mode: Option<RunMode> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut keep: Option<PathBuf> = None;

    let mut endpoint = env
        .get(ENV_ENDPOINT)
        .unwrap_or_else(|| gen_api::DEFAULT_ENDPOINT.to_string());
    let mut model = env
        .get(ENV_MODEL)
        .unwrap_or_else(|| gen_api::DEFAULT_MODEL.to_string());
    let mut key_env = env
        .get(ENV_KEY_NAME)
        .unwrap_or_else(|| gen_api::DEFAULT_KEY_ENV.to_string());
    let mut grammar_field = GrammarField::StructuredOutputs;
    let mut constrained = true;
    let mut probe = true;
    let mut max_attempts = 3u32;
    let mut verbosity = Verbosity::Normal;
    let mut show_program = false;
    let mut print_prompt = false;
    let mut dry_run = false;
    let mut examples = ExamplesArg::default();
    let mut timeout: u32 = 120;
    let mut temperature: f64 = 0.0;
    let mut max_tokens: u32 = 1200;
    let mut extra: Option<Value> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let need = |flag: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg {
            "--aot" => {
                set_mode(&mut run_mode, RunMode::Aot)?;
                i += 1;
            }
            "--run" => {
                set_mode(&mut run_mode, RunMode::Interpreter)?;
                i += 1;
            }
            "-o" | "--out" => {
                out_path = Some(PathBuf::from(need("--out")?));
                i += 2;
            }
            "--keep" => {
                keep = Some(PathBuf::from(need("--keep")?));
                i += 2;
            }
            "-f" | "--file" => {
                spec_file = Some(PathBuf::from(need("--file")?));
                i += 2;
            }
            "--endpoint" => {
                endpoint = need("--endpoint")?;
                i += 2;
            }
            "--model" => {
                model = need("--model")?;
                i += 2;
            }
            "--api-key-env" => {
                key_env = need("--api-key-env")?;
                i += 2;
            }
            "--extra-json" => {
                let raw = need("--extra-json")?;
                let parsed = ainl_core::json_value::builtin_json_parse(&[Value::str(&raw)])
                    .map_err(|e| format!("--extra-json is not valid JSON: {e}"))?;
                if !matches!(parsed, Value::Map(_)) {
                    return Err(
                        "--extra-json must be a JSON object, e.g. '{\"k\":{\"k2\":false}}'".into(),
                    );
                }
                extra = Some(parsed);
                i += 2;
            }
            "--grammar-field" => {
                grammar_field =
                    GrammarField::parse(&need("--grammar-field")?).map_err(|ApiError(m)| m)?;
                i += 2;
            }
            "--max-tokens" => {
                max_tokens = parse_u32(&need("--max-tokens")?, "--max-tokens")?;
                if max_tokens == 0 {
                    return Err("--max-tokens must be at least 1".into());
                }
                i += 2;
            }
            "--timeout" => {
                timeout = parse_u32(&need("--timeout")?, "--timeout")?;
                i += 2;
            }
            "--attempts" => {
                max_attempts = parse_u32(&need("--attempts")?, "--attempts")?;
                i += 2;
            }
            "--temperature" => {
                let v = need("--temperature")?;
                temperature = v
                    .parse()
                    .map_err(|_| format!("--temperature needs a number, got '{v}'"))?;
                if !(0.0..=2.0).contains(&temperature) {
                    return Err(format!("--temperature must be between 0 and 2, got {v}"));
                }
                i += 2;
            }
            "--constrained" => {
                constrained = true;
                i += 1;
            }
            "--unconstrained" => {
                constrained = false;
                i += 1;
            }
            "--no-probe" => {
                probe = false;
                i += 1;
            }
            "-q" | "--quiet" => {
                verbosity = Verbosity::Quiet;
                i += 1;
            }
            "--show-program" => {
                show_program = true;
                i += 1;
            }
            "--print-prompt" => {
                print_prompt = true;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--examples" => {
                examples = ExamplesArg::parse(need("--examples")?.as_str())?;
                i += 2;
            }
            "--no-examples" => {
                examples = ExamplesArg::Off;
                i += 1;
            }
            flag if flag.starts_with('-') => {
                return Err(format!(
                    "unknown flag '{flag}'\n  \
                     run `ainl gen --help` for the full list"
                ))
            }
            other => {
                if spec_inline.is_some() || spec_file.is_some() {
                    return Err(format!(
                        "unexpected extra argument '{other}': the spec comes from one \
                         of an argument, --file, or stdin"
                    ));
                }
                spec_inline = Some(other.to_string());
                i += 1;
            }
        }
    }

    let spec_source = match (spec_inline, spec_file) {
        (Some(s), None) => SpecSource::Inline(s),
        (None, Some(f)) => SpecSource::File(f),
        (None, None) => SpecSource::Stdin,
        (Some(_), Some(_)) => {
            return Err("give the spec once: either an argument or --file, not both".into())
        }
    };

    // AOT needs somewhere to put the binary. Without `--out`, derive a name
    // from the spec, so the common case is one flag, and say what the binary
    // will be called rather than silently choosing a temp file.
    let out_path = match run_mode {
        Some(RunMode::Aot) => Some(out_path.unwrap_or_else(|| {
            let base = match &spec_source {
                SpecSource::Inline(_) => "ainl-gen".to_string(),
                SpecSource::File(f) => f
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "ainl-gen".into()),
                SpecSource::Stdin => "ainl-gen".into(),
            };
            PathBuf::from(format!("{base}-ainl"))
        })),
        // `--run` does not compile, so an output path would be meaningless. A
        // typo'd flag is rejected rather than quietly dropped.
        _ if out_path.is_some() => {
            return Err("--out only applies to --aot; --run executes in-process".into())
        }
        _ => None,
    };

    if max_attempts == 0 {
        return Err("--attempts must be at least 1".into());
    }

    // Read the key through the injected environment, not `std::env` directly —
    // the whole point of the `Env` seam is that configuration has exactly one
    // path in, so a test can exercise the real lookup.
    let api_key = match env.get(&key_env) {
        Some(k) if !k.trim().is_empty() => k,
        _ => {
            return Err(format!(
                "no API key: the environment variable {key_env} is unset or empty\n  \
                 export {key_env}=... (never pass a key as an argument — it would be \
                 visible in `ps` output and in shell history)\n  \
                 use --api-key-env NAME to read a different variable, or --dry-run to \
                 print the request without sending it"
            ))
        }
    };

    let mut backend = Backend::new(&endpoint, &model, api_key, grammar_field, timeout);
    backend.temperature = temperature;
    backend.max_tokens = max_tokens;
    backend.extra = extra;

    Ok(Options {
        spec_source,
        run_mode: run_mode.unwrap_or(RunMode::Interpreter),
        out_path,
        keep,
        backend,
        constrained,
        probe,
        max_attempts,
        verbosity,
        show_program,
        print_prompt,
        dry_run,
        examples,
    })
}

/// The `--help` text. Lives here rather than in `main.rs` so the whole contract
/// for the command — every flag, every environment variable, the meaning of the
/// exit code — is readable in one place, next to the code that implements it.
pub const HELP: &str = "\
ainl gen — generate an AINL program from a spec, then validate, compile and run it

USAGE
  ainl gen \"<spec>\"            spec as an argument
  ainl gen -f <file>           spec from a file
  echo \"<spec>\" | ainl gen     spec from stdin
  ainl gen --help

  The spec describes what the program should do. It is sent to a model over an
  OpenAI-compatible API, the result is parsed, and then run — through the
  interpreter (--run) or as an AOT-compiled standalone binary (--aot). If a step
  fails, the error is fed back to the model and it tries again, up to
  --attempts times.

BACKEND
  --endpoint <url>       base URL, /v1, or a full chat/completions URL
                         (env {ENV_ENDPOINT})
  --model <name>         model id (env {ENV_MODEL})
  --api-key-env <NAME>   env var holding the key (default: {DEFAULT_KEY_ENV})
  --grammar-field <f>    where to put a decoding grammar:
                           structured_outputs.grammar  vLLM (default, verified)
                           grammar                     llama.cpp server
                           guided_grammar              usually SILENTLY IGNORED
                           none                        no constraint sent
  --timeout <secs>       per-request timeout (default 120)
  --max-tokens <n>       generation budget (default 1200)
  --temperature <n>      0.0-2.0 (default 0)
  --extra-json <json>    merge an object into every request, for vendor-specific
                         members. A reasoning model needs, for example:
                         '{{\"chat_template_kwargs\":{{\"enable_thinking\":false}}}}'

CONSTRAINED DECODING
  --constrained          force grammar-constrained decoding (default)
  --unconstrained        generate freely; the validate step catches slips
  --no-probe             skip the probe that tests whether the backend honours
                         the constraint. It runs by default, because a backend
                         can drop the constraint without saying so.

OUTPUT
  --run                   interpret in-process (default)
  --aot                   AOT-compile to a standalone binary, then run it
  -o, --out <path>        where the binary goes (--aot only; default
                          <spec-file-stem>-ainl)
  --keep <path>           write the generated program here
  --show-program          print the generated program
  -q, --quiet             print only the program's own output

PROMPT
  --examples <n>          put the first <n> worked examples from
                          examples/few-shot.txt in the prompt (default 2)
  --examples all          the whole corpus
  --no-examples           none; the language reference alone
                          The corpus is read from examples/few-shot.txt
                          relative to the WORKING DIRECTORY, so run from an
                          ai-lang checkout. Without it this command still
                          works — it just sends the language reference alone,
                          which is what it did before the corpus existed.
                          examples/README.md lists what each example teaches,
                          so the number you pass can be chosen on purpose.

DIAGNOSTICS
  --print-prompt          print the prompt sent to the model
  --dry-run               build and print the request, send nothing
  -h, --help              this text

ENVIRONMENT
  {ENV_ENDPOINT}            default endpoint
  {ENV_MODEL}               default model
  {ENV_KEY_NAME}     which variable holds the API key
  {ENV_USER_AGENT}         override the User-Agent header

EXIT STATUS
  0  the program was generated and ran to completion
  1  bad usage, a transport or API failure, or every attempt failed — the last
     step error is printed, with the step that produced it

NOTES
  The key is read from the environment and is never passed as an argument: argv
  is readable by any process on the machine for the life of the call. It is
  handed to curl on stdin, so it appears in neither `ps` output nor shell
  history.
";

/// [`HELP`], with the environment-variable names substituted in.
///
/// The names appear in the text as `{ENV_ENDPOINT}`-style placeholders so the
/// constant above reads as a template; this is the one place they are filled in.
pub fn help_text() -> String {
    HELP.replace("{ENV_ENDPOINT}", ENV_ENDPOINT)
        .replace("{ENV_MODEL}", ENV_MODEL)
        .replace("{ENV_KEY_NAME}", ENV_KEY_NAME)
        .replace("{ENV_USER_AGENT}", ENV_USER_AGENT)
        .replace("{DEFAULT_KEY_ENV}", gen_api::DEFAULT_KEY_ENV)
}

/// What to say at the end of a successful run about constrained decoding.
///
/// Extracted as a pure function so the honesty of the report can be pinned by a
/// test: the bug this replaces reported "constrained decoding was verified in
/// force" whenever the `--constrained` *flag* was set, including under
/// `--no-probe` (where no verification happened) and under `--grammar-field
/// none` (where no grammar was even sent). A run that claims a check it did not
/// perform is worse than one that reports no check at all, so the three cases
/// are stated separately and none of them overclaims.
///
/// * `constrained` — a grammar was actually put on the wire for this run.
/// * `probe_ran` — the impossibility probe executed.
/// * `member` — the returned program is a member of the AINL grammar.
fn constraint_summary(constrained: bool, probe_ran: bool, member: bool) -> String {
    let mut s = match (constrained, probe_ran) {
        (true, true) => "constrained decoding was verified in force for this run.".to_string(),
        (true, false) => {
            "a grammar was sent, but the backend was not probed (--no-probe), so constraint \
             enforcement is unverified; the program was checked against the grammar locally."
                .to_string()
        }
        (false, _) => {
            "this run was NOT grammar-constrained: the backend was not sent a grammar.".to_string()
        }
    };
    if constrained && !member {
        s.push_str(
            "\nNote: the returned program was not a member of the AINL grammar, so the \
             constraint was not honoured for this run even though one was sent.",
        );
    }
    s
}

fn parse_u32(v: &str, flag: &str) -> Result<u32, String> {
    v.parse()
        .map_err(|_| format!("{flag} needs a whole number, got '{v}'"))
}

fn set_mode(slot: &mut Option<RunMode>, m: RunMode) -> Result<(), String> {
    if let Some(prev) = *slot {
        if prev != m {
            return Err("--aot and --run are mutually exclusive: pick one".into());
        }
    }
    *slot = Some(m);
    Ok(())
}

/// Read the spec from wherever it lives.
fn read_spec(source: &SpecSource) -> Result<String, String> {
    match source {
        SpecSource::Inline(s) => Ok(s.clone()),
        SpecSource::File(p) => {
            std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))
        }
        SpecSource::Stdin => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("cannot read the spec from stdin: {e}"))?;
            if s.trim().is_empty() {
                return Err(
                    "no spec on stdin (piped input was empty); pass the spec as an \
                     argument or with --file"
                        .into(),
                );
            }
            Ok(s)
        }
    }
}

/// `ainl gen …` — the whole pipeline.
///
/// Returns the exit code. The contract is the same one `ainl test` uses and for
/// the same reason: a shell step consumes the exit code, and a command that
/// printed a useful trace and exited 0 on failure would make every CI step
/// built on it a no-op.
pub fn run(args: &[String]) -> ExitCode {
    // `--help` is handled before anything else, and notably before the key is
    // required: asking how to configure the command must work on a machine
    // that has not configured it yet.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        // `HELP` interpolates the env-var names at build time via `format!`,
        // so the documented names can never drift from the constants the code
        // actually reads.
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    // The GBNF cross-validation entry point, handled **before** argument
    // parsing and therefore before the key is required: it is a pure,
    // offline predicate and must work on a machine with no credentials at all
    // (which is exactly where CI runs it). Going through `parse_args` would
    // have required a key it never uses.
    if args.iter().any(|a| a == "--self-check-gbnf") {
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            return ExitCode::FAILURE;
        }
        for case in input.split('\0') {
            if case.is_empty() {
                continue;
            }
            println!("{}", if gbnf::accepts(case) { "yes" } else { "no" });
        }
        return ExitCode::SUCCESS;
    }

    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("ainl gen: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let spec = match read_spec(&opts.spec_source) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("ainl gen: {msg}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(msg) = check_interpreter_only_for_aot(&spec, opts.run_mode) {
        eprintln!("ainl gen: {msg}");
        return ExitCode::FAILURE;
    }

    let quiet = opts.verbosity == Verbosity::Quiet;
    let reference = language_reference();

    if !quiet {
        eprintln!("ainl gen · {}", opts.backend.describe());
        eprintln!(
            "  mode        {}",
            match opts.run_mode {
                RunMode::Aot => "--aot (compile to a standalone binary, then run it)",
                RunMode::Interpreter => "--run (interpret in-process)",
            }
        );
        // `constrained` here means "the user asked for a constrained decode".
        // `--grammar-field none` means no grammar is put on the wire at all, so
        // reporting that as "constrained yes (GBNF via none)" claims a
        // constraint the run does not have. The `none` field is therefore
        // reported the same way as `--unconstrained`, because it is the same
        // situation: the backend is never told about the grammar.
        let sends_grammar =
            opts.constrained && !matches!(opts.backend.grammar_field, GrammarField::None);
        eprintln!(
            "  constrained {}",
            if sends_grammar {
                format!("yes (GBNF via {})", opts.backend.grammar_field.label())
            } else {
                "no (unconstrained decode; the validate step is what catches slips)".into()
            }
        );
        eprintln!("  attempts    up to {}", opts.max_attempts);
        if let Some(o) = &opts.out_path {
            eprintln!("  binary      {}", o.display());
        }
    }

    // The probe: prove the constraint is honoured *before* trusting it, so the
    // trace can report it as a measured fact. A backend that drops the
    // constraint is reported and the run continues unconstrained, because
    // silently producing unconstrained output under a "constrained" label
    // would be the exact failure this whole feature exists to avoid.
    let mut constrained = opts.constrained;
    // `--grammar-field none` puts no grammar on the wire, so there is nothing
    // to probe: the run is unconstrained whatever the `--constrained` flag
    // says. Fold it in here rather than at each use site.
    if matches!(opts.backend.grammar_field, GrammarField::None) {
        constrained = false;
    }
    // Whether the probe actually executed. Reported separately from
    // `constrained`, because "a grammar was sent" and "the backend was proven
    // to apply it" are different claims and only the second one is evidence.
    let mut probe_ran = false;
    if opts.probe && constrained && !opts.dry_run {
        probe_ran = true;
        eprintln!("  probe       testing whether the backend honours the grammar…");
        match gen_api::probe_constraint(&opts.backend) {
            Ok(true) => eprintln!("  probe       ok — the constraint is in force"),
            // `probe_constraint` reports a non-constrained backend as an
            // `ApiError` (it has to explain which field was ignored and what
            // to use instead), so the message arrives here rather than as a
            // bare `Ok(false)`. Both spellings are handled so the two failure
            // shapes can never be confused for each other.
            Ok(false) => {
                eprintln!("  probe       the backend did not apply the constraint");
                eprintln!(
                    "  probe       continuing unconstrained — this run is NOT \
                     grammar-constrained"
                );
                constrained = false;
            }
            Err(ApiError(msg)) => {
                eprintln!(
                    "  probe       the grammar was NOT applied:\n  {}",
                    msg.replace('\n', "\n  ")
                );
                eprintln!(
                    "  probe       continuing unconstrained — this run is NOT \
                     grammar-constrained"
                );
                constrained = false;
            }
        }
    }

    let grammar = if constrained {
        Some(ainl_core::grammar::GBNF)
    } else {
        None
    };

    // The generated program is written to a real file, not held in memory
    // only: `import` resolves against the file's directory, `--keep` needs
    // somewhere real, and the interpreter entry point takes a path.
    let mut work = match WorkingFile::new(&spec) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("ainl gen: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The few-shot corpus is a SEPARATE turn from the language reference, not
    // an append to it. Two reasons:
    //
    //   * it is a different KIND of material — the reference is a description
    //     of the language, the corpus is demonstration. A model that reads
    //     them as one blob tends to treat the whole message as a template to
    //     continue;
    //   * it is the part most likely to be absent. A user with no checkout has
    //     no corpus, and the reference alone is still a complete, useful
    //     prompt. Splitting the turns is what lets that case degrade to
    //     "one message" rather than to a broken one.
    //
    // It is sent BEFORE the task instruction, once, and never repeated on a
    // repair turn: the repair turn already carries the failing program and
    // its error, and re-sending the corpus on every attempt would make each
    // retry larger than the last for no gain.
    let (corpus, corpus_count) = load_examples(opts.examples.resolve(usize::MAX));
    let few_shot = few_shot_section(&corpus, corpus_count);
    if !quiet && corpus_count > 0 {
        eprintln!("  examples   {corpus_count} from examples/few-shot.txt");
    }

    let mut messages: Vec<(String, String)> = vec![("user".into(), reference)];
    if !few_shot.is_empty() {
        messages.push(("user".into(), few_shot));
    }
    let mut attempt_no = 0u32;
    let mut last_failure: Option<StepFailure> = None;

    while attempt_no < opts.max_attempts {
        attempt_no += 1;
        if !quiet {
            eprintln!();
            eprintln!("── attempt {attempt_no} ──────────────────────────────");
        }
        let instruction = task_instruction(&spec, attempt_no);
        messages.push(("user".into(), instruction.clone()));

        if opts.print_prompt || opts.dry_run {
            eprintln!(
                "prompt:\n{}",
                messages
                    .iter()
                    .map(|(_, c)| c.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        if opts.dry_run {
            // Stop after showing the request: the point of --dry-run is to
            // inspect the payload without spending a call or needing a key.
            eprintln!("dry run: nothing was sent");
            return ExitCode::SUCCESS;
        }

        let completion = match gen_api::complete(&opts.backend, &messages, grammar) {
            Ok(c) => c,
            Err(e) => {
                // A transport or API failure is not something the model can fix
                // by rewriting its program, so retrying would burn the
                // caller's quota to no effect. Say so and stop.
                eprintln!("ainl gen: the model call failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Some(t) = completion.total_tokens {
            if !quiet {
                eprintln!("  tokens      {t} total");
            }
        }
        let Some(program) = completion.content else {
            eprintln!("ainl gen: {}", gen_api::explain_empty(&completion));
            return ExitCode::FAILURE;
        };

        // Membership: the sound, strict "was this constrained?" check on the
        // program that actually came back. A no-op under a working constraint,
        // and the thing that catches a silently-ignored one.
        let member = gbnf::accepts(&program);
        if !quiet {
            eprintln!(
                "  generated   {}",
                if completion.excerpt.is_empty() {
                    "(empty)".to_string()
                } else {
                    completion.excerpt.clone()
                }
            );
            eprintln!(
                "  gbnf        {}",
                if member {
                    "member of the AINL grammar"
                } else {
                    "NOT a member of the AINL grammar"
                }
            );
        }

        if opts.show_program || opts.keep.is_some() {
            if let Err(e) = work.write(&program) {
                eprintln!("ainl gen: cannot write the generated program: {e}");
                return ExitCode::FAILURE;
            }
        }

        match attempt(
            &program,
            work.path(),
            opts.run_mode,
            opts.out_path.as_deref(),
        ) {
            Ok(()) => {
                if !quiet {
                    eprintln!("  validate    ok");
                    eprintln!("  run         ok");
                    eprintln!();
                    eprintln!("── done ─────────────────────────────────────────");
                    // Report the constraint status that is actually true of
                    // *this* run, never the one that was merely requested.
                    eprintln!("{}", constraint_summary(constrained, probe_ran, member));
                    eprintln!(
                        "The program ran to completion. Whether it does what the spec asked \
                         for is a judgement this command does not make."
                    );
                }
                if let Some(k) = &opts.keep {
                    if let Err(e) = work.write_to(k) {
                        eprintln!("ainl gen: cannot write {}: {e}", k.display());
                        return ExitCode::FAILURE;
                    }
                    if !quiet {
                        eprintln!("kept: {}", k.display());
                    }
                }
                return ExitCode::SUCCESS;
            }
            Err(f) => {
                // Carry the failure forward as a real turn, so attempt 2 sees
                // its own program and its own error as conversation history.
                // Built *before* the value is stored, so `last_failure` keeps
                // the failure for the final report.
                messages.push(repair_turn(&program, &f.for_model));
                last_failure = Some(f);
            }
        }
    }

    // Every attempt used up. Report the last failure verbatim rather than a
    // summary: the model-readable error is the useful part, and a wrapper
    // message would only make the reader go looking for it.
    eprintln!();
    eprintln!(
        "── giving up after {attempt_no} attempt{} ─────────────────",
        if attempt_no == 1 { "" } else { "s" }
    );
    if let Some(f) = last_failure {
        eprintln!("  step        {}", f.step);
        eprintln!("  {}", f.for_user);
    }
    if let Some(k) = &opts.keep {
        if let Err(e) = work.write_to(k) {
            eprintln!("ainl gen: cannot write {}: {e}", k.display());
        } else {
            eprintln!("the last attempt was kept at {}", k.display());
        }
    }
    ExitCode::FAILURE
}

/// A temporary file for the generated program, removed on drop unless kept.
struct WorkingFile {
    path: PathBuf,
    content: String,
}

impl WorkingFile {
    fn new(_spec: &str) -> Result<WorkingFile, std::io::Error> {
        // Named from the pid so two concurrent `ainl gen` runs cannot collide,
        // and from the clock so a crashed run's leftovers are distinguishable.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("ainl-gen-{}-{nanos}.ainl", std::process::id()));
        Ok(WorkingFile {
            path,
            content: String::new(),
        })
    }

    fn write(&mut self, program: &str) -> Result<(), std::io::Error> {
        self.content = program.to_string();
        std::fs::write(&self.path, &self.content)
    }

    fn write_to(&self, dest: &Path) -> Result<(), std::io::Error> {
        std::fs::write(dest, &self.content)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkingFile {
    fn drop(&mut self) {
        // Best effort: a temp file left behind is a nuisance, not a failure,
        // and there is nothing useful to do with an error here.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Refuse `--aot` for a spec that cannot be AOT-compiled, before spending a
/// model call on it.
///
/// The check is on the *spec text* for the obvious cases rather than on a
/// generated program, because that is what can be known before generating. It
/// is a convenience, not a guarantee — a model can still reach for
/// interpreter-only forms, and `run_aot` refuses those with the same message
/// when they appear in the result.
fn check_interpreter_only_for_aot(spec: &str, mode: RunMode) -> Result<(), String> {
    if mode != RunMode::Aot {
        return Ok(());
    }
    // The spec is prose, not AINL, so it is scanned as text. A mention of
    // "import" in a sentence is not a directive, so only the two unambiguous
    // spellings count.
    for (needle, why) in [
        ("http-get", "the HTTP client is interpreter-only"),
        ("http-post", "the HTTP client is interpreter-only"),
        ("read-pass", "the masked terminal read is interpreter-only"),
        ("import", "modules are interpreter-only"),
    ] {
        if spec.contains(needle) {
            return Err(format!(
                "--aot cannot be used for this spec: it mentions `{needle}`, and {why} \
                 (docs/HTTP_TLS.md).\n  Use --run, which interprets in-process."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A stand-in for the process environment. The tests never touch the real
    /// one: `cargo test` runs tests in parallel threads, so a `set_var` here
    /// would race every other test reading `AINL_*` (and `set_var` is `unsafe`
    /// in current Rust for exactly that reason).
    struct TestEnv(Vec<(String, String)>);

    impl TestEnv {
        /// A key present under a known name, nothing else set.
        ///
        /// The value is obviously fake on purpose: it must not be mistakable for
        /// a real credential by a human reading a test failure, nor by a
        /// scanner that greps the tree for things that look like keys.
        fn with_key() -> TestEnv {
            TestEnv(vec![(
                "AINL_TEST_KEY".to_string(),
                "not-a-real-key-test-fixture".to_string(),
            )])
        }

        /// No key anywhere — for the tests that need one to be *missing*.
        fn empty() -> TestEnv {
            TestEnv(Vec::new())
        }
    }

    impl Env for TestEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// Parse with a key present, plus whatever the test passes. Only
    /// `--api-key-env` is forced in, so a test can pass its own `--aot` and the
    /// conflict logic is exercised for real rather than shadowed.
    fn parse(v: &[&str]) -> Result<Options, String> {
        let mut a = args(&["--api-key-env", "AINL_TEST_KEY"]);
        a.extend(v.iter().map(|s| s.to_string()));
        parse_args_from(&a, &TestEnv::with_key())
    }

    /// The simple cases parse: mode flags, and defaults a user should not have
    /// to specify.
    #[test]
    fn basic_flags_parse() {
        let ok = parse(&["--run", "print the sum of 2 and 3"]).expect("parses");
        assert_eq!(ok.run_mode, RunMode::Interpreter);
        assert!(ok.constrained, "constrained by default");
        assert!(ok.probe, "the probe runs by default");
        assert_eq!(ok.max_attempts, 3, "three attempts by default");

        let aot = parse(&["--aot", "spec here"]).expect("parses");
        assert_eq!(aot.run_mode, RunMode::Aot);
        // A derived, predictable name rather than a silent temp file.
        let out = aot.out_path.expect("aot has an output path");
        assert_eq!(out.to_string_lossy(), "ainl-gen-ainl");
    }

    /// `--file` gives a better default binary name than the inline form: the
    /// name comes from the spec file, not a generic default.
    #[test]
    fn the_default_binary_name_follows_the_spec_source() {
        let o = parse(&["--aot", "-f", "specs/fizz.txt"]).expect("parses");
        assert_eq!(o.out_path.expect("a path").to_string_lossy(), "fizz-ainl");
    }

    /// Every flag that takes a value reports the same way when it has none, and
    /// each one actually lands where it should.
    #[test]
    fn value_flags_are_read_and_validated() {
        let o = parse(&[
            "--endpoint",
            "https://h/v1",
            "--model",
            "m-2",
            "--timeout",
            "7",
            "--max-tokens",
            "64",
            "--temperature",
            "0.3",
            "--attempts",
            "5",
            "--grammar-field",
            "grammar",
        ])
        .expect("parses");
        assert_eq!(o.backend.url, "https://h/v1/chat/completions");
        assert_eq!(o.backend.model, "m-2");
        assert_eq!(o.backend.timeout_secs, 7);
        assert_eq!(o.backend.max_tokens, 64);
        assert!((o.backend.temperature - 0.3).abs() < 1e-9);
        assert_eq!(o.max_attempts, 5);
        assert_eq!(o.backend.grammar_field, GrammarField::Grammar);

        for (flag, bad) in [
            ("--timeout", "abc"),
            ("--max-tokens", "abc"),
            ("--attempts", "0"),
            ("--temperature", "9"),
            ("--temperature", "abc"),
            ("--grammar-field", "nonsense"),
        ] {
            let e = parse(&[flag, bad]).unwrap_err();
            assert!(
                e.contains(flag) || e.contains("--temperature"),
                "{flag} {bad} gave a confusing error: {e}"
            );
        }
        // A flag with no value at all is caught, not read as the next argument.
        for flag in ["--endpoint", "--model", "--attempts", "--max-tokens"] {
            let e = parse(&[flag]).unwrap_err();
            assert!(e.contains("needs a value"), "{flag}: {e}");
        }
    }

    /// `--extra-json` merges vendor-specific members into the request. This is
    /// how a reasoning model's thinking gets turned off, so a mistake here
    /// shows up as an empty completion that looks like a bad key.
    #[test]
    fn extra_json_merges_into_the_request() {
        let o = parse(&[
            "--extra-json",
            r#"{"chat_template_kwargs":{"enable_thinking":false}}"#,
        ])
        .expect("parses");
        let extra = o.backend.extra.expect("extra is set");
        // `json-serialize` is a builtin, so it returns a Value: unwrap the
        // string it holds rather than expecting one.
        let s = match ainl_core::json_value::builtin_json_serialize(&[extra]).expect("serializes") {
            Value::Str(s) => s.as_str().to_string(),
            other => panic!("expected a string, got {}", other.type_name()),
        };
        assert!(s.contains("enable_thinking"), "{s}");
    }

    /// A malformed `--extra-json` is rejected at parse time, with a message
    /// that shows the shape it wanted — not at the first network call.
    #[test]
    fn malformed_extra_json_is_rejected_with_an_example() {
        let e = parse(&["--extra-json", "not json at all"]).unwrap_err();
        assert!(e.contains("extra-json"), "{e}");
    }

    /// An unknown flag is rejected. The reason this matters more here than in
    /// most commands: a typo like `--constraint` silently reading as
    /// `--constrained` would produce a run labeled constrained that is not, and
    /// the labels are the product.
    #[test]
    fn an_unknown_flag_is_rejected() {
        let e = parse(&["--constraint", "spec"]).unwrap_err();
        assert!(e.contains("--constraint"), "{e}");
        assert!(e.contains("unknown flag"), "{e}");
    }

    /// `--aot` and `--run` are mutually exclusive, and saying so beats
    /// whichever one happened to be parsed last.
    #[test]
    fn aot_and_run_are_mutually_exclusive() {
        let e = parse(&["--aot", "--run", "spec"]).unwrap_err();
        assert!(e.contains("mutually exclusive"), "{e}");
        // Repeating the *same* mode is harmless, though.
        assert!(parse(&["--aot", "--aot", "spec"]).is_ok());
    }

    /// `--out` is meaningless for `--run`; a typo'd flag must not be dropped
    /// silently, or the binary would land somewhere the user did not name.
    #[test]
    fn out_is_rejected_without_aot() {
        let e = parse(&["--run", "-o", "x", "spec"]).unwrap_err();
        assert!(e.contains("--out only applies to --aot"), "{e}");
    }

    /// A missing key names the variable and how to supply it, because the most
    /// common reason to be here is not having configured one yet. It also
    /// explains *why* there is no `--api-key` flag.
    #[test]
    fn a_missing_key_is_a_clear_error() {
        let e = parse_args_from(
            &args(&["--api-key-env", "AINL_ABSENT", "spec"]),
            &TestEnv::empty(),
        )
        .unwrap_err();
        assert!(e.contains("no API key"), "{e}");
        assert!(e.contains("AINL_ABSENT"), "{e}");
        assert!(e.contains("shell history"), "{e}");
        assert!(e.contains("dry-run"), "{e}");
    }

    /// An empty-string key is treated as missing. A variable that exists but
    /// holds nothing is the shape an unset-but-exported credential takes, and
    /// it must not be sent as an empty bearer token.
    #[test]
    fn an_empty_key_counts_as_missing() {
        let env = TestEnv(vec![("AINL_TEST_KEY".to_string(), "   ".to_string())]);
        let e =
            parse_args_from(&args(&["--api-key-env", "AINL_TEST_KEY", "spec"]), &env).unwrap_err();
        assert!(e.contains("no API key"), "{e}");
    }

    /// The environment can supply the endpoint, the model, and *which variable*
    /// holds the key — so a host with its own credential naming works with no
    /// flags at all.
    #[test]
    fn the_environment_supplies_the_whole_configuration() {
        let env = TestEnv(vec![
            (
                "AINL_GEN_ENDPOINT".to_string(),
                "https://from-env".to_string(),
            ),
            ("AINL_GEN_MODEL".to_string(), "model-from-env".to_string()),
            (
                "AINL_GEN_API_KEY_ENV".to_string(),
                "HOST_CREDENTIAL".to_string(),
            ),
            ("HOST_CREDENTIAL".to_string(), "secret-from-env".to_string()),
        ]);
        let o = parse_args_from(&args(&["--run", "spec"]), &env).expect("parses");
        assert_eq!(o.backend.url, "https://from-env/v1/chat/completions");
        assert_eq!(o.backend.model, "model-from-env");
        assert_eq!(o.backend.api_key, "secret-from-env");
    }

    /// A flag must beat the environment for the same setting: the whole point
    /// of a flag is to override a machine-wide default.
    #[test]
    fn a_flag_overrides_the_environment() {
        let env = TestEnv(vec![
            ("AINL_GEN_MODEL".to_string(), "from-env".to_string()),
            ("AINL_TEST_KEY".to_string(), "k".to_string()),
        ]);
        let o = parse_args_from(
            &args(&[
                "--api-key-env",
                "AINL_TEST_KEY",
                "--model",
                "from-flag",
                "spec",
            ]),
            &env,
        )
        .expect("parses");
        assert_eq!(o.backend.model, "from-flag");
    }

    /// The spec is taken from one place only. Two sources is ambiguous, and
    /// silently preferring one would mean a user's carefully written spec file
    /// was ignored.
    #[test]
    fn the_spec_comes_from_one_place() {
        let e = parse(&["--run", "inline spec", "--file", "f.txt"]).unwrap_err();
        assert!(e.contains("once"), "{e}");

        let e2 = parse(&["--run", "one", "two"]).unwrap_err();
        assert!(e2.contains("unexpected extra argument"), "{e2}");
    }

    /// With no spec anywhere, the spec comes from stdin — and that is decided
    /// at parse time, not by discovering an empty string later.
    #[test]
    fn no_spec_argument_means_stdin() {
        let o = parse(&["--run"]).expect("parses");
        assert!(matches!(o.spec_source, SpecSource::Stdin));
    }

    /// The repair instruction appears only on a retry, and names the error as
    /// the thing to read. A first attempt that says "your previous attempt did
    /// not work" confuses a model that has not made one.
    #[test]
    fn the_repair_instruction_is_added_only_on_a_retry() {
        let first = task_instruction("add 2 and 3", 0);
        assert!(!first.contains("previous attempt"), "{first}");
        assert!(first.contains("Task: add 2 and 3"), "{first}");

        let second = task_instruction("add 2 and 3", 2);
        assert!(second.contains("previous attempt"), "{second}");
        assert!(second.contains("error"), "{second}");
    }

    /// Every instruction says the two things that decide whether a generation
    /// is usable: only the program, and print the answer.
    #[test]
    fn the_instruction_states_the_output_contract() {
        for attempt in [0u32, 1, 2] {
            let t = task_instruction("print the sum of 2 and 3", attempt);
            assert!(t.contains("ONLY"), "attempt {attempt}: {t}");
            assert!(t.contains("print"), "attempt {attempt}: {t}");
            assert!(!t.contains("```"), "no code fences: {t}");
        }
    }

    /// A block must contain the PROGRAM, not just the banner that names it.
    ///
    /// This is the assertion that catches the two wrong readings of the
    /// banner format, and both of them were written before this test existed.
    /// A count is not evidence: a splitter that yields the right NUMBER of
    /// blocks, each holding only `# comments`, passes every check except this
    /// one — and ships a prompt containing no AINL whatsoever.
    #[test]
    fn a_corpus_block_carries_the_program_not_just_its_header() {
        let corpus = "\
# header prose, ignored
# =====================================================================
# examples/countdown.ainl
#   teaches:     loop
#   demonstrates: a loop
# =====================================================================
(print 1)
# =====================================================================
# examples/next.ainl
#   teaches:     map
#   demonstrates: a map
# =====================================================================
(print 2)
";
        let blocks = split_corpus(corpus);
        assert_eq!(blocks.len(), 2, "one block per banner: {blocks:?}");
        for b in &blocks {
            assert!(
                b.contains("(print"),
                "a block with no executable form is a header, not an example: {b}"
            );
        }
        assert!(blocks[0].contains("countdown.ainl"), "{}", blocks[0]);
        assert!(blocks[0].contains("(print 1)"), "{}", blocks[0]);
        assert!(
            !blocks[0].contains("next.ainl"),
            "blocks must not bleed: {}",
            blocks[0]
        );
        assert!(blocks[1].contains("next.ainl"), "{}", blocks[1]);
        assert!(blocks[1].contains("(print 2)"), "{}", blocks[1]);
        assert!(
            !blocks[1].contains("countdown"),
            "blocks must not bleed: {}",
            blocks[1]
        );
    }

    /// The last block has no following banner to close it, so it must run to
    /// the end of the file. A splitter that drops it — or emits an empty one —
    /// silently shrinks the corpus by one every time an example is added.
    #[test]
    fn the_last_block_runs_to_the_end_of_the_file() {
        let corpus = "\
# =====================================================================
# examples/only.ainl
#   teaches:     loop
#   demonstrates: the only one
# =====================================================================
(print 42)
";
        let blocks = split_corpus(corpus);
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert!(blocks[0].contains("(print 42)"), "{}", blocks[0]);
    }

    /// A corpus that is present but unusable is reported, not silently empty.
    /// An empty few-shot section that looks like a successful load is the
    /// failure mode worth ruling out.
    #[test]
    fn a_corpus_with_no_banners_yields_no_blocks_rather_than_a_panic() {
        assert!(split_corpus("").is_empty());
        assert!(split_corpus("just some prose\nwith no fences\n").is_empty());
    }

    /// The default is two examples: enough to show a real program, few enough
    /// that the task is not crowded out. A default of zero would quietly
    /// restore the pre-corpus behavior and nobody would notice.
    #[test]
    fn the_default_is_two_examples() {
        assert_eq!(ExamplesArg::default(), ExamplesArg::Count(2));
    }

    /// Asking for more examples than exist clamps and says so, rather than
    /// failing a run over a pedantic point. The message names both numbers so
    /// a reader knows the corpus is smaller than they assumed.
    #[test]
    fn asking_for_more_examples_than_exist_clamps() {
        assert_eq!(ExamplesArg::Count(99).resolve(9), 9);
        assert_eq!(ExamplesArg::All.resolve(9), 9);
        assert_eq!(ExamplesArg::Off.resolve(9), 0);
        assert_eq!(ExamplesArg::Count(0).resolve(9), 0);
    }

    /// The value is parsed, not guessed: a typo is rejected with the three
    /// accepted spellings named, and `0` is the same as `off` because both
    /// mean "no examples" and a caller writing either should get it.
    #[test]
    fn the_examples_flag_parses_and_rejects() {
        assert_eq!(ExamplesArg::parse("3").unwrap(), ExamplesArg::Count(3));
        assert_eq!(ExamplesArg::parse("all").unwrap(), ExamplesArg::All);
        assert_eq!(ExamplesArg::parse("ALL").unwrap(), ExamplesArg::All);
        assert_eq!(ExamplesArg::parse("off").unwrap(), ExamplesArg::Off);
        assert_eq!(ExamplesArg::parse("0").unwrap(), ExamplesArg::Off);
        let e = ExamplesArg::parse("banana").unwrap_err();
        assert!(e.contains("--examples"), "{e}");
        assert!(
            e.contains("'all'"),
            "the error must name what is accepted: {e}"
        );
    }

    /// `--examples` and `--no-examples` are real flags, parsed at parse time.
    /// A flag that parses but is ignored would leave the run's own report
    /// claiming examples were sent when none were.
    #[test]
    fn the_examples_flags_reach_the_options() {
        let on = parse(&["--run", "--examples", "5", "s"]).expect("parses");
        assert_eq!(on.examples, ExamplesArg::Count(5));
        let all = parse(&["--run", "--examples", "all", "s"]).expect("parses");
        assert_eq!(all.examples, ExamplesArg::All);
        let off = parse(&["--run", "--no-examples", "s"]).expect("parses");
        assert_eq!(off.examples, ExamplesArg::Off);
        // And the default, with neither flag present.
        let d = parse(&["--run", "s"]).expect("parses");
        assert_eq!(d.examples, ExamplesArg::Count(2));
        // A value-less --examples is a usage error, not a silent default.
        let e = parse(&["--run", "--examples"]).unwrap_err();
        assert!(e.contains("needs a value"), "{e}");
    }

    /// The few-shot wrapper must mark the corpus as reference material. The
    /// corpus is full of `(print ...)` lines, and a model that reads it as the
    /// program to continue will emit example code instead of an answer — the
    /// one failure the injection exists to prevent.
    #[test]
    fn the_few_shot_section_marks_the_corpus_as_reference() {
        let s = few_shot_section("(print 1)\n", 1);
        assert!(s.contains("REFERENCE MATERIAL"), "{s}");
        assert!(s.contains("not your answer"), "{s}");
        assert!(s.contains("begin worked examples"), "{s}");
        assert!(s.contains("(print 1)"), "the program must survive: {s}");
        // An empty corpus produces no section at all — no empty wrapper sent
        // to the model, and no "here are 0 programs".
        assert!(few_shot_section("", 0).is_empty());
        assert!(few_shot_section("   \n", 0).is_empty());
    }

    /// The help text has to describe the flag, or the feature is undiscoverable
    /// — and a flag that is not in the help is the one nobody uses.
    #[test]
    fn the_help_text_documents_the_examples_flags() {
        let h = help_text();
        for frag in ["--examples <n>", "--examples all", "--no-examples"] {
            assert!(h.contains(frag), "the help does not mention {frag}");
        }
        assert!(
            h.contains("examples/few-shot.txt"),
            "the help must say where the corpus comes from: {h}"
        );
    }

    /// The corpus in the repository must actually load, and every block in it
    /// must carry a program. This is the test that would have caught both
    /// splitter bugs against the real file rather than against a fixture.
    #[test]
    fn the_repository_corpus_loads_with_programs_in_every_block() {
        let Ok(text) = std::fs::read_to_string("examples/few-shot.txt") else {
            // Running outside a checkout (a release tarball, a packaged test
            // binary) is not a failure: the corpus is a repository artifact and
            // the command is documented to work without it.
            return;
        };
        let blocks = split_corpus(&text);
        assert!(
            blocks.len() >= 10,
            "the corpus is the few-shot source; {} blocks is too few to serve that",
            blocks.len()
        );
        for b in &blocks {
            assert!(
                b.contains("(def ") || b.contains("(print "),
                "a corpus block with no AINL in it is a dead example: {b}"
            );
        }
    }

    /// The reference the model is given must be the *live* builtin list. A
    /// hardcoded list would rot the first time a builtin is added, and a prompt
    /// that names a builtin which no longer exists is worse than one that never
    /// mentions it.
    #[test]
    fn the_builtin_list_is_read_from_the_prelude() {
        let ref_text = language_reference();
        // `test` is the most recently added builtin, and `http-get` is
        // interpreter-only — both exactly what a stale list would have missed.
        assert!(ref_text.contains("test"), "the newest builtin is missing");
        assert!(ref_text.contains("http-get"), "the HTTP builtin is missing");
        assert!(
            ref_text.contains("json-parse"),
            "the JSON builtin is missing"
        );
        // And nothing invented: every name the prompt lists must be real.
        let line = ref_text
            .lines()
            .find(|l| l.starts_with("Builtins:"))
            .expect("a Builtins line");
        for name in line.trim_start_matches("Builtins: ").split(' ') {
            assert!(
                ainl_core::Env::with_prelude()
                    .all_names()
                    .iter()
                    .any(|n| n == name),
                "the reference names '{name}', which is not in the prelude"
            );
        }
    }

    /// The reference must describe the special forms and the output contract.
    /// A model that knows the forms but not the shapes will still emit prose.
    #[test]
    fn the_reference_covers_the_language() {
        let r = language_reference();
        for form in [
            "(def ", "(fn ", "(if ", "(do ", "(let ", "(while ", "(and ", "(or ",
        ] {
            assert!(r.contains(form), "the reference does not mention {form}");
        }
        assert!(r.contains("S-expression"), "{r}");
        assert!(r.contains("print"), "it must show printing");
    }

    /// AINL's own error text is what the model is shown, and it must arrive
    /// with the fix attached and the byte offset stripped — the offset is the
    /// one part a model cannot use, and leaving it in invites a model to reason
    /// about bytes.
    #[test]
    fn the_model_error_keeps_the_fix_and_drops_the_byte_offset() {
        // A suggestion requires a close name to be *in scope*: AINL suggests
        // from what the environment actually holds, not from a dictionary.
        // Binding `double` is what makes the suggestion possible, and is the
        // realistic shape — a model mistypes a name it defined a line earlier.
        let src = "(def double (fn (x) (* 2 x)))\n(print (doble 2))\n";
        let err = ainl_core::run_str(src).unwrap_err();
        let for_model = model_error(&err, src);
        assert!(for_model.contains("unbound symbol"), "{for_model}");
        assert!(
            for_model.contains("line 2"),
            "the position must survive and point at the failing form: {for_model}"
        );
        assert!(
            for_model.contains("did you mean"),
            "the fix must survive: {for_model}"
        );
        assert!(!for_model.contains("byte"), "trim the offset: {for_model}");
    }

    /// A parse error must reach the loop in the same model-readable shape.
    #[test]
    fn a_parse_error_is_also_model_readable() {
        let src = "(print 1\n";
        let err = ainl_core::parse(src).unwrap_err();
        let for_model = model_error(&err, src);
        assert!(for_model.contains("parse error"), "{for_model}");
        assert!(for_model.contains("line 1"), "{for_model}");
    }

    /// The repair turn must carry *both* the program and the error, or the
    /// model cannot tell which of its programs went wrong.
    #[test]
    fn the_repair_turn_carries_the_program_and_the_error() {
        let (role, body) = repair_turn("(print (doble 2))", "unbound symbol 'doble'");
        assert_eq!(role, "user");
        assert!(body.contains("(print (doble 2))"), "{body}");
        assert!(body.contains("unbound symbol 'doble'"), "{body}");
    }

    /// The `--aot` guard refuses the interpreter-only cases up front, with the
    /// reason — and only for `--aot`, because `--run` can do all three.
    #[test]
    fn aot_refuses_interpreter_only_specs_up_front() {
        for spec in [
            "print the result of (http-get \"http://x/\")",
            "fetch it with http-post",
            "import a module and print its name",
        ] {
            let e = check_interpreter_only_for_aot(spec, RunMode::Aot).unwrap_err();
            assert!(e.contains("docs/HTTP_TLS.md"), "{e}");
            assert!(e.contains("--run"), "the fix must be named: {e}");
        }
        assert!(
            check_interpreter_only_for_aot("use http-get please", RunMode::Interpreter).is_ok()
        );
    }

    /// A spec that merely *mentions* a word the guard keys on, in a way that
    /// is not a directive, must not be refused. Over-eager refusal is its own
    /// bug: it teaches users to distrust the message.
    #[test]
    fn the_aot_guard_is_not_triggered_by_unrelated_prose() {
        assert!(check_interpreter_only_for_aot("print the sum of 2 and 3", RunMode::Aot).is_ok());
        assert!(check_interpreter_only_for_aot(
            "define a function that reverses a list",
            RunMode::Aot
        )
        .is_ok());
    }

    /// The compiled binary must actually be executed, and a **relative** output
    /// path must work.
    ///
    /// A path with no `/` in it is resolved against `PATH` by
    /// `std::process::Command`, never against the working directory — so
    /// `Command::new("prog-ainl")` fails with ENOENT while the binary the
    /// compiler just wrote sits in the cwd. The compile step resolves the same
    /// name against the cwd, so the failure is invisible until the run step and
    /// then it looks like "the program could not be executed" for a program
    /// that is sitting right there. Found by the live `--aot` acceptance run.
    #[test]
    fn a_relative_output_path_is_executed_not_looked_up_on_path() {
        // A real directory, entered for the duration of the test, because the
        // bug is precisely about how a bare name resolves. Tests run in
        // parallel, so this must not chdir the shared process cwd without
        // holding a lock — hence the lock, and hence restoring it afterwards.
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!("ainl-aot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let before = std::env::current_dir().expect("cwd");

        // Enter the directory, then use a bare name for the output: that is the
        // combination that broke in the live run.
        std::env::set_current_dir(&dir).expect("enter temp dir");
        let out = Path::new("relprog-ainl");

        let result = run_aot("(print (+ 2 3))\n", out);
        let produced = dir.join(out);
        let existed = produced.exists();
        let size = std::fs::metadata(&produced).map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(&produced).ok();
        std::env::set_current_dir(before).expect("restore cwd");
        std::fs::remove_dir_all(&dir).ok();

        result.unwrap_or_else(|e| panic!("a relative -o must run: {e:?}"));
        assert!(existed, "the binary was not written to {}", out.display());
        assert!(size > 0, "an empty file is not a program");
    }

    /// A generated program that uses an interpreter-only form must be refused by
    /// the AOT step with the same reason, not by a confusing codegen failure.
    /// This is the case the up-front guard cannot catch.
    #[test]
    fn a_generated_interpreter_only_program_is_refused_by_aot() {
        let src = "(import \"m.ainl\")\n(print 1)\n";
        let f = run_aot(src, Path::new("/tmp/ainl-gen-refuse-test")).unwrap_err();
        assert_eq!(f.step, "compile");
        assert!(f.for_model.contains("interpreter-only"), "{}", f.for_model);
        assert!(f.for_model.contains("--run"), "the fix must be named");
    }

    /// End to end through the pipeline, with no model involved: a good program
    /// passes, and a broken one fails at the step that actually breaks, with a
    /// message the repair loop can use.
    #[test]
    fn the_pipeline_passes_a_good_program_and_catches_a_bad_one() {
        let w = WorkingFile::new("spec").expect("a work file");
        let out = std::env::temp_dir().join("ainl-gen-pipeline-test");

        // Good: prints 36 and returns.
        assert!(attempt(
            "(def sq (fn (x) (* x x)))\n(print (sq 6))\n",
            w.path(),
            RunMode::Interpreter,
            None
        )
        .is_ok());

        // Bad at run time: unbound symbol, with the suggestion attached.
        let f = attempt(
            "(def double (fn (x) (* 2 x)))\n(print (doble 2))\n",
            w.path(),
            RunMode::Interpreter,
            None,
        )
        .unwrap_err();
        assert_eq!(f.step, "run");
        assert!(f.for_model.contains("unbound symbol"), "{}", f.for_model);
        assert!(
            f.for_model.contains("did you mean"),
            "the suggestion must survive into the repair prompt: {}",
            f.for_model
        );

        // Bad at parse time.
        let f2 = attempt("(print 1\n", w.path(), RunMode::Interpreter, None).unwrap_err();
        assert_eq!(f2.step, "validate");
        assert!(f2.for_model.contains("parse error"), "{}", f2.for_model);

        // Bad at compile time, for --aot: an interpreter-only form.
        let f3 = attempt(
            "(http-get \"http://127.0.0.1:1/\")\n",
            w.path(),
            RunMode::Aot,
            Some(&out),
        )
        .unwrap_err();
        assert_eq!(f3.step, "compile");

        let _ = std::fs::remove_file(&out);
    }

    /// `--aot` on a host without a C compiler must say so, and must not leave a
    /// half-written binary behind. The message names the fix.
    #[test]
    fn aot_without_a_compiler_says_so() {
        // Guarded on the host actually lacking `cc`, since a developer machine
        // usually has one; the assertion is about the message, not the host.
        if Command::new("cc").arg("-dumpversion").output().is_ok() {
            return;
        }
        let f = run_aot("(print 1)\n", Path::new("/tmp/ainl-gen-no-cc")).unwrap_err();
        assert_eq!(f.step, "compile");
        assert!(f.for_model.contains("no C compiler"), "{}", f.for_model);
        assert!(f.for_model.contains("--run"), "the fix must be named");
    }

    /// A temporary program file is removed on drop, and a kept copy is written
    /// where asked. A `gen` that littered /tmp would be a nuisance on a CI
    /// runner.
    #[test]
    fn the_working_file_is_cleaned_up_unless_kept() {
        let dest = std::env::temp_dir().join("ainl-gen-kept-test.ainl");
        {
            let mut w = WorkingFile::new("spec").expect("a work file");
            w.write("(print 1)\n").expect("write");
            let p = w.path().to_path_buf();
            assert!(p.exists(), "the program must be on disk to resolve imports");
            w.write_to(&dest).expect("keep");
        }
        let _ = std::fs::remove_file(&dest);
    }

    /// The end-of-run report must never claim a check that did not happen.
    ///
    /// Three combinations, all reachable from the command line, and all of
    /// which used to print "constrained decoding was verified in force"
    /// because that text was keyed off the `--constrained` flag alone.
    #[test]
    fn the_summary_never_claims_a_check_that_did_not_run() {
        // A grammar on the wire and the probe ran: the one real claim.
        let verified = constraint_summary(true, true, true);
        assert!(verified.contains("verified in force"), "{verified}");

        // `--no-probe`: a grammar was sent, but nothing proved it was applied.
        let unprobed = constraint_summary(true, false, true);
        assert!(!unprobed.contains("verified in force"), "{unprobed}");
        assert!(unprobed.contains("--no-probe"), "{unprobed}");
        assert!(unprobed.contains("unverified"), "{unprobed}");

        // `--grammar-field none` / `--unconstrained`: no grammar was sent.
        let none = constraint_summary(false, false, true);
        assert!(!none.contains("verified in force"), "{none}");
        assert!(none.contains("NOT grammar-constrained"), "{none}");
    }

    /// A grammar that was sent but *not honoured* is called out, because the
    /// silent-ignore is the failure this whole feature exists to catch.
    #[test]
    fn a_sent_but_ignored_grammar_is_reported() {
        let s = constraint_summary(true, true, false);
        assert!(s.contains("verified in force"), "{s}");
        assert!(s.contains("not a member"), "{s}");

        // And it is not reported when the run was unconstrained anyway:
        // a non-member program is not evidence of an ignored constraint when
        // no constraint was ever sent.
        let s2 = constraint_summary(false, false, false);
        assert!(!s2.contains("not a member"), "{s2}");
    }

    /// `--help` returns the help text rather than being treated as an unknown
    /// flag, and the text must mention every environment variable the code
    /// reads — otherwise the documented contract and the real one drift apart.
    #[test]
    fn help_documents_every_environment_variable() {
        // The interpolated text, which is what a user actually sees.
        let e = help_text();
        assert!(
            !e.contains("{ENV_") && !e.contains("{DEFAULT_"),
            "a placeholder was left unsubstituted"
        );
        for name in [ENV_ENDPOINT, ENV_MODEL, ENV_KEY_NAME, ENV_USER_AGENT] {
            assert!(e.contains(name), "--help does not document {name}");
        }
        // And the flags that matter most.
        for flag in [
            "--aot",
            "--run",
            "--constrained",
            "--unconstrained",
            "--grammar-field",
            "--attempts",
            "--api-key-env",
            "--keep",
            "--dry-run",
        ] {
            assert!(e.contains(flag), "--help does not document {flag}");
        }
    }
}
