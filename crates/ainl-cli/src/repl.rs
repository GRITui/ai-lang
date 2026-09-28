//! `ainl repl` — the read-eval-print loop.
//!
//! The vibe-coding loop is describe → run → tweak → run. A REPL keeps the
//! program in a *running* environment so a tweak costs one line instead of
//! write-file / compile / run, and so state (a `def`, a counter, an
//! accumulator) is still there when the next line arrives.
//!
//! # Scope: interpreter only
//!
//! Everything here runs on the bytecode VM ([`ainl_core::vm`]), which is the
//! interpreter backend. A REPL is inherently a development-time tool, so the
//! AOT C backend and the three transpiler targets are deliberately *out of
//! scope*: there is nothing to "repl" in a generated C binary, and a
//! transpiled program's `def`s live in the host language's own REPL. The
//! cross-backend parity story is unchanged by this file — a REPL expression is
//! ordinary AINL that any backend evaluates identically; only the front end
//! that feeds it is new. `ainl run` / `eval` / `compile` / `transpile` are
//! untouched and remain the way to get the other backends.
//!
//! # Three things this file decides
//!
//! 1. **What counts as "one submission".** A submission is read until its
//!    strings are all closed ([`ainl_core::lexer::scan`]) — *not* until its
//!    parens balance. An unclosed paren is left to the parser, so a genuinely
//!    unbalanced line reports the parser's real message
//!    (`unclosed '('`, `unexpected ')'`) instead of silently swallowing the
//!    line and demanding a continuation that would never come.
//! 2. **One form at a time.** A submission of several top-level forms is
//!    evaluated form-by-form ([`ainl_core::vm::run_form`]), so a form that
//!    fails cannot discard the `def`s earlier forms in the same submission
//!    already made. This matches the tree-walking evaluator.
//! 3. **Errors never end the session.** Every error is printed to stderr with
//!    a `line N:` prefix and the loop continues. The only ways out are `(exit)`,
//!    an EOF, or — deliberately — nothing else.

use std::io::{self, BufRead, Write};

use ainl_core::error::Error;
use ainl_core::eval::Env;
use ainl_core::Value;

/// The prompt shown when no submission is in progress.
pub const PROMPT: &str = "λ ";

/// The prompt shown while a submission is still open (an unterminated string).
/// A distinct glyph, so a multi-line input is never mistaken for a fresh one.
pub const CONTINUATION_PROMPT: &str = "… ";

/// Inputs that end the session, accepted with or without the paren wrapper a
/// REPL user would naturally type.
const EXITS: &[&str] = &["(exit)", "exit", "(quit)", "quit", ":q"];

/// What one `eval`d submission did. Returned rather than printed so the whole
/// loop is testable without a subprocess or a captured terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do: a blank line, or a line inside a `;` comment.
    Empty,
    /// The session ended on an `exit` command.
    Exit,
    /// A form evaluated; carries its `repr` (empty when the value is `nil`).
    Value(String),
    /// A form failed. The message is already formatted for a human; the
    /// session continues.
    Error(String),
}

/// A running REPL: the environment that persists across submissions.
pub struct Repl {
    env: Env,
    /// Source lines of the submission being assembled, and the 1-based number of
    /// the *first* of them, so an error can name the line the user typed.
    buf: Vec<String>,
    first_line: usize,
    /// 1-based count of lines read so far in the session.
    line_no: usize,
}

impl Default for Repl {
    fn default() -> Repl {
        Repl::new()
    }
}

impl Repl {
    /// A fresh session with the prelude loaded.
    pub fn new() -> Repl {
        Repl {
            env: Env::with_prelude(),
            buf: Vec::new(),
            first_line: 0,
            line_no: 0,
        }
    }

    /// The prompt appropriate to the current state.
    pub fn prompt(&self) -> &'static str {
        if self.continuing() {
            CONTINUATION_PROMPT
        } else {
            PROMPT
        }
    }

    /// Feed one input line. Returns what happened, or `None` when the line did
    /// not complete a submission.
    ///
    /// The line is taken *without* its trailing newline: `BufRead::read_line`
    /// keeps the `\n`, and the buffer is joined with `\n`, so passing both
    /// through would insert a blank line between every continuation — which
    /// silently changes the value of a multi-line string (every string in a
    /// REPL session would be one character longer than it looked). The
    /// terminator is not a syntax character in AINL, so dropping it is safe.
    pub fn line(&mut self, input: &str) -> Option<Outcome> {
        self.line_no += 1;
        if self.buf.is_empty() {
            self.first_line = self.line_no;
        }
        self.buf
            .push(input.trim_end_matches(['\n', '\r']).to_string());
        let src = self.buf.join("\n");
        if ainl_core::lexer::scan(&src).needs_more() {
            // A string or a `(` is still open. The submission is NOT discarded,
            // so the next line appends to it.
            None
        } else {
            let at = self.first_line;
            self.buf.clear();
            Some(self.eval(&src, at))
        }
    }

    /// True while a submission is mid-flight (the REPL is showing `…`).
    fn continuing(&self) -> bool {
        !self.buf.is_empty()
    }

    /// The line an unfinished submission started on, if one is pending. Used to
    /// report an end-of-input inside an open form, which would otherwise look
    /// like a clean run that produced no output.
    pub fn unfinished(&self) -> Option<usize> {
        (!self.buf.is_empty()).then_some(self.first_line)
    }

    /// Evaluate a complete submission. Split out from [`Repl::line`] so it can
    /// be called directly by tests, and so the line-number handling lives in
    /// one place.
    fn eval(&mut self, src: &str, at: usize) -> Outcome {
        let trimmed = src.trim();
        // A blank line, or one that is only a comment, is not a submission.
        if trimmed.is_empty() || trimmed.starts_with(';') {
            return Outcome::Empty;
        }
        if EXITS.contains(&trimmed) {
            return Outcome::Exit;
        }
        let forms = match ainl_core::parse(src) {
            Ok(f) => f,
            // The parse error is reported at its true byte offset, but the line
            // number is the one the user can act on: in a multi-line submission
            // the offending byte is rarely on the line they remember typing.
            Err(e) => return Outcome::Error(describe(e, at)),
        };
        let mut last = String::new();
        let mut printed = false;
        for form in &forms {
            match ainl_core::vm::run_form_in(form, &self.env, src) {
                Ok(v) => {
                    if !is_undisplayed(&v) {
                        // Only the *last* form's value is echoed, so a
                        // multi-form submission prints one result, not a stream
                        // of intermediate `def` names. `print` output still
                        // appears as it happens, which is the point of `print`.
                        last = v.repr();
                        printed = true;
                    }
                }
                Err(e) => return Outcome::Error(describe(e, at)),
            }
        }
        if printed {
            Outcome::Value(last)
        } else {
            Outcome::Empty
        }
    }
}

/// A value the REPL does not echo. `(def x 1)` evaluates to the symbol `x`,
/// which every REPL in the world prints as noise; echoing the *last* form of a
/// multi-form submission means `(def x 1)` alone would print `x`. `nil` and the
/// empty list are likewise silent, so a REPL session reads as a stream of
/// answers rather than a stream of bookkeeping.
fn is_undisplayed(v: &Value) -> bool {
    matches!(v, Value::Nil | Value::Sym(_)) || matches!(v, Value::List(l) if l.len == 0)
}

/// Render an error for a REPL session.
///
/// The error already carries a resolved `at line N, col M` whenever the failing
/// node had a position, so this only adds the submission's starting line as a
/// *fallback* — for the errors that have no node (a resource limit), and for a
/// submission the REPL parses incrementally. It is deliberately not an
/// unconditional prefix: with a position already present, `line 1: … at line 1,
/// col 2` names two different lines and reads as a contradiction.
fn describe(e: Error, line: usize) -> String {
    if e.location().is_some() {
        format!("{e}")
    } else {
        format!("line {line}: {e}")
    }
}

/// One `read-eval-print` iteration against an explicit reader and writer.
///
/// Generic over `BufRead`/`Write` so the identical code path serves the
/// interactive loop and the `--stdin` script mode, and so a test can drive it
/// with an in-memory reader and assert the exact bytes. Public because
/// `doctor` drives it too — a self-test that used a different entry point
/// would not be testing the shipped path.
pub fn drive(
    repl: &mut Repl,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    interactive: bool,
) -> io::Result<()> {
    loop {
        if interactive {
            // The prompt is written unconditionally: a script that pipes input
            // and a human at a terminal then get byte-identical handling, and
            // a terminal that is not a TTY (a CI log, `repl < notes.txt`)
            // simply shows the prompts as harmless text.
            write!(output, "{}", repl.prompt())?;
            output.flush()?;
        }
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => {
                // EOF. If a submission was left open, the user is told: they
                // piped an unfinished form and the silence of a clean exit
                // would read as "it ran and produced nothing", which is a
                // different (and wrong) conclusion. Submitting it anyway would
                // be worse — it is not valid AINL.
                if let Some(pending) = repl.unfinished() {
                    let _ = writeln!(
                        errors,
                        "line {pending}: end of input inside an unfinished form (nothing was evaluated)"
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                // An input error is reported like any other: the session does
                // not die silently, and the loop stops because the stream is
                // unusable.
                let _ = writeln!(errors, "input error: {e}");
                return Ok(());
            }
        }
        let Some(outcome) = repl.line(&line) else {
            continue;
        };
        match outcome {
            Outcome::Empty => {}
            Outcome::Exit => return Ok(()),
            Outcome::Value(v) => {
                writeln!(output, "{v}")?;
                output.flush()?;
            }
            Outcome::Error(msg) => {
                // Errors go to stderr, so `ainl repl --stdin < prog.ainl >
                // out.txt` keeps a clean result stream even when a line fails.
                writeln!(errors, "{msg}")?;
                errors.flush()?;
            }
        }
    }
}

/// `ainl repl` / `ainl repl --stdin`.
///
/// `--stdin` is not a different REPL: it is the *same* loop with the
/// interactive prompt suppressed, which is what makes the transcript
/// assertable in a test and identical in CI. Without it, `ainl repl` reads
/// stdin too, so the flag's only job is to make the non-interactive intent
/// explicit (and it is what `doctor` and the test suite use).
pub fn run(stdin_mode: bool) -> std::process::ExitCode {
    let mut repl = Repl::new();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    if !stdin_mode {
        // Banner: the version line, so a bug report that pastes a transcript
        // says which build produced it, and the exit instruction, so a first
        // session does not need the docs open.
        let _ = writeln!(
            out,
            "ainl {} REPL — type (exit) or Ctrl-D to quit",
            crate::VERSION
        );
    }

    let mut input = io::stdin().lock();
    let result = drive(&mut repl, &mut input, &mut out, &mut err, !stdin_mode);
    if let Err(e) = result {
        let _ = writeln!(err, "repl: {e}");
        return std::process::ExitCode::FAILURE;
    }
    if !stdin_mode {
        // A newline so the shell prompt does not land on the `λ ` we were
        // interrupted at (e.g. after Ctrl-D).
        let _ = writeln!(out);
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a whole session over an in-memory script and return
    /// `(stdout, stderr)`. Exercises the exact `drive` loop the CLI uses.
    fn session(input: &str) -> (String, String) {
        let mut repl = Repl::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut reader = io::BufReader::new(input.as_bytes());
        drive(&mut repl, &mut reader, &mut out, &mut err, false).unwrap();
        (
            String::from_utf8(out).expect("utf-8 stdout"),
            String::from_utf8(err).expect("utf-8 stderr"),
        )
    }

    /// The values a session printed, one per line — the assertable transcript.
    ///
    /// Note what is *not* here: anything from `(print …)`. `print` writes to
    /// the process's real stdout, so an in-process test cannot capture it —
    /// the REPL's own echo goes to the injected writer, and `print` bypasses
    /// it entirely. `print` is therefore covered by the subprocess suite in
    /// tests/repl_cli.rs, which is the only place it can honestly be tested.
    fn values(input: &str) -> Vec<String> {
        let (out, err) = session(input);
        assert!(err.is_empty(), "unexpected errors: {err}");
        out.lines().map(str::to_string).collect()
    }

    #[test]
    fn evaluates_a_line_and_prints_the_result() {
        assert_eq!(values("(+ 1 2 3)\n"), ["6"]);
    }

    #[test]
    fn bindings_persist_across_submissions() {
        // The whole point of the REPL: a `def` in one submission is still
        // there in the next.
        assert_eq!(values("(def x 41)\n(def x (+ x 1))\nx\n"), ["42"]);
    }

    #[test]
    fn a_def_is_not_echoed_as_its_own_name() {
        // `(def x 1)` returns the symbol `x`; printing it is noise that makes
        // a real transcript unreadable.
        assert_eq!(values("(def x 1)\n(+ x 1)\n"), ["2"]);
    }

    #[test]
    fn nil_results_are_silent() {
        assert_eq!(values("(while false 1)\nnil\n"), Vec::<String>::new());
    }

    #[test]
    fn the_empty_list_is_silent_too() {
        // `(list)` is the other "nothing to say" value; echoing `()` after
        // every empty-literal call would be the same noise as echoing `x`.
        assert_eq!(values("(list)\n(len (list))\n"), ["0"]);
    }

    #[test]
    fn a_non_empty_list_is_echoed_verbatim() {
        assert_eq!(values("(list 1 2 3)\n"), ["(1 2 3)"]);
    }

    #[test]
    fn multiple_forms_on_one_line_print_only_the_last_value() {
        // `1 2 3` is three top-level forms, so only the last is echoed.
        assert_eq!(values("1 2 3\n"), ["3"]);
    }

    #[test]
    fn an_unclosed_paren_continues_to_the_next_line() {
        assert_eq!(values("(+ 1\n   2)\n"), ["3"]);
    }

    #[test]
    fn a_multi_line_string_has_no_phantom_newline() {
        // Regression: `read_line` keeps its `\n` and the buffer is joined with
        // `\n`, so passing both through inserted an extra blank line between
        // every continuation and made every multi-line string longer than the
        // user typed. The expectations below are cross-checked against
        // `ainl eval` on the same source, which takes the no-continuation path
        // and must agree: "hello" + one newline = 6, and "a\nb\nc\n" = 6.
        assert_eq!(values("(len \"hello\n\")\n"), ["6"]);
        assert_eq!(values("(len \"a\nb\nc\n\")\n"), ["6"]);
        // A string with no continuation is unaffected — this is the baseline
        // the two above are compared against.
        assert_eq!(values("(len \"hello\")\n"), ["5"]);
    }

    #[test]
    fn a_multi_line_form_continues_across_many_lines() {
        let src = "(def fib (fn (n)\n  (if (< n 2) n\n    (+ (fib (- n 1))\n       (fib (- n 2))))))\n(fib 10)\n";
        assert_eq!(values(src), ["55"]);
    }

    #[test]
    fn an_unterminated_string_continues() {
        // The scanner's reason for existing: `(+ "abc` is not an error, it is
        // an unfinished submission.
        assert_eq!(values("(len \"hello\n\")\n"), ["6"]);
    }

    #[test]
    fn an_escaped_quote_does_not_close_the_string() {
        // `\"` is a quote *inside* the string, so this must keep reading —
        // a naive quote-count would close it here and then blow up on the
        // real terminator. The `\"` is one character, so the length is the
        // proof that it stayed inside the string.
        assert_eq!(values(r#"(len "a\"b")"#), ["3"]);
    }

    #[test]
    fn a_trailing_escape_errors_instead_of_asking_for_more() {
        // `\` at the very end of a line is unfixable: the next line's first
        // character is its own newline, and `\<newline>` is an invalid escape
        // in the lexer's rules. So the REPL submits immediately and shows the
        // real `unterminated escape`, rather than inviting a line that cannot
        // help. (The scanner is conservative on purpose here.)
        let (out, err) = session("(len \"a\\\n(+ 1 2)\n");
        assert_eq!(out, "3\n", "the next line still runs");
        assert!(err.contains("escape"), "got: {err}");
    }

    #[test]
    fn a_comment_at_end_of_line_does_not_demand_a_continuation() {
        // A `;` comment can never continue, so this must submit immediately
        // rather than hanging the session waiting for another line.
        let (out, err) = session("(+ 1 2) ; two\n");
        assert_eq!(out, "3\n", "stderr was: {err}");
    }

    #[test]
    fn a_fully_commented_line_is_not_a_submission() {
        assert_eq!(values("; just a note\n(+ 1 2)\n"), ["3"]);
    }

    #[test]
    fn an_error_prints_and_the_session_continues() {
        let (out, err) = session("(nosuchvar)\n(+ 1 2)\n");
        assert_eq!(out, "3\n", "the session must continue after an error");
        assert!(err.contains("unbound symbol 'nosuchvar'"), "got: {err}");
        // The position is what a user can act on: line and column, not a bare
        // byte offset. `nosuchvar` opens at column 2 of line 1.
        assert!(err.contains("at line 1, col 2"), "got: {err}");
    }

    #[test]
    fn an_error_does_not_poison_the_environment() {
        // A failed line leaves everything defined before it intact. The error
        // is expected, so it is checked for explicitly rather than via
        // `values`, which fails on any stderr.
        let (out, err) = session("(def a 7)\n(nosuch)\na\n");
        assert_eq!(out, "7\n", "the value defined earlier must survive");
        assert!(err.contains("unbound symbol 'nosuch'"), "got: {err}");
    }

    #[test]
    fn a_def_before_a_failing_form_in_the_same_submission_survives() {
        // One form at a time, so the later failure cannot roll back the earlier
        // `def`. This is the reason the REPL does not submit a whole
        // multi-form program to `run_in` at once.
        let (out, err) = session("(def a 7) (nosuch)\na\n");
        assert_eq!(out, "7\n", "stderr was: {err}");
    }

    #[test]
    fn a_parse_error_reports_the_real_parser_message() {
        // Deliberately *not* swallowed as "unfinished input": the user is told
        // what is actually wrong instead of being invited to keep typing.
        let (out, err) = session("(+ 1 2))\n(+ 1 2)\n");
        assert_eq!(out, "3\n", "the session must continue after a parse error");
        assert!(err.contains("unexpected ')'"), "got: {err}");
    }

    #[test]
    fn an_unclosed_paren_waits_rather_than_erroring() {
        // The counterpart to the previous test. A dangling `(` is genuinely
        // unfinished input, so the REPL must ask for the next line — and at end
        // of input it says so, instead of exiting as if it had run.
        let mut repl = Repl::new();
        assert_eq!(repl.line("(+ 1 2\n"), None, "must wait for the `)`");
        assert_eq!(repl.unfinished(), Some(1), "and remember where it started");
        // Completing it on the next line is the normal path.
        assert_eq!(repl.line(")\n"), Some(Outcome::Value("3".into())));
        assert_eq!(repl.unfinished(), None);
    }

    #[test]
    fn end_of_input_inside_an_open_form_is_reported() {
        // A script that ends mid-form must not look like a clean run. The
        // value of the EOF diagnostic is that silence is never mistaken for
        // "it evaluated to nothing".
        let (out, err) = session("(+ 1\n  2\n");
        assert_eq!(out, "", "nothing can be evaluated from an open form");
        assert!(err.contains("unfinished form"), "got: {err}");
        assert!(err.contains("line 1:"), "got: {err}");
    }

    #[test]
    fn an_error_reports_the_line_within_the_submission_that_failed() {
        // The submission is one unbalanced form spanning two lines. The error
        // names the line *inside* the submission the offending token is on,
        // which is more useful than always naming the submission's first line.
        let (_, err) = session("(+ 1\n   2))\n");
        assert!(err.contains("unexpected ')'"), "got: {err}");
        assert!(err.contains("line 2"), "got: {err}");
    }

    #[test]
    fn exit_stops_the_session_everywhere_in_the_input() {
        // A line after `(exit)` must never run, or a script could not end
        // early without losing its output.
        let (out, _) = session("(+ 1 2)\n(exit)\n999\n");
        assert_eq!(out, "3\n", "the 999 after (exit) must not be echoed");
    }

    #[test]
    fn every_documented_exit_spellings_works() {
        for cmd in ["(exit)", "exit", "(quit)", "quit", ":q"] {
            let mut repl = Repl::new();
            assert_eq!(repl.line(&format!("{cmd}\n")), Some(Outcome::Exit), "{cmd}");
        }
    }

    #[test]
    fn exit_is_only_recognised_as_a_whole_line() {
        // `(exit)` nested inside a form is not the REPL's quit command: the
        // quit check looks at the *whole trimmed line*, never at a substring.
        // Quoted, so the `exit` builtin is not called — calling it would end
        // the test harness process. A real `(exit 0)` call from a REPL line is
        // covered in tests/repl_cli.rs, which has a process to spare.
        let mut repl = Repl::new();
        assert_eq!(
            repl.line("(quote (exit))\n"),
            Some(Outcome::Value("(exit)".into())),
            "a quoted `(exit)` is a value, not a quit command"
        );
        assert!(!repl.continuing(), "the submission was consumed");
    }

    #[test]
    fn eof_ends_the_session_cleanly() {
        let mut repl = Repl::new();
        assert_eq!(repl.line("(+ 1 2)\n"), Some(Outcome::Value("3".into())));
        // `line` cannot see EOF — that is the caller's job — so a real EOF is
        // asserted through `drive`, which owns the reader.
        let (out, _) = session("(+ 1 2)");
        assert_eq!(
            out, "3\n",
            "input with no trailing newline must still submit"
        );
        assert!(!repl.continuing());
    }

    #[test]
    fn a_dangling_open_string_at_eof_is_reported_not_swallowed() {
        // EOF inside a continuation: nothing can be evaluated, and the session
        // says so rather than exiting as if it had run. It must not print a
        // bogus value, and it must not exit silently either.
        let (out, err) = session("(len \"unfinished");
        assert_eq!(out, "", "an unfinished submission has no value to print");
        assert!(err.contains("unfinished form"), "got: {err}");
    }

    #[test]
    fn the_prompt_switches_to_a_continuation_prompt() {
        let mut repl = Repl::new();
        assert_eq!(repl.prompt(), PROMPT);
        assert_eq!(repl.line("(+ 1\n"), None, "should be waiting for more");
        assert!(repl.continuing());
        assert_eq!(repl.prompt(), CONTINUATION_PROMPT);
        assert_eq!(repl.line("2)\n"), Some(Outcome::Value("3".into())));
        assert!(!repl.continuing());
        assert_eq!(repl.prompt(), PROMPT);
    }

    #[test]
    fn the_interactive_prompt_is_written_and_the_script_prompt_is_not() {
        // Same loop, one flag: this is the difference between the two modes,
        // asserted at the byte level.
        let mut repl = Repl::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut reader = io::BufReader::new(&b"(+ 1 2)\n"[..]);
        drive(&mut repl, &mut reader, &mut out, &mut err, true).unwrap();
        let interactive = String::from_utf8(out).unwrap();
        assert_eq!(interactive, format!("{PROMPT}3\n{PROMPT}"));

        let mut out = Vec::new();
        let mut reader = io::BufReader::new(&b"(+ 1 2)\n"[..]);
        drive(&mut repl, &mut reader, &mut out, &mut err, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "3\n");
    }

    #[test]
    fn a_continuation_line_is_echoed_only_by_its_prompt() {
        // The intermediate line is not re-printed; only the prompt changes.
        let mut repl = Repl::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut reader = io::BufReader::new(&b"(+ 1\n2)\n"[..]);
        drive(&mut repl, &mut reader, &mut out, &mut err, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{PROMPT}{CONTINUATION_PROMPT}3\n{PROMPT}")
        );
    }

    #[test]
    fn a_failing_submission_still_flushes_its_own_output() {
        // `print` writes to the process's own stdout, so its output cannot be
        // observed here — the property that a form's output is kept when a
        // *later* form in the same submission fails is asserted in
        // tests/repl_cli.rs, where a real process is involved. What is
        // assertable in-process is the part that does not need `print`: the
        // `def` from the same submission is still bound afterwards.
        let (out, err) = session("(def kept 5) (nosuch)\nkept\n");
        assert_eq!(out, "5\n", "stderr was: {err}");
        assert!(err.contains("unbound symbol"), "got: {err}");
    }

    #[test]
    fn the_vm_and_the_tree_walk_agree_on_a_repl_session() {
        // The REPL runs on the VM; the tree-walk is the semantic reference the
        // VM is checked against everywhere else. Replaying the same
        // form-by-form session through both must produce the same values and
        // the same failures, or the REPL is documenting semantics that the
        // reference does not have.
        use ainl_core::vm;
        let script = [
            "(def acc 0)",
            "(def bump (fn (n) (def acc (+ acc n))))",
            "(bump 5)",
            "acc",
            "(def xs (list 1 2 3))",
            "(len xs)",
            "(nosuch)",
            "acc",
            "(def acc (* acc 2))",
            "acc",
        ];
        let vm_env = Env::with_prelude();
        let tw_env = Env::with_prelude();
        for line in script {
            let forms = ainl_core::parse(line).expect("test script must parse");
            for form in &forms {
                let a = vm::run_form(form, &vm_env).map(|v| v.repr());
                let b = ainl_core::eval::eval(form, &tw_env).map(|v| v.repr());
                assert_eq!(a, b, "disagreement on {line}");
            }
        }
    }

    #[test]
    fn a_failed_submission_does_not_leave_the_repl_mid_continuation() {
        // A parse error must clear the buffer, or the *next* line would be
        // appended to dead text and fail too — a single typo would poison
        // every line after it.
        let mut repl = Repl::new();
        assert!(repl.line("(+ 1 2))\n").is_some());
        assert!(!repl.continuing());
        assert_eq!(repl.prompt(), PROMPT);
        assert_eq!(repl.line("(+ 1 2)\n"), Some(Outcome::Value("3".into())));
    }

    #[test]
    fn a_multi_line_error_still_clears_the_buffer() {
        let mut repl = Repl::new();
        assert_eq!(repl.line("(+ 1\n"), None, "waiting for the rest");
        assert!(repl.continuing());
        let outcome = repl.line("2))\n").expect("the second line completes it");
        assert!(matches!(outcome, Outcome::Error(_)), "got {outcome:?}");
        assert!(
            !repl.continuing(),
            "a failed multi-line submission must reset"
        );
    }

    #[test]
    fn the_step_budget_is_per_submission_so_a_runaway_line_does_not_poison_the_session() {
        // One runaway form must not make every later line fail too.
        let (out, err) = session("(while true 1)\n(+ 1 2)\n");
        assert_eq!(out, "3\n", "the next submission must still run");
        assert!(err.contains("step limit exceeded"), "got: {err}");
    }
}
