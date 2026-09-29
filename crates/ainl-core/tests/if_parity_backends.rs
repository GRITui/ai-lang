//! `if` 4-backend parity — a 2-arg `if` in a closure body, on all five runners.
//!
//! The sibling `if_expr.rs` proves the VM and the tree-walk agree. This file
//! covers the three runners that never execute a bytecode stream at all: the
//! AOT C binary, and the python3 / node / ruby transpiler targets. They were
//! never wrong — each emits a real host `if`/`else` or ternary into ONE
//! temporary, so there is no operand stack to over-push — and this test is
//! what makes "they were never wrong" a checked claim rather than an
//! assumption, so a future lowering rewrite cannot silently introduce the
//! stack discipline the VM has.
//!
//! Every backend must produce BYTE-IDENTICAL stdout, stderr and exit code,
//! which is the 4-backend rule. Requires a host C compiler and python3/node/ruby;
//! a missing host is a SKIP (reported, not failed) so this stays usable on a
//! bare dev box, matching the AOT suite's convention of panicking only when a
//! present compiler fails.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join("ainl-if-parity");
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn have(prog: &str, arg: &str) -> bool {
    Command::new(prog).arg(arg).output().is_ok()
}

/// The release `ainl` binary. The parity tests shell out to it because the
/// library entry points return a program's last VALUE, while a parity check
/// needs a program's printed STDOUT.
fn ainl_binary() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target/release/ainl")
}

/// stdout+stderr and the exit code, as one comparable blob.
fn capture(prog: &str, args: &[&str]) -> (String, i32) {
    let out = Command::new(prog).args(args).output().expect("spawn");
    let mut blob = String::from_utf8_lossy(&out.stdout).to_string();
    blob.push_str(&String::from_utf8_lossy(&out.stderr));
    (blob, out.status.code().unwrap_or(-1))
}

fn src() -> String {
    r#"
(def probe (fn (x)
             (if (= x 0) (print "hit"))
             (print "done")))
(print (probe 0))
(print (probe 5))
"#
    .to_string()
}

/// Compile through the AOT C backend and run the resulting binary.
fn aot_c() -> Option<(String, i32)> {
    if !have("cc", "--version") {
        return None;
    }
    let ainl = ainl_binary();
    if !ainl.exists() {
        return None; // release build absent: nothing to compare against
    }
    let dir = scratch();
    let src_path = dir.join("parity.ainl");
    std::fs::write(&src_path, src()).expect("write ainl");
    let bin = dir.join("parity_aot");
    let c = dir.join("parity_aot.c");
    let status = Command::new(&ainl)
        .args([
            "compile",
            src_path.to_str().unwrap(),
            "-o",
            bin.to_str().unwrap(),
        ])
        .args(["--keep-c", c.to_str().unwrap()])
        .output()
        .expect("ainl compile");
    assert!(
        status.status.success(),
        "AOT compile failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    Some(capture(bin.to_str().unwrap(), &[]))
}

/// Transpile to one host and run it. Returns None when the host is absent.
fn transpiled(lang: &str, runner: &str) -> Option<(String, i32)> {
    if !have(runner, "--version") {
        return None;
    }
    let ainl = ainl_binary();
    if !ainl.exists() {
        return None;
    }
    let dir = scratch();
    let src_path = dir.join("parity.ainl");
    std::fs::write(&src_path, src()).expect("write ainl");
    let out = dir.join(format!("parity.{lang}"));
    let status = Command::new(&ainl)
        .args(["transpile", src_path.to_str().unwrap(), "--to", lang])
        .output()
        .expect("ainl transpile");
    assert!(
        status.status.success(),
        "transpile to {lang} failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    // `ainl transpile` prints the host program on stdout.
    std::fs::write(&out, &status.stdout).expect("write host src");
    Some(capture(runner, &[out.to_str().unwrap()]))
}

/// Assert every present backend matches `expected` byte-for-byte.
fn assert_all_agree(expected: &(String, i32), results: Vec<(&str, Option<(String, i32)>)>) {
    for (name, got) in results {
        match got {
            None => eprintln!("SKIP {name}: host not installed"),
            Some((blob, code)) => {
                assert_eq!(
                    (blob.clone(), code),
                    expected.clone(),
                    "backend {name} diverged from the tree-walk baseline"
                );
            }
        }
    }
}

#[test]
fn two_arg_if_true_is_identical_on_every_backend() {
    // The baseline is `ainl run` in a subprocess, not the in-process
    // `run_in_tree_walk`: the value a library call returns is only the LAST
    // form's value, while every backend here produces a program's full
    // printed stdout. Comparing printed output to a returned value would
    // compare "nil" against "hit\ndone\n..." and fail for the wrong reason.
    // `ainl run` IS the VM, and it is the reference the other CI parity
    // scripts (`check-transpile.sh`, `check-aot.sh`) already use.
    let expected = vm_baseline(&src());
    assert!(
        expected.0.contains("hit\ndone"),
        "the VM did not run the card's repro: {:?}",
        expected.0
    );

    // The AOT binary and the three transpiler hosts must each print exactly
    // the same bytes. The blob folds stderr in so a backend that warns
    // (e.g. a Python deprecation) cannot pass by being quiet where the
    // others are not.
    let results = vec![
        ("aot-c", aot_c()),
        ("python", transpiled("python", "python3")),
        ("node", transpiled("js", "node")),
        ("ruby", transpiled("ruby", "ruby")),
    ];
    assert_all_agree(&expected, results);
}

/// Run `prog` through the VM (`ainl run`) and return its stdout+stderr blob
/// and exit code. This is the baseline every other backend is compared to.
fn vm_baseline(prog: &str) -> (String, i32) {
    let ainl = ainl_binary();
    if !ainl.exists() {
        panic!(
            "target/release/ainl not found at {} — build with `cargo build --release` \
             before running the backend parity tests",
            ainl.display()
        );
    }
    let path = scratch().join(format!(
        "baseline_{}.ainl",
        prog.len() as u64 // distinct per program, stable across runs
    ));
    std::fs::write(&path, prog).expect("write ainl");
    capture(ainl.to_str().unwrap(), &["run", path.to_str().unwrap()])
}

/// The `if` in a `let` body, and nested `if`s, on the backends that can run
/// here. This is the shape the e2e corpus used (`05_organize`'s `do-move`),
/// which is where the leak was first seen in the field.
#[test]
fn two_arg_if_in_a_let_body_is_identical_on_every_backend() {
    let prog = r#"
(def probe (fn (x)
             (let ((v (if (= x 0) (print "hit"))))
               (str "done " (str x)))))
(print (probe 0))
(print (probe 5))
"#;
    let expected = vm_baseline(prog);
    assert!(
        expected.0.contains("hit\ndone 0"),
        "the VM did not run the let-body repro: {:?}",
        expected.0
    );

    // Reuse the generic backend runners, which read `src()`; write this
    // program's own file and drive the transpilers/AOT over it directly so
    // the same helper covers all four.
    let dir = scratch();
    let ainl = ainl_binary();
    let src_path = dir.join("parity_let.ainl");
    std::fs::write(&src_path, prog).expect("write ainl");

    if have("cc", "--version") {
        let bin = dir.join("parity_let_aot");
        let c = dir.join("parity_let_aot.c");
        let st = Command::new(&ainl)
            .args([
                "compile",
                src_path.to_str().unwrap(),
                "-o",
                bin.to_str().unwrap(),
                "--keep-c",
                c.to_str().unwrap(),
            ])
            .output()
            .expect("compile");
        assert!(st.status.success(), "AOT compile failed");
        let got = capture(bin.to_str().unwrap(), &[]);
        assert_eq!(got, expected, "aot-c diverged from the VM on the let body");
    } else {
        eprintln!("SKIP aot-c: cc not installed");
    }

    for (lang, runner) in [("python", "python3"), ("js", "node"), ("ruby", "ruby")] {
        if !have(runner, "--version") {
            eprintln!("SKIP {runner}: not installed");
            continue;
        }
        let out = dir.join(format!("parity_let.{lang}"));
        let status = Command::new(&ainl)
            .args(["transpile", src_path.to_str().unwrap(), "--to", lang])
            .output()
            .expect("transpile");
        assert!(status.status.success(), "transpile {lang} failed");
        std::fs::write(&out, &status.stdout).expect("write");
        let got = capture(runner, &[out.to_str().unwrap()]);
        assert_eq!(
            got, expected,
            "{runner} diverged from the VM on the let body"
        );
    }
}

/// The 3-arg `if` still takes its else everywhere — the fix touched only the
/// 2-arg arm, and this is what stops a future "unify the two arms" edit from
/// quietly dropping the else. Value-level (no print), so the library entry
/// points compare directly here.
#[test]
fn three_arg_if_takes_its_else_on_every_backend() {
    let prog = r#"
(list (if true "t" "e") (if false "t" "e") (if (= 1 1) "a" "b"))
"#;
    let vm = ainl_core::run_str(prog).expect("vm");
    let tree = ainl_core::run_in_tree_walk(prog).expect("tree-walk");
    assert_eq!(
        vm.to_string(),
        tree.to_string(),
        "VM diverged from the tree-walk"
    );
    assert_eq!(vm.to_string(), r#"("t" "e" "a")"#);
}
