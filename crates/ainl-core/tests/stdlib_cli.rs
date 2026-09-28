//! CLI-level tests for the two stdlib builtins that cannot be tested in-process.
//!
//! `exit` terminates the process by design, so it can only be exercised through
//! a subprocess — running it inside a `#[test]` would kill the test harness.
//! These tests therefore drive the real `ainl` binary, which also covers the
//! one path the library tests cannot: `ainl run` returning the builtin's exit
//! code as the process's own.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn ainl_bin() -> PathBuf {
    // Debug before release: `cargo test` rebuilds only the debug binary, so a
    // release binary found first here can be stale (and the cargo cache's key
    // is Cargo.toml-only, so it can carry one across runs). See aot_stdlib.rs
    // for the full story.
    let release = {
        let debug = repo_root().join("target/debug/ainl");
        if debug.exists() {
            debug
        } else {
            repo_root().join("target/release/ainl")
        }
    };
    if !release.exists() {
        panic!(
            "ainl binary not built ({}); run `cargo build` first",
            release.display()
        );
    }
    release
}

/// Run a snippet through `ainl eval` and return `(exit_code, stdout)`.
fn eval_snippet(src: &str) -> (i32, String) {
    let out = Command::new(ainl_bin())
        .arg("eval")
        .arg(src)
        .output()
        .expect("run ainl eval");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn exit_terminates_with_the_given_code() {
    // Each code is checked separately: a process exits with a code modulo 256
    // on POSIX, so 0 / 3 / 42 are all distinct and none is a truncation case.
    for code in [0i64, 3, 42] {
        let (got_code, out) = eval_snippet(&format!("(exit {code})"));
        assert_eq!(
            got_code, code as i32,
            "`(exit {code})` exited with {got_code}"
        );
        assert_eq!(out, "", "(exit) must print nothing before exiting");
    }
}

#[test]
fn exit_flushes_stdout_written_before_it() {
    // `print` writes to a LineWriter, so a whole line is normally already out
    // when `process::exit` runs — but `exit` must not *depend* on that detail.
    // Buffered output surviving the abrupt exit is the contract this pins.
    let (code, out) = eval_snippet("(print \"before\") (exit 7)");
    assert_eq!(code, 7, "expected exit code 7, stdout was {out:?}");
    assert_eq!(out, "before\n", "output before `exit` was lost");
}

#[test]
fn exit_with_a_non_int_operand_is_a_runtime_error_not_a_crash() {
    // `(exit "0")` must fail the same way any other type error does: a clean
    // `runtime error:` on stderr with the CLI's failure code, and no exit
    // status of the string reinterpreted as an int.
    let out = Command::new(ainl_bin())
        .arg("eval")
        .arg(r#"(exit "0")"#)
        .output()
        .expect("run ainl eval");
    assert!(!out.status.success(), "a bad exit operand must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("exit expects an int, got str"), "got: {err}");
}

#[test]
fn stdlib_builtins_are_reachable_from_the_cli() {
    // A smoke pass over the whole stdlib through the real binary: this is the
    // path check-transpile.sh and check-aot.sh compare against, so a builtin
    // that only works in the library (e.g. bound to a different prelude) would
    // show up here.
    let (code, out) = eval_snippet(
        r#"(do
             (print (join (split "a,b" ",") "|"))
             (print (upcase (trim "  hi  ")))
             (print (replace "aaa" "a" "b"))
             (print (contains "hay" "a"))
             (print (abs -3) (min 3 1) (max 3 1) (floor 1.9) (sqrt 16)))"#,
    );
    assert_eq!(code, 0, "stdlib smoke snippet failed: {out:?}");
    assert_eq!(
        // `ainl eval` prints the snippet's result value after any `print`
        // output — here the `do`'s value, nil.
        out,
        "a|b\nHI\nbbb\ntrue\n3 1 3 1 4.0\nnil\n",
        "unexpected stdlib output"
    );
}
