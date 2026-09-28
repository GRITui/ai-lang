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
    let c = ainl_cc::generate(&forms);
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
    let ainl = repo_root().join("target/release/ainl");
    let ainl = if ainl.exists() {
        ainl
    } else {
        repo_root().join("target/debug/ainl")
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
