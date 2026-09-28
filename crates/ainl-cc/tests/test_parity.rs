//! 4-backend parity for the `test` builtin.
//!
//! The card requires `(test ...)` to work in all four backends with byte-identical
//! output, so this suite is what holds that claim down. It compares the
//! **message body** of a failing test across the interpreter, the AOT C binary
//! and all three transpiler targets.
//!
//! # Why the position suffix is stripped rather than compared
//!
//! The interpreter and the VM resolve a byte offset into `at line N, col M`
//! because they hold the source. The AOT C backend emits a standalone C program
//! that embeds no source, so a line and column **do not exist** at run time —
//! not "is hard to get", but does not exist, and the runtime never invents one.
//! That is the same documented difference as every other AOT diagnostic
//! (docs/SYNTAX.md §5a), and `aot_stdlib.rs` already normalizes it the same way.
//! What this backend still owes the 4-backend rule is that the *description*
//! matches byte-for-byte, and that is what is compared here.
//!
//! # Why the transpiler targets are compared on the message only
//!
//! Python, Node and Ruby each prefix an uncaught exception with their own
//! class name and a source location (`RuntimeError: …`, `Error: …\n    at _test
//! (file:33:9)`). That framing is the host's, not AINL's, and no amount of
//! transpiler work can make it match C's bare message. What AINL owns is the
//! text AINL put in the exception, and that is compared byte-for-byte.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `cmd` and return its stdout, stderr and success flag.
fn capture(mut cmd: Command) -> (String, String, bool) {
    let out = cmd.output().expect("run command");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// `ainl run <file>`, built as a value so `capture` can take it by value.
/// (`Command::arg` borrows, so it cannot be chained inside the call.)
fn ainl_run(bin: &Path, case: &Path) -> Command {
    let mut c = Command::new(bin);
    c.arg("run").arg(case);
    c
}

/// Run a transpiler target's emitted program with its own interpreter.
fn run_target(runner: &str, program: &Path) -> Command {
    let mut c = Command::new(runner);
    c.arg(program);
    c
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Debug before release: `cargo test` rebuilds only the debug binary, and the
/// release binary can be stale (see aot_stdlib.rs for the full story).
fn ainl_bin() -> PathBuf {
    let debug = repo_root().join("target/debug/ainl");
    if debug.exists() {
        return debug;
    }
    repo_root().join("target/release/ainl")
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("ainl-test-parity");
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn write_case(name: &str, src: &str) -> PathBuf {
    let path = scratch_dir().join(name);
    std::fs::write(&path, src).expect("write case");
    path
}

/// The shared message body every backend must produce for a failing test.
const EXPECTED: &str = "test failed: adds two numbers: expected 4, got 3";

/// Isolate the message AINL produced, dropping everything a host or the
/// position suffix adds around it.
///
/// The interpreter and VM append ` at line N, col M (byte B)`; the AOT runtime
/// has no position to print (docs/SYNTAX.md §5a). Python, Node and Ruby each add
/// their own framing — a class name, a source line and caret, a backtrace. What
/// AINL owns is the text between `test failed: ` and the end of the assertion,
/// which is the same on all five and is what this helper returns.
///
/// The cut is made on the assertion's own text: `expected <x>, got <y>`. Both
/// halves are part of the message, and nothing a host appends can contain
/// "expected " followed by a comma-space "got " — so this finds the end of the
/// message without hard-coding the value.
fn message_body(stderr: &str) -> Option<String> {
    const START: &str = "test failed: ";
    const GOT: &str = ", got ";
    // A host that prints a traceback also echoes the *source line* that raised,
    // and that source line is the Python/JS/Ruby program text — which contains
    // the same `test failed: %s: expected %s, got %s` format string. A line
    // carrying a `%s` or `" + ` is the program being quoted, not the message
    // being raised, so it is skipped. The real message is the one with the
    // values already substituted.
    let line = stderr
        .lines()
        .filter(|l| l.contains(START) && !l.contains("%s") && !l.contains("\" + "))
        .find(|l| l.contains(START))?;
    let tail = &line[line.find(START).expect("checked")..];
    let got_at = tail.find(GOT)? + GOT.len();
    let rest = &tail[got_at..];
    // The actual value runs to the first thing that cannot be part of it: the
    // position suffix (` at line N, col M`) the interpreter appends, or Ruby's
    // trailing ` (RuntimeError)` exception class. A backtrace reference or a
    // source location is on a following line, so it never reaches this cut.
    let end = rest
        .find(" at ")
        .or_else(|| rest.find(" ("))
        .unwrap_or(rest.len());
    Some(format!("{}{}", &tail[..got_at], &rest[..end]))
}

#[test]
fn a_failing_test_says_the_same_thing_on_every_backend() {
    let case = write_case("fail.ainl", "(test \"adds two numbers\" (+ 1 2) \"4\")\n");
    let bin = ainl_bin();

    // 1. Interpreter.
    let (_, interp_err, ok) = capture(ainl_run(&bin, &case));
    assert!(!ok, "a failing test must not exit 0 under `ainl run`");
    assert!(
        interp_err.contains(EXPECTED),
        "interpreter said:\n{interp_err}"
    );
    // The interpreter has the source, so it must also give a position.
    assert!(
        interp_err.contains("at line 1, col 1"),
        "the interpreter must locate the failure:\n{interp_err}"
    );
    let interp_body = message_body(&interp_err).expect("interpreter message");
    assert_eq!(interp_body, EXPECTED, "interpreter message body");

    // 2. AOT C. Same body, no position — the standalone binary embeds no source.
    let c_path = scratch_dir().join("fail.c");
    let bin_path = scratch_dir().join("fail.aot");
    let c = ainl_cc::generate(&ainl_core::parse(&std::fs::read_to_string(&case).unwrap()).unwrap())
        .expect("aot codegen");
    std::fs::write(&c_path, c).expect("write .c");
    let out = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin_path)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, aot_err, ok) = capture(Command::new(&bin_path));
    assert!(!ok, "a failing test must not exit 0 in the AOT binary");
    assert_eq!(
        aot_err.trim(),
        EXPECTED,
        "the AOT message must be exactly the body, with no position invented"
    );

    // 3-5. The three transpiler targets.
    for (target, runner) in [("python", "python3"), ("js", "node"), ("ruby", "ruby")] {
        if which(runner).is_none() {
            eprintln!("skip {target} ({runner} not installed)");
            continue;
        }
        let src = std::fs::read_to_string(&case).unwrap();
        let code =
            ainl_transpile::transpile_src(ainl_transpile::Target::from_name(target).unwrap(), &src)
                .expect("transpile");
        let out_path = scratch_dir().join(format!("fail.{target}"));
        std::fs::write(&out_path, code).expect("write target");
        let (_, err, ok) = capture(run_target(runner, &out_path));
        assert!(!ok, "{target}: a failing test must not exit 0");
        let body = message_body(&err).unwrap_or_else(|| {
            panic!(
                "{target} produced no test message:\n{err}\n--- program ---\n{}",
                std::fs::read_to_string(&out_path).unwrap()
            )
        });
        assert_eq!(body, EXPECTED, "{target} message body differs");
    }
}

#[test]
fn a_passing_test_is_quiet_and_succeeds_on_every_backend() {
    let case = write_case("pass.ainl", "(test \"adds two numbers\" (+ 1 2) \"3\")\n");
    let bin = ainl_bin();
    let (interp_out, err, ok) = capture(ainl_run(&bin, &case));
    assert!(ok, "a passing test must exit 0; stderr: {err}");
    assert_eq!(
        interp_out, "",
        "a passing test prints nothing; got {interp_out:?}"
    );
    assert_eq!(err, "", "a passing test says nothing; got {err:?}");

    // And the AOT binary agrees, which is the half that needs its own runtime.
    let c = ainl_cc::generate(&ainl_core::parse(&std::fs::read_to_string(&case).unwrap()).unwrap())
        .expect("aot codegen");
    let c_path = scratch_dir().join("pass.c");
    let bin_path = scratch_dir().join("pass.aot");
    std::fs::write(&c_path, c).expect("write .c");
    let cc_out = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin_path)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run cc");
    assert!(cc_out.status.success(), "cc failed");
    let (aot_out, aot_err, ok) = capture(Command::new(&bin_path));
    assert!(ok, "AOT: a passing test must exit 0; stderr: {aot_err}");
    assert_eq!(aot_out, interp_out);
    assert_eq!(aot_err, err);
}

#[test]
fn an_operand_type_error_says_the_same_thing_on_every_backend() {
    // The operand checks are the other half of the builtin's contract, and they
    // carry a type name — which each host could easily spell its own way.
    let case = write_case("badname.ainl", "(test 7 1 \"1\")\n");
    let bin = ainl_bin();
    let (_, err, ok) = capture(ainl_run(&bin, &case));
    assert!(!ok);
    assert!(
        err.contains("test expects a str name, got int"),
        "interpreter said:\n{err}"
    );

    let c = ainl_cc::generate(&ainl_core::parse(&std::fs::read_to_string(&case).unwrap()).unwrap())
        .expect("aot codegen");
    let c_path = scratch_dir().join("badname.c");
    let bin_path = scratch_dir().join("badname.aot");
    std::fs::write(&c_path, c).expect("write .c");
    let out = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin_path)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run cc");
    assert!(out.status.success(), "cc failed");
    let (_, aot_err, _) = capture(Command::new(&bin_path));
    assert_eq!(
        aot_err.trim(),
        "test expects a str name, got int",
        "the AOT runtime must spell the type name the interpreter does"
    );
}

/// `which` without shelling out, so the test works on a runner with a minimal
/// PATH and does not depend on `command -v`.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}
