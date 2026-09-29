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
//! which is the 4-backend rule.
//!
//! The transpiler and AOT halves need `target/release/ainl` and their hosts.
//! This crate's `cargo test` job builds DEBUG only, so a missing release
//! binary is a SKIP here, not a failure. The full five-runner gate is
//! `scripts/check-if-parity.sh`, which CI runs in a job that has a release
//! build plus python3/node/ruby. The value-level cases at the bottom need
//! neither and therefore run in every job — the bug's own signal is asserted
//! wherever the suite runs, not only where a release binary happens to exist.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory PRIVATE to one test.
///
/// Per-test, not shared: `cargo test` runs the cases in this file in parallel
/// threads, and they all write an AINL file and read it back. A shared name
/// made each test compile the OTHER test's program — which surfaces as a
/// backend "diverging" with a plausible-looking program, not as a file error,
/// so it is worth the extra nesting to be immune. The test name is part of
/// the path, so the isolation survives a rename.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("ainl-if-parity").join(test);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn have(prog: &str, arg: &str) -> bool {
    Command::new(prog).arg(arg).output().is_ok()
}

/// The release `ainl` binary, or None when this job built debug only. The
/// parity checks shell out to it because the library entry points return a
/// program's last VALUE, while parity needs a program's printed STDOUT.
fn ainl_binary() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target/release/ainl");
    p.exists().then_some(p)
}

/// stdout+stderr and the exit code, as one comparable blob. stderr is folded
/// in so a backend that warns (a Python deprecation, a Ruby warning) cannot
/// pass by being quiet where the others are not.
fn capture(prog: &str, args: &[&str]) -> (String, i32) {
    let out = Command::new(prog).args(args).output().expect("spawn");
    let mut blob = String::from_utf8_lossy(&out.stdout).to_string();
    blob.push_str(&String::from_utf8_lossy(&out.stderr));
    (blob, out.status.code().unwrap_or(-1))
}

/// The card's repro: a 2-arg `if` in a closure body, both conditions, and
/// TWO calls per program. The second call is the point — a leaked operand is
/// only *fatal* on the following call, so a single-call program returns the
/// right value either way. That is precisely why this bug shipped.
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

/// Run `prog` through the VM (`ainl run`) and return its printed blob and exit
/// code. This is the baseline every other backend is compared to.
fn vm_baseline(test: &str, prog: &str) -> Option<(String, i32)> {
    let ainl = ainl_binary()?;
    let path = scratch(test).join("baseline.ainl");
    std::fs::write(&path, prog).expect("write ainl");
    Some(capture(
        ainl.to_str().unwrap(),
        &["run", path.to_str().unwrap()],
    ))
}

/// Compile `prog` through the AOT C backend and run the resulting binary.
fn aot_c(test: &str, prog: &str) -> Option<(String, i32)> {
    if !have("cc", "--version") {
        return None;
    }
    let ainl = ainl_binary()?;
    let dir = scratch(test);
    let src_path = dir.join("prog.ainl");
    std::fs::write(&src_path, prog).expect("write ainl");
    let bin = dir.join("prog_aot");
    let c = dir.join("prog_aot.c");
    let status = Command::new(&ainl)
        .args([
            "compile",
            src_path.to_str().unwrap(),
            "-o",
            bin.to_str().unwrap(),
            "--keep-c",
            c.to_str().unwrap(),
        ])
        .output()
        .expect("ainl compile");
    assert!(
        status.status.success(),
        "AOT compile failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    Some(capture(bin.to_str().unwrap(), &[]))
}

/// Transpile `prog` to one host and run it. None when the host or the release
/// binary is absent.
fn transpiled(test: &str, prog: &str, lang: &str, runner: &str) -> Option<(String, i32)> {
    if !have(runner, "--version") {
        return None;
    }
    let ainl = ainl_binary()?;
    let dir = scratch(test);
    let src_path = dir.join("prog.ainl");
    std::fs::write(&src_path, prog).expect("write ainl");
    let out = dir.join(format!("prog.{lang}"));
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

/// Assert every present backend matches `expected` byte-for-byte, and report
/// how many actually ran. A missing host is a reported SKIP, never a silent
/// pass — the count is printed so a job that "passed" on zero backends is
/// visible rather than indistinguishable from one that checked five.
fn assert_all_agree(expected: &(String, i32), results: Vec<(&str, Option<(String, i32)>)>) {
    let mut ran = Vec::new();
    for (name, got) in results {
        match got {
            None => eprintln!("SKIP {name}: host or release binary not available"),
            Some(got) => {
                assert_eq!(
                    got,
                    expected.clone(),
                    "backend {name} diverged from the VM baseline"
                );
                ran.push(name);
            }
        }
    }
    println!("backends compared byte-for-byte: {ran:?} (baseline = ainl run / VM)");
}

/// Compare `prog` across all five runners, or SKIP when this build cannot.
/// `test` names this case's private scratch dir, so parallel tests cannot
/// read each other's program.
fn all_five_backends_agree(test: &str, prog: &str, expect_substrings: &[&str]) {
    let Some(expected) = vm_baseline(test, prog) else {
        eprintln!("SKIP: no target/release/ainl (debug-only build)");
        return;
    };
    for want in expect_substrings {
        assert!(
            expected.0.contains(want),
            "the VM did not run the repro (expected {want:?} in {:?})",
            expected.0
        );
    }
    let results = vec![
        ("aot-c", aot_c(test, prog)),
        ("python", transpiled(test, prog, "python", "python3")),
        ("node", transpiled(test, prog, "js", "node")),
        ("ruby", transpiled(test, prog, "ruby", "ruby")),
    ];
    assert_all_agree(&expected, results);
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
    all_five_backends_agree(
        "two_arg_if_true_is_identical_on_every_backend",
        &src(),
        &["hit\ndone", "done\n"],
    );
}

/// The `if` in a `let` body, on every backend. This is the shape the e2e
/// corpus used (`05_organize`'s `do-move`), which is where the leak was
/// first seen in the field.
#[test]
fn two_arg_if_in_a_let_body_is_identical_on_every_backend() {
    let prog = r#"
(def probe (fn (x)
             (let ((v (if (= x 0) (print "hit"))))
               (str "done " (str x)))))
(print (probe 0))
(print (probe 5))
"#;
    all_five_backends_agree(
        "two_arg_if_in_a_let_body_is_identical_on_every_backend",
        prog,
        &["hit\ndone 0", "done 5"],
    );
}

// ---- the cases that need no release binary, so they run in EVERY job ----

/// The 3-arg `if` still takes its else — the fix touched only the 2-arg arm,
/// and this is what stops a future "unify the two arms" edit from quietly
/// dropping the else. Value-level (no `print`), so the library entry points
/// compare directly.
#[test]
fn three_arg_if_takes_its_else_on_both_evaluators() {
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

/// A 2-arg `if` whose then-branch is a value, followed by a real form, inside
/// a closure — at VALUE level, so the stack-discipline bug is asserted in the
/// debug-only job too. The trailing `(str ...)` is the form the leaked `Nil`
/// used to shift; with the leak its operand is the then-value instead.
#[test]
fn two_arg_if_value_semantics_match_the_tree_walk() {
    let prog = r#"
(def probe (fn (x) (if (= x 0) "hit") (str "done-" (str x))))
(list (probe 0) (probe 5))
"#;
    let vm = ainl_core::run_str(prog).expect("vm");
    let tree = ainl_core::run_in_tree_walk(prog).expect("tree-walk");
    assert_eq!(
        vm.to_string(),
        tree.to_string(),
        "VM diverged from the tree-walk"
    );
    assert_eq!(vm.to_string(), r#"("done-0" "done-5")"#);
}

/// The card's own repro at value level: a 2-arg `if` in a fn body followed by
/// a call, then ANOTHER top-level call. Without the fix the VM dies with
/// "cannot call a nil" on the second one, so this fails loudly in a
/// debug-only build where the five-runner gate is skipped.
#[test]
fn two_arg_if_then_a_following_call_does_not_abort() {
    let prog = r#"
(def probe (fn (x)
             (if (= x 0) (print "hit"))
             (print "done")))
(probe 0)
(probe 5)
"#;
    let vm = ainl_core::run_str(prog).expect("the VM aborted: cannot call a nil");
    let tree = ainl_core::run_in_tree_walk(prog).expect("tree-walk");
    assert_eq!(
        vm.to_string(),
        tree.to_string(),
        "VM diverged from the tree-walk"
    );
}
