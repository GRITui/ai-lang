//! End-to-end tests for `ainl repl`, driving the real binary through `--stdin`.
//!
//! The REPL's in-process unit tests (in `src/repl.rs`) cover the loop with an
//! injected reader and writer. This file covers what only a real process can:
//!
//! * `print` writing to the process's actual stdout, interleaved correctly with
//!   the REPL's own value echo;
//! * the two streams really being separate (so `… > out.txt` gives a clean
//!   result file while errors stay visible);
//! * the interactive prompt and banner appearing on a pipe as well as a
//!   terminal, and `--stdin` suppressing both;
//! * the process exit code.
//!
//! Each case is a transcript: lines in, exact stdout and stderr out. That is
//! the property `--stdin` exists for — a REPL session that can be scripted and
//! diffed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn ainl_bin() -> PathBuf {
    // Debug before release: `cargo test` rebuilds only the debug binary, so a
    // release binary found first here can be stale. See the same note in
    // ainl-core/tests/stdlib_cli.rs.
    let debug = repo_root().join("target/debug/ainl");
    if debug.exists() {
        debug
    } else {
        repo_root().join("target/release/ainl")
    }
}

/// The result of one scripted REPL session.
struct Session {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Session {
    fn stdout_lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }
}

/// Run `ainl repl --stdin` over `input` and capture both streams.
///
/// Written to the child's stdin explicitly (rather than piped from a shell) so
/// the test does not depend on the host shell, and so a zero-length input is
/// still a well-formed request.
fn repl_stdin(input: &str) -> Session {
    repl_stdin_args(&["repl", "--stdin"], input)
}

/// `ainl <args…>` with `input` on stdin.
fn repl_stdin_args(args: &[&str], input: &str) -> Session {
    let bin = ainl_bin();
    if !bin.exists() {
        panic!(
            "ainl binary not built ({}); run `cargo build` first",
            bin.display()
        );
    }
    let mut child = Command::new(&bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ainl");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(input.as_bytes())
            .expect("write to ainl stdin");
    }
    let out = child.wait_with_output().expect("wait for ainl");
    Session {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A session that must succeed with no stderr, asserted in one call so every
/// transcript test states its own preconditions.
fn ok(input: &str) -> Session {
    let s = repl_stdin(input);
    assert_eq!(s.code, 0, "expected success, stderr was: {}", s.stderr);
    assert!(s.stderr.is_empty(), "expected no stderr, got: {}", s.stderr);
    s
}

// ---------------------------------------------------------------------------
// The headline capability: a scripted session whose transcript is assertable.
// ---------------------------------------------------------------------------

#[test]
fn a_stdin_session_evaluates_lines_in_order_and_keeps_state() {
    // The whole point of `--stdin`: the describe → run → tweak → run loop,
    // scripted. A `def` on line 2 is still live on line 6, which is what a
    // REPL is for and what a fresh `ainl run` per line could not do.
    let s = ok(r#"(def x 10)
(def double (fn (n) (* n 2)))
(double x)
(def x (+ x 5))
(double x)
"#);
    assert_eq!(s.stdout_lines(), ["20", "30"]);
}

#[test]
fn a_multiline_expression_is_accepted_through_stdin() {
    // An unclosed `(` on one line continues: the value of the whole form is
    // printed once, after the line that closes it.
    let s = ok("(+ 1\n   2\n   3)\n");
    assert_eq!(s.stdout_lines(), ["6"]);
}

#[test]
fn a_real_multiline_function_works_through_stdin() {
    // Not a toy: a recursive `def` written the way a human would type it.
    let s = ok(r#"(def fib (fn (n)
  (if (< n 2) n
    (+ (fib (- n 1))
       (fib (- n 2))))))
(fib 15)
(fib 20)
"#);
    assert_eq!(s.stdout_lines(), ["610", "6765"]);
}

#[test]
fn a_multiline_string_keeps_its_exact_contents() {
    // The phantom-newline regression, end to end: `read_line` hands over its
    // `\n` and the buffer is joined with `\n`, so passing both through would
    // add a blank line here. `ainl eval` on the same text is the oracle, and
    // both must print the same length.
    let src = "(len \"line one\nline two\nline three\n\")\n";
    let s = ok(src);
    let oracle = Command::new(ainl_bin())
        .args(["eval", "(len \"line one\nline two\nline three\n\")"])
        .output()
        .expect("run ainl eval");
    let want = String::from_utf8_lossy(&oracle.stdout).trim().to_string();
    assert_eq!(s.stdout_lines(), [want.as_str()]);
    // 8 + 1 + 8 + 1 + 10 + 1 = 29: three lines and the three newlines the
    // user typed. A phantom blank line per continuation would make this 31.
    assert_eq!(s.stdout_lines(), ["29"]);
}

// ---------------------------------------------------------------------------
// Errors: printed, session continues.
// ---------------------------------------------------------------------------

#[test]
fn an_error_prints_to_stderr_and_the_session_continues() {
    let s = repl_stdin("(nosuchvar)\n(+ 1 2)\n");
    assert_eq!(s.code, 0, "a REPL that errors is not a failed run");
    assert_eq!(s.stdout_lines(), ["3"], "the next line still evaluated");
    assert!(
        s.stderr.contains("unbound symbol 'nosuchvar'"),
        "got: {}",
        s.stderr
    );
    // The line number makes a multi-line session actionable.
    assert!(s.stderr.contains("line 1:"), "got: {}", s.stderr);
}

#[test]
fn a_parse_error_reports_and_the_session_continues() {
    // An *extra* `)` is reported immediately rather than swallowed as
    // "unfinished input", so the user gets the real diagnostic.
    let s = repl_stdin("(+ 1 2))\n(+ 1 2)\n");
    assert_eq!(s.stdout_lines(), ["3"]);
    assert!(s.stderr.contains("unexpected ')'"), "got: {}", s.stderr);
}

#[test]
fn a_failed_line_leaves_earlier_bindings_intact() {
    let s = repl_stdin("(def a 7)\n(nosuch)\na\n");
    assert_eq!(s.stdout_lines(), ["7"], "the earlier def must survive");
    assert!(s.stderr.contains("unbound symbol"), "got: {}", s.stderr);
}

#[test]
fn a_runaway_line_does_not_poison_the_next_one() {
    // The step budget is per submission, so a hit limit is contained.
    let s = repl_stdin("(while true 1)\n(+ 1 2)\n");
    assert_eq!(s.stdout_lines(), ["3"]);
    assert!(
        s.stderr.contains("step limit exceeded"),
        "got: {}",
        s.stderr
    );
}

#[test]
fn several_errors_are_all_reported_and_all_keep_the_session_alive() {
    let s = repl_stdin("(nosuch1)\n(nosuch2)\n(nosuch3)\n(+ 1 1)\n");
    assert_eq!(s.stdout_lines(), ["2"]);
    for n in ["nosuch1", "nosuch2", "nosuch3"] {
        assert!(s.stderr.contains(n), "{n} missing from: {}", s.stderr);
    }
    assert_eq!(
        s.stderr.lines().count(),
        3,
        "one error line per failure: {}",
        s.stderr
    );
}

// ---------------------------------------------------------------------------
// print and the two streams.
// ---------------------------------------------------------------------------

#[test]
fn print_output_interleaves_with_the_value_echo() {
    // `print` goes straight to the process stdout, so its output lands before
    // the REPL echoes the form's value. This is the assertion that cannot be
    // made in-process, which is why this file exists.
    let s = ok("(print \"one\")\n(+ 1 2)\n(print \"two\") 9\n");
    assert_eq!(s.stdout_lines(), ["one", "3", "two", "9"]);
}

#[test]
fn a_def_is_not_echoed_but_print_still_is() {
    // `(def …)` returns the symbol; echoing it would make every transcript
    // unreadable. `print` is explicit, so it is never suppressed.
    let s = ok("(print \"setting up\")\n(def x 1)\nx\n");
    assert_eq!(s.stdout_lines(), ["setting up", "1"]);
}

#[test]
fn results_go_to_stdout_and_errors_to_stderr_so_a_script_can_redirect() {
    // The practical reason errors are on stderr: `ainl repl --stdin < s.ainl
    // > out.txt` must produce a clean result file with a failing line in the
    // middle, not an interleaved mess. Asserted by separating the streams.
    let s = repl_stdin("(+ 1 2)\n(nosuch)\n(+ 3 4)\n");
    assert_eq!(s.stdout_lines(), ["3", "7"], "results only, in order");
    assert_eq!(
        s.stderr.lines().count(),
        1,
        "the error is the only thing on stderr: {}",
        s.stderr
    );
}

#[test]
fn a_print_before_a_failing_form_is_not_lost() {
    // Output is a side effect that already happened; a later failure in the
    // same submission does not roll it back. (Same property `exit` is tested
    // for in ainl-core/tests/stdlib_cli.rs.)
    let s = repl_stdin("(print \"before\")\n(nosuch)\n");
    assert_eq!(s.stdout_lines(), ["before"], "printed output must survive");
    assert!(s.stderr.contains("unbound symbol"), "got: {}", s.stderr);
}

// ---------------------------------------------------------------------------
// The two modes, and the process contract.
// ---------------------------------------------------------------------------

#[test]
fn stdin_mode_prints_no_prompt_and_no_banner() {
    // This is the difference between the modes, at the byte level: nothing on
    // stdout that is not a result.
    let s = ok("(+ 1 2)\n");
    assert_eq!(s.stdout, "3\n", "no prompt, no banner, no stray newline");
}

#[test]
fn interactive_mode_prints_the_prompt_before_every_line() {
    // Without `--stdin` the same loop shows a banner and `λ `, so a transcript
    // is still readable. Piped input is not a TTY, which is exactly the case
    // this asserts — the prompt must not be suppressed just because it is not
    // a terminal.
    let s = repl_stdin_args(&["repl"], "(+ 1 2)\n");
    assert_eq!(s.code, 0, "stderr: {}", s.stderr);
    let (banner, rest) = s.stdout.split_once('\n').expect("a banner line");
    assert!(banner.contains("REPL"), "got: {banner}");
    // Result on the line that produced it, then a prompt left dangling at EOF
    // so the shell prompt does not land on it.
    assert_eq!(rest, "λ 3\nλ \n");
}

#[test]
fn the_interactive_prompt_switches_for_a_continuation() {
    // The `…` prompt is what tells a human their form is still open. Without
    // it a multi-line form looks like the session has hung.
    let s = repl_stdin_args(&["repl"], "(+ 1\n2)\n");
    assert_eq!(s.code, 0, "stderr: {}", s.stderr);
    let (_, rest) = s.stdout.split_once('\n').expect("a banner line");
    assert_eq!(rest, "λ … 3\nλ \n", "the second line is a continuation");
}

#[test]
fn the_interactive_banner_names_the_version() {
    // A pasted transcript should say which build produced it.
    let s = repl_stdin_args(&["repl"], "");
    let (banner, _) = s.stdout.split_once('\n').expect("a banner line");
    assert!(banner.contains("REPL"), "got: {banner}");
    assert!(banner.contains(env!("CARGO_PKG_VERSION")), "got: {banner}");
    assert!(banner.contains("(exit)"), "got: {banner}");
}

#[test]
fn exit_ends_the_session_and_skips_the_rest() {
    // Both spellings, and the guarantee that makes a script able to end early.
    for cmd in ["(exit)", "exit", "(quit)", "quit", ":q"] {
        let s = repl_stdin(&format!("(+ 1 2)\n{cmd}\n(nosuch)\n"));
        assert_eq!(s.code, 0, "{cmd}: {}", s.stderr);
        assert_eq!(s.stdout_lines(), ["3"], "{cmd}: later lines must not run");
        assert!(s.stderr.is_empty(), "{cmd}: {}", s.stderr);
    }
}

#[test]
fn the_exit_builtin_still_works_and_sets_the_exit_code() {
    // The REPL's own quit is a line the REPL recognises; `(exit N)` evaluated
    // as AINL is the stdlib builtin, and must still reach `process::exit`.
    let s = repl_stdin("(print \"bye\")\n(exit 7)\n(nosuch)\n");
    assert_eq!(s.code, 7, "the builtin's code must be the process code");
    assert_eq!(
        s.stdout_lines(),
        ["bye"],
        "output before exit must be flushed"
    );
    assert!(
        s.stderr.is_empty(),
        "nothing after exit may run: {}",
        s.stderr
    );
}

#[test]
fn eof_ends_the_session_successfully() {
    for input in ["", "(+ 1 2)", "(+ 1 2)\n"] {
        let s = ok(input);
        assert_eq!(
            s.stdout_lines(),
            if input.contains("+") {
                vec!["3"]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn eof_inside_an_open_form_is_reported_not_swallowed() {
    // A script that ends mid-form must not look like a clean run that produced
    // nothing. Silence here would be the single most misleading outcome.
    let s = repl_stdin("(+ 1\n  2\n");
    assert_eq!(s.code, 0, "an unfinished form is not a crash");
    assert_eq!(s.stdout, "", "nothing could be evaluated");
    assert!(
        s.stderr.contains("unfinished form"),
        "EOF must be reported: got: {}",
        s.stderr
    );
}

#[test]
fn blank_lines_and_comments_are_not_submissions() {
    // Neither produces a result line, and neither is an error.
    let s = ok("\n; a note\n   \n(+ 1 2)\n; trailing\n");
    assert_eq!(s.stdout_lines(), ["3"]);
}

#[test]
fn a_trailing_comment_does_not_wait_for_a_continuation() {
    // The hang-avoidance case: a comment can never continue, so the submission
    // must go through on the same line.
    let s = ok("(+ 1 2) ; the answer\n(+ 3 4) ; another\n");
    assert_eq!(s.stdout_lines(), ["3", "7"]);
}

#[test]
fn an_unknown_flag_is_rejected_rather_than_ignored() {
    // Same rule as `doctor`: a typo must not silently run a different session.
    let s = repl_stdin_args(&["repl", "--stdinn"], "(+ 1 2)\n");
    assert_ne!(s.code, 0, "an unknown flag must fail");
    assert!(
        s.stderr.contains("unknown flag '--stdinn'"),
        "got: {}",
        s.stderr
    );
}

#[test]
fn help_mentions_the_stdin_flag() {
    // The flag is only useful if it is discoverable.
    let out = Command::new(ainl_bin())
        .arg("help")
        .output()
        .expect("run ainl help");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ainl repl"), "got: {text}");
    assert!(text.contains("--stdin"), "got: {text}");
}

// ---------------------------------------------------------------------------
// A scripted session is byte-identical to the same program through `ainl run`.
// ---------------------------------------------------------------------------

#[test]
fn a_stdin_session_agrees_with_ainl_run_on_the_same_program() {
    // The REPL is a front end, not a second language: the same program must
    // produce the same output whether it arrives line-by-line through the
    // REPL or as a file through `ainl run`. This is the check that would fail
    // if the REPL's buffering or echo rules ever diverged from the run path.
    let program = "(def acc 0)\n(def bump (fn (n) (def acc (+ acc n))))\n(bump 3)\n(bump 4)\n(print acc)\n(print (json-serialize (list 1 \"a\" true)))\n";
    let dir = std::env::temp_dir().join(format!("ainl-repl-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("prog.ainl");
    std::fs::write(&path, program).expect("write program");

    let run = Command::new(ainl_bin())
        .arg("run")
        .arg(&path)
        .output()
        .expect("run ainl run");
    let via_run = String::from_utf8_lossy(&run.stdout).into_owned();

    // Through the REPL the transcript must be *identical* to `ainl run`'s
    // output. Every line here is a `(def …)` or a `(print …)`: `def` returns a
    // symbol and `print` returns `nil`, and the REPL echoes neither, so the
    // two paths agree exactly. That is the point of the fixture — it would
    // catch the REPL's echo rules drifting from the run path's, and it is
    // asserted as equality with `ainl run` rather than as a hardcoded list, so
    // the oracle is the run path itself.
    let via_repl = repl_stdin(program);
    let want: Vec<&str> = via_run.lines().collect();

    let _ = std::fs::remove_dir_all(&dir);
    assert!(run.status.success(), "ainl run failed: {}", via_run);
    assert_eq!(via_repl.stderr, "", "REPL stderr: {}", via_repl.stderr);
    assert_eq!(
        via_repl.stdout_lines(),
        want,
        "REPL and ainl run disagree on the same program"
    );
    // Guard against a vacuous pass: the transcript must not be empty, or this
    // would compare "" to "" and prove nothing.
    assert!(!want.is_empty(), "the fixture must produce output");
}
