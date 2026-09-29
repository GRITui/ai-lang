//! AOT parity: the compiled binary's stdout must be byte-identical to the
//! interpreter's stdout for every example and for the 40k benchmark.
//!
//! This is the in-suite version of `scripts/check-aot.sh` (which CI runs on
//! a release build, together with the timing gates). Here we care about
//! *semantics*: AINL -> C -> cc must not change what a program prints.
//!
//! Requires a host C compiler (`cc`); the AOT backend is C by design.

use std::path::{Path, PathBuf};
use std::process::Command;

const N: i64 = 40_000;
const EXPECTED_SUM: i64 = 799_980_000; // 0 + 1 + ... + 39999

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Compile `src` to C, hand it to `cc`, and return the binary path.
fn compile_aot(src: &str, name: &str) -> Option<PathBuf> {
    if Command::new("cc").arg("--version").output().is_err() {
        panic!("cc not found: the AOT backend needs a host C compiler");
    }
    let forms = ainl_core::parse(src).expect("parse");
    let c = ainl_cc::generate(&forms).expect("aot codegen");
    let dir = std::env::temp_dir().join(format!("ainl-aot-test-{name}"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let c_path = dir.join(format!("{name}.c"));
    let bin = dir.join(name);
    std::fs::write(&c_path, c).expect("write .c");
    // `-lm`: on glibc (Linux) fmod() lives in libm, not libc, so the link
    // fails without it. macOS folds libm into libSystem, hence the flag is
    // redundant (but harmless) there.
    let out = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "cc failed for {name}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(bin)
}

fn run_capture(cmd: &mut Command) -> String {
    let out = cmd.output().expect("run");
    assert!(
        out.status.success(),
        "command {cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

/// The interpreter's stdout for a program, captured by running the same
/// `print` calls in-process. Printing goes to the process's stdout, so we
/// instead re-derive it by running the AINL file through a tiny driver: the
/// simplest faithful comparison is the CLI's own `run`.
fn interpreter_stdout(path: &Path) -> String {
    // Debug before release: `cargo test` rebuilds only the debug binary, so a
    // release binary found first here can be stale (and the cargo cache's key
    // is Cargo.toml-only, so it can carry one across runs). See aot_stdlib.rs
    // for the full story.
    let ainl = {
        let debug = repo_root().join("target/debug/ainl");
        if debug.exists() {
            debug
        } else {
            repo_root().join("target/release/ainl")
        }
    };
    if !ainl.exists() {
        panic!(
            "ainl binary not built ({} / {}); run `cargo build` first",
            ainl.display(),
            repo_root().join("target/release/ainl").display()
        );
    }
    let mut cmd = Command::new(ainl);
    cmd.arg("run").arg(path);
    run_capture(&mut cmd)
}

fn assert_parity(rel: &str) {
    let path = repo_root().join(rel);
    let src = std::fs::read_to_string(&path).expect("read example");
    let name = path.file_stem().unwrap().to_string_lossy().into_owned();
    let bin = compile_aot(&src, &name).expect("compiled");
    let got = run_capture(&mut Command::new(&bin));
    let want = interpreter_stdout(&path);
    assert_eq!(
        got, want,
        "AOT stdout differs from the interpreter for {rel} (AOT first, interpreter second)"
    );
}

#[test]
fn aot_matches_interpreter_on_all_examples() {
    for ex in ["hello.ainl", "fib.ainl", "lists.ainl", "maps.ainl"] {
        assert_parity(&format!("examples/{ex}"));
    }
}

/// `and`/`or` return an OPERAND, not a boolean (SYNTAX.md 2). The AOT emitter
/// used to seed its accumulator with `v_bool(1)` and overwrite it only on the
/// short-circuit path, so an all-truthy chain returned that seed: `(and 1 2 3)`
/// printed `true` under AOT C while every other backend printed `3`.
///
/// The comparison is against the interpreter rather than a literal, because the
/// interpreter is the language's definition. The cases deliberately use
/// non-boolean operands, since with booleans alone "returns a boolean" and
/// "returns the operand" are indistinguishable — which is exactly why the bug
/// survived the collections gate. Note that only `nil`/`false` are falsey, so
/// `0` and `""` are truthy operands here.
#[test]
fn aot_and_or_return_operands_not_booleans() {
    let src = r#"
(print (and 1 2))
(print (and 0 1))
(print (and 0 ""))
(print (and 1 2 3))
(print (and 1 nil))
(print (and nil 1))
(print (and false 1))
(print (and "a" "b"))
(print (and 0 (list 1 2)))
(print (and))
(print (and 7))
(print (or 0 1))
(print (or nil 1))
(print (or nil 0))
(print (or 1 2 3))
(print (or nil (list 1 2)))
(print (or "a" "b"))
(print (or))
(print (or 7))
(print (and 1 (or nil "inner")))
(print (or nil (and 1 "inner")))
"#;
    let bin = compile_aot(src, "and_or_operands").expect("compiled");
    let got = run_capture(&mut Command::new(&bin));

    // Run the identical source through the interpreter, via the CLI.
    let path = repo_root().join("target/and_or_operands.ainl");
    std::fs::write(&path, src).expect("write .ainl");
    let want = interpreter_stdout(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        got, want,
        "AOT and/or returned a different value than the interpreter"
    );
    // Pin the answers themselves, so a change in what `and`/`or` MEAN cannot
    // pass by moving both backends together. `and` returns the LAST operand;
    // `or` returns the FIRST truthy one — so `(or 1 2 3)` is `1` and
    // `(or "a" "b")` is `a`, while `(and 1 2 3)` is `3` and `(and "a" "b")`
    // is `b`. Line 3 is `(and 0 "")`, which is the empty string: a blank line.
    assert_eq!(
        want.trim(),
        "2\n1\n\n3\nnil\nnil\nfalse\nb\n(1 2)\ntrue\n7\n0\n1\n0\n1\n(1 2)\na\nfalse\n7\ninner\ninner",
        "and/or semantics changed"
    );
}

/// The value must survive as a real value, not just print correctly: a string
/// or list returned from the AOT accumulator is read again by the next form, so
/// losing the reference here would corrupt `str`/`len` rather than this output.
#[test]
fn aot_and_or_result_is_usable_afterwards() {
    let src = r#"
(def v (and 1 "kept"))
(print v)
(print (str "x" (and 1 "kept")))
(def w (or nil (list 1 2)))
(print (len w))
(def pick (fn (x) (and x "default")))
(print (pick 0))
(print (pick nil))
"#;
    let bin = compile_aot(src, "and_or_usable").expect("compiled");
    let got = run_capture(&mut Command::new(&bin));
    let path = repo_root().join("target/and_or_usable.ainl");
    std::fs::write(&path, src).expect("write .ainl");
    let want = interpreter_stdout(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!(got, want, "AOT and/or result was not usable afterwards");
}

/// `and`/`or` are lazy. The emitter has to test the accumulator BEFORE emitting
/// the next operand's code, or a `goto` that gets the VALUE right still
/// evaluates every operand on every run. A `print` in the dead branch is the
/// observable difference, and `(/ 1 0)` there proves the operand was never
/// evaluated at all.
///
/// The count is not asserted: a `fn` body ending in a value prints twice on
/// every backend (a pre-existing quirk, unrelated to `and`/`or`), so only the
/// *absence* of DEAD is a meaningful invariant here.
#[test]
fn aot_and_or_short_circuit() {
    let src = r#"
(def note (fn (s) (print s) nil))
(print (and 1 (note "LIVE")))
(print (or nil 1))
(print (and false (note "DEAD")))
(print (or true (note "DEAD")))
(print (and nil (note "DEAD")))
(print (or 0 (note "DEAD")))
(print (and false (/ 1 0)))
(print (or 1 (/ 1 0)))
"#;
    let bin = compile_aot(src, "and_or_short_circuit").expect("compiled");
    let got = run_capture(&mut Command::new(&bin));
    let path = repo_root().join("target/and_or_short_circuit.ainl");
    std::fs::write(&path, src).expect("write .ainl");
    let want = interpreter_stdout(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!(got, want, "AOT and/or short-circuiting diverged");
    assert!(!got.contains("DEAD"), "a dead branch was evaluated: {got}");
    assert!(
        !got.contains("division"),
        "a dead branch raised a division error: {got}"
    );
}

#[test]
fn aot_40k_sum_is_correct() {
    let src = format!("(def i 0)\n(def s 0)\n(while (< i {N})\n  (def s (+ s i))\n  (def i (+ i 1)))\n(print s)\n");
    let bin = compile_aot(&src, "loop40k_sum").expect("compiled");
    let got = run_capture(&mut Command::new(&bin));
    assert_eq!(
        got.trim(),
        EXPECTED_SUM.to_string(),
        "40k sum-to-N is wrong under AOT"
    );
}

/// A loop long enough to blow the default 2,000,000-step cap, but short
/// enough (1e7 iterations, ~0.3 s compiled) to keep the tests quick.
const RUNAWAY_SRC: &str = "(def i 0)\n(while (< i 10000000) (def i (+ i 1)))\n(print i)\n";

#[test]
fn aot_step_counter_defaults_to_two_million() {
    // A runaway loop must be bounded by the default 2,000,000-step cap.
    let bin = compile_aot(RUNAWAY_SRC, "runaway_default").expect("compiled");
    let out = Command::new(&bin).output().expect("run");
    assert!(!out.status.success(), "runaway loop should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("step limit exceeded") && err.contains("2000000"),
        "expected the default 2,000,000-step cap, got: {err}"
    );
}

#[test]
fn aot_step_counter_honours_ainl_max_steps() {
    let bin = compile_aot(RUNAWAY_SRC, "runaway_override").expect("compiled");

    // A small override trips immediately, quoting the override's value.
    let out = Command::new(&bin)
        .env("AINL_MAX_STEPS", "50")
        .output()
        .expect("run");
    assert!(!out.status.success(), "should trip at 50 steps");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("max 50 evaluation steps"),
        "AINL_MAX_STEPS=50 not honored: {err}"
    );

    // A raised override lets the same program finish. The cap is checked with
    // `g_steps > g_max_steps`, and the while loop ticks once per iteration plus
    // once for the final failing condition, so a 1e7-iteration loop needs
    // slightly more than 1e7 steps.
    let out = Command::new(&bin)
        .env("AINL_MAX_STEPS", "20000000")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "AINL_MAX_STEPS=20000000 should allow 1e7 iterations: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "10000000");
}
