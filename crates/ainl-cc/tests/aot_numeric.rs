//! Numeric-model parity: the AOT-compiled binary must match the AINL
//! interpreter exactly on the numeric edge cases in docs/NUMERIC_MODEL.md.
//!
//! The model is **i64 with promotion to f64 on overflow**, not C's undefined
//! signed-overflow wrapping. `9223372036854775807 + 1` is not
//! `-9223372036854775808` (what naive C emits); it is a float. Anything the
//! two disagree on is a real compiler bug, so each case is asserted against
//! the interpreter's own output rather than a hardcoded string.

use std::path::{Path, PathBuf};
use std::process::Command;

/// FNV-1a, so a case's temp filename is stable across runs and unique per
/// expression without needing a random source.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn compile_aot(src: &str, name: &str) -> PathBuf {
    let forms = ainl_core::parse(src).expect("parse");
    let c = ainl_cc::generate(&forms);
    let dir = std::env::temp_dir().join(format!("ainl-aot-num-{name}"));
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
        .expect("run cc (AOT backend needs a host C compiler)");
    assert!(
        out.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin
}

/// Assert that `(print <expr>)` prints the same thing under AOT and under the
/// interpreter.
///
/// The interpreter is compared through its own `print`, so both sides are
/// reading *stdout* — `(print ...)` evaluates to nil, so the interpreter's
/// return value can't be used to check what was printed. Errors are compared
/// too (both must fail), so a divergence in error handling is also caught.
fn assert_same(expr: &str) {
    let src = format!("(print {expr})\n");
    // Unique per case: the tests run in parallel, and the temp paths below are
    // derived from the expression rather than a counter.
    let bin = compile_aot(&src, &format!("case{:x}", stable_hash(expr.as_bytes())));
    let aot = Command::new(&bin).output().expect("run aot");
    let interp = ainl_core::run_str(&src);
    match (&interp, aot.status.success()) {
        (Ok(_), true) => {
            // The interpreter's print went to this process's stdout, which the
            // test harness cannot capture portably, so the authoritative
            // comparison is against the CLI (`ainl run`), which captures the
            // same code path into a pipe.
            let want = interpreter_stdout(&src, expr);
            assert_eq!(
                String::from_utf8_lossy(&aot.stdout),
                want,
                "AOT stdout differs from the interpreter for `{}`",
                expr
            );
        }
        (Err(_), false) => {}
        (Ok(_), false) => panic!(
            "interpreter succeeded but the AOT binary failed for `{}`: {}",
            expr,
            String::from_utf8_lossy(&aot.stderr)
        ),
        (Err(e), true) => panic!(
            "interpreter errored ({e}) but the AOT binary succeeded for `{}`: {}",
            expr,
            String::from_utf8_lossy(&aot.stdout)
        ),
    }
}

/// Run a program through the CLI's `run` (which captures `print` to stdout).
///
/// `name` must be unique per case: the file is written to a fixed temp path,
/// and these tests run in parallel.
fn interpreter_stdout(src: &str, name: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let ainl = root.join("target/release/ainl");
    let ainl = if ainl.exists() {
        ainl
    } else {
        root.join("target/debug/ainl")
    };
    assert!(
        ainl.exists(),
        "ainl binary not built ({}); run `cargo build` first",
        ainl.display()
    );
    let dir = std::env::temp_dir().join("ainl-aot-num-src");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // `name` is an expression, so it is hashed rather than used verbatim as a
    // path component (it contains spaces, parens and slashes).
    let f = dir.join(format!("{:x}.ainl", stable_hash(name.as_bytes())));
    std::fs::write(&f, src).expect("write");
    let out = Command::new(ainl)
        .arg("run")
        .arg(&f)
        .output()
        .expect("run ainl");
    let _ = std::fs::remove_file(&f);
    assert!(
        out.status.success(),
        "interpreter failed on {src:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

#[test]
fn integer_arithmetic_matches_interpreter() {
    for expr in [
        "(+ 1 2)",
        "(- 5 8)",
        "(* 6 7)",
        "(+ 1 2 3 4 5)",
        "(- 10 1 2)",
        "(* 2 3 4)",
        "(mod 17 5)",
        "(mod -7 3)",
        "(mod 7 -3)",
        "(mod -7 -3)",
        "(- 5)",
        "(/ 7 2)",
        "(/ 1 3)",
    ] {
        assert_same(expr);
    }
}

#[test]
fn i64_overflow_promotes_to_float_not_wrap() {
    // The core of docs/NUMERIC_MODEL.md: an i64 that would overflow becomes
    // an f64, exactly as the interpreter does. Naive C would wrap these.
    let max = "9223372036854775807";
    let min = "-9223372036854775808";
    for expr in [
        // The interpreter: 9223372036854775807 + 1 -> 9.223372036854776e18
        &format!("(+ {max} 1)"),
        &format!("(- {min} 1)"),
        &format!("(* {max} 2)"),
        &format!("(+ {max} {max})"),
        &format!("(- {min} {max})"),
        &format!("(* {min} 2)"),
        &format!("(* 2 {min})"),
        &format!("(* {min} {min})"),
    ] {
        assert_same(expr);
    }
}

#[test]
fn int64_min_magnitude_boundary() {
    // The i64 boundary is where the C runtime's unsigned-magnitude arithmetic
    // is easiest to get wrong, and where two separate bugs lived:
    //
    // - a magnitude product of exactly 2^64 wraps uint64 to 0, which then
    //   *passed* the overflow test (so (* 2 INT64_MIN) returned 0);
    // - negating 2^63 is not representable as int64_t, so `-(int64_t)ur` is UB
    //   and clang folded it to 0.
    //
    // Both must behave exactly like the interpreter: in range where the
    // product fits (note (* -1 INT64_MIN) is +2^63, *not* an overflow — it
    // promotes, because Rust's checked_mul overflows there), and promote to f64
    // where it does not.
    for expr in [
        "(* -1 -9223372036854775808)",
        "(* -9223372036854775808 1)",
        "(* -2 4611686018427387904)",
        "(- -9223372036854775808 0)",
        "(- 0 -9223372036854775808)",
        "(- 0 -9223372036854775807)",
        "(* 0 -9223372036854775808)",
    ] {
        assert_same(expr);
    }
}

#[test]
fn float_formatting_matches_interpreter() {
    // Integer-valued floats print with a trailing ".0" in both engines; the
    // shortest-round-trip formatting is the fiddly part of the port.
    for expr in [
        "(/ 10 5)",
        "(/ 1.0 3)",
        "(/ 2.0 3)",
        "(+ 0.1 0.2)",
        "(* 1.5 2)",
        "(- 1.0 0.5)",
        "(/ 1 0.0)",
        "(/ 0.0 0.0)",
        "(* 1.0e300 1.0e300)",
        "(+ 0.1 0.2 0.3)",
    ] {
        assert_same(expr);
    }
}

#[test]
fn mixed_int_float_and_comparison_match_interpreter() {
    for expr in [
        "(+ 1 2.5)",
        "(* 3 0.5)",
        "(< 1 2.0)",
        "(= 1 1.0)",
        "(= 1.0 1)",
        "(>= 2 1.5)",
        "(str 1.5)",
        "(str 42)",
    ] {
        assert_same(expr);
    }
}

#[test]
fn runtime_errors_match_interpreter() {
    // Both engines must fail, not silently diverge (e.g. div-by-zero is an
    // error in AINL, but a trap or inf in naive C).
    assert_same("(/ 1 0)");
    assert_same("(mod 1 0)");
    assert_same("(< \"a\" 1)");
    assert_same("(+ 1 \"a\")");
}
