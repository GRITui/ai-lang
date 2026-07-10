//! Pins the numeric-model divergence documented in docs/NUMERIC_MODEL.md.
//!
//! Integer overflow behaves differently by design across the interpreter, JS,
//! Python, and Ruby (see that doc for why). This test locks in the *current*
//! measured values for each target so a future change to any one target's
//! arithmetic shows up as a visible, deliberate diff here instead of silent
//! drift away from what's documented. Gracefully skips a target whose runtime
//! isn't installed, matching `scripts/check-transpile.sh`.

use ainl_transpile::{transpile_js_src, transpile_python_src, transpile_ruby_src};
use std::process::Command;

const SRC: &str = r#"(print (* 9223372036854775807 2))
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))"#;

fn run(runner: &str, code: &str, tag: &str) -> Option<String> {
    if Command::new(runner).arg("--version").output().is_err() {
        eprintln!("skipping {tag}: `{runner}` not installed");
        return None;
    }
    let tmp = std::env::temp_dir().join(format!("ainl-numeric-divergence-{tag}.tmp"));
    std::fs::write(&tmp, code).unwrap();
    let out = Command::new(runner)
        .arg(&tmp)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {runner}: {e}"));
    std::fs::remove_file(&tmp).ok();
    assert!(
        out.status.success(),
        "{tag} exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn js_diverges_to_float_precision_with_no_promotion_step() {
    let js = transpile_js_src(SRC).unwrap();
    if let Some(out) = run("node", &js, "js") {
        assert_eq!(out, "18446744073709552000\n1.5511210043330986e+25");
    }
}

#[test]
fn python_computes_the_exact_arbitrary_precision_result() {
    let py = transpile_python_src(SRC).unwrap();
    if let Some(out) = run("python3", &py, "python") {
        assert_eq!(out, "18446744073709551614\n15511210043330985984000000");
    }
}

#[test]
fn ruby_computes_the_exact_arbitrary_precision_result() {
    let rb = transpile_ruby_src(SRC).unwrap();
    if let Some(out) = run("ruby", &rb, "ruby") {
        assert_eq!(out, "18446744073709551614\n15511210043330985984000000");
    }
}

#[test]
fn python_and_ruby_agree_with_each_other_but_not_with_js() {
    // Both have arbitrary-precision integers, so they should match exactly —
    // it's specifically JS (and the interpreter) that diverge from them.
    let py_out = run("python3", &transpile_python_src(SRC).unwrap(), "python-cmp");
    let rb_out = run("ruby", &transpile_ruby_src(SRC).unwrap(), "ruby-cmp");
    if let (Some(py_out), Some(rb_out)) = (py_out, rb_out) {
        assert_eq!(py_out, rb_out);
    }
}
