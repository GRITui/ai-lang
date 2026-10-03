//! Pins the numeric-model behaviour documented in docs/NUMERIC_MODEL.md.
//!
//! The interpreter, Python and Ruby all use arbitrary-precision integers, so
//! they agree exactly. JavaScript has only `f64`, so it rounds — a limit of the
//! target language, not of the transpiler. This test locks in the *current*
//! measured value for each target so a future change to any one target's
//! arithmetic shows up as a visible, deliberate diff here instead of silent
//! drift away from what's documented. Gracefully skips a target whose runtime
//! isn't installed, matching `scripts/check-transpile.sh`.

use ainl_transpile::{transpile_js_src, transpile_python_src, transpile_ruby_src};
use std::process::Command;

const SRC: &str = r#"(print (* 9223372036854775807 2))
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))"#;

/// The exact result, shared by the interpreter, Python and Ruby.
const EXACT: &str = "18446744073709551614\n15511210043330985984000000";

/// JS's `f64` answer for the same program (the one remaining divergence).
const JS_FLOAT: &str = "18446744073709552000\n1.5511210043330986e+25";

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
        assert_eq!(out, JS_FLOAT);
    }
}

#[test]
fn python_computes_the_exact_arbitrary_precision_result() {
    let py = transpile_python_src(SRC).unwrap();
    if let Some(out) = run("python3", &py, "python") {
        assert_eq!(out, EXACT);
    }
}

#[test]
fn ruby_computes_the_exact_arbitrary_precision_result() {
    let rb = transpile_ruby_src(SRC).unwrap();
    if let Some(out) = run("ruby", &rb, "ruby") {
        assert_eq!(out, EXACT);
    }
}

#[test]
fn python_and_ruby_agree_with_each_other() {
    // Both have arbitrary-precision integers, so they must match exactly.
    let py_out = run("python3", &transpile_python_src(SRC).unwrap(), "python-cmp");
    let rb_out = run("ruby", &transpile_ruby_src(SRC).unwrap(), "ruby-cmp");
    if let (Some(py_out), Some(rb_out)) = (py_out, rb_out) {
        assert_eq!(py_out, rb_out);
    }
}

#[test]
fn the_interpreter_agrees_exactly_with_python_and_ruby() {
    // The change that closed the interpreter half of the divergence: it used
    // to promote `i64` overflow to `f64` and print a rounded float. Both its
    // evaluators (bytecode VM and tree-walk) must now produce the same exact
    // integer digits Python and Ruby do.
    //
    // Each expression is evaluated on its own — `SRC` ends in a `print`, which
    // returns nil, so its value says nothing about the arithmetic.
    let prelude = "(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))\n";
    for (expr, want) in [
        ("(* 9223372036854775807 2)", "18446744073709551614"),
        ("(fact 25)", "15511210043330985984000000"),
    ] {
        let src = format!("{prelude}{expr}");
        let vm = ainl_core::run_str(&src).unwrap_or_else(|e| panic!("vm {expr}: {e}"));
        let tw =
            ainl_core::run_in_tree_walk(&src).unwrap_or_else(|e| panic!("tree-walk {expr}: {e}"));
        assert_eq!(format!("{vm}"), want, "vm on {expr}");
        assert_eq!(format!("{tw}"), want, "tree-walk on {expr}");
    }
}

#[test]
fn the_interpreter_still_diverges_from_js_by_design() {
    // The remaining, deliberate divergence: `f64` cannot represent these.
    // If this ever fails, JS moved to BigInt and the doc needs updating.
    let out = run("node", &transpile_js_src(SRC).unwrap(), "js-vs-interp");
    if let Some(out) = out {
        assert_eq!(out, JS_FLOAT);
        assert_ne!(out, EXACT);
    }
}
