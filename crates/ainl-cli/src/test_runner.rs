//! `ainl test` — discover and run AINL test files, then exit non-zero if any
//! test failed.
//!
//! # What a test file is
//!
//! An ordinary AINL program containing `(test "name" expr "expected")` forms.
//! There is no test-specific file type, no registration step, and no
//! discovery convention beyond "a `.ainl` file" — a test file runs the same way
//! `ainl run` runs it, through the same [`ainl_core::run_named_in`] entry
//! point, so `import` resolution and the prelude are identical. That is
//! deliberate: a test harness whose runner differs from the thing it tests is a
//! harness that can pass while the program fails.
//!
//! # Why the exit code is the contract
//!
//! The whole point is CI. `ainl test` exits 0 only when every `(test ...)` in
//! every discovered file passed, and non-zero otherwise, so a shell step needs
//! no parsing. A failing `(test ...)` raises, which aborts its file; the runner
//! reports it and moves on to the next file, so one broken test reports one
//! failure rather than hiding every test after it.

use ainl_core::error::Error;
use ainl_core::parser::Node;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One test's static name, or an empty string when the name is computed.
type TestName = String;

/// A test file's outcome, kept separate from how it is reported.
struct FileResult {
    /// The tests that passed, in source order.
    passed: Vec<TestName>,
    /// The first failure, if any: `(name of the failing test, error)`.
    failed: Option<(TestName, Error)>,
}

/// Walk a program's forms, collecting one entry per `(test ...)` in source
/// order: its literal name, or `""` when the name is a computed expression.
///
/// A computed name is still executed and still fails correctly — it is simply
/// reported without a name, which is honest, since naming it would mean
/// guessing what an unevaluated expression produces.
///
/// `quote` is data, so a quoted `(test ...)` is not a test: nothing inside it
/// ever runs. Same rule as `interpreter_only::find`.
fn collect_tests(forms: &[Node], out: &mut Vec<TestName>) {
    for form in forms {
        let Node::List(items, _) = form else {
            continue;
        };
        let Some(Node::Sym(head, _)) = items.first() else {
            // `()` is legal AINL and has no head; slicing from 1 would panic.
            continue;
        };
        if head == ainl_core::testing::TEST_SYM {
            out.push(match items.get(1) {
                Some(Node::Str(s, _)) => s.clone(),
                _ => String::new(),
            });
        }
        if head == "quote" {
            continue;
        }
        collect_tests(&items[1..], out);
    }
}

/// The name a failure message reports, matched against the file's own test
/// names to recover *which* test broke.
///
/// The message is the parity contract (`test failed: <name>: expected …`, see
/// `ainl_core::testing::failure_message`), so matching on it reads a documented
/// format rather than scraping incidental text. A failure from anywhere else — a
/// genuine bug in the program under test — carries no such prefix and yields
/// `None`, which is the right answer: the file failed for a reason that is not a
/// test mismatch, and no test index exists.
fn failing_index(e: &Error, names: &[TestName]) -> Option<usize> {
    let rest = e.message().strip_prefix("test failed: ")?;
    let name = rest.split(':').next()?;
    names.iter().position(|n| n == name)
}

/// True when this error *is* a test mismatch (as opposed to any other error the
/// program under test may raise).
fn is_mismatch(e: &Error) -> bool {
    e.message().starts_with("test failed: ")
}

/// Run one file and classify the outcome.
fn run_file(path: &Path) -> FileResult {
    let mut res = FileResult {
        passed: Vec::new(),
        failed: None,
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            res.failed = Some((
                String::new(),
                Error::runtime(format!("cannot read {}: {e}", path.display())),
            ));
            return res;
        }
    };
    let forms = match ainl_core::parse(&src) {
        Ok(f) => f,
        Err(e) => {
            res.failed = Some((String::new(), e));
            return res;
        }
    };
    let mut names = Vec::new();
    collect_tests(&forms, &mut names);
    match ainl_core::run_named_in(&src, path, &ainl_core::Env::with_prelude()) {
        Ok(_) => res.passed = names,
        Err(e) => {
            // A failure aborts the file, so every test after the one that broke
            // is untested — not passed. Report what actually ran rather than
            // what the file contained.
            let ran = match failing_index(&e, &names) {
                Some(i) => i + 1,
                // Not a test mismatch: nothing is known to have run, because a
                // program that raises before its first test never reached one.
                None => 0,
            };
            let name = failing_index(&e, &names)
                .and_then(|i| names.get(i).cloned())
                .unwrap_or_default();
            res.passed = names.into_iter().take(ran).collect();
            res.failed = Some((name, e));
        }
    }
    res
}

/// `ainl test <path> [--quiet]`
pub fn run(target: &str, quiet: bool) -> ExitCode {
    let files = match discover(Path::new(target)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ainl test: {e}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        // An empty suite is a real result, and a silent exit 0 would read as
        // "everything passed" — which is how a typo'd path turns green.
        eprintln!("ainl test: no .ainl test files found in {target}");
        return ExitCode::FAILURE;
    }

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    for file in &files {
        let r = run_file(file);
        let shown = display_path(file);
        match r.failed {
            Some((name, e)) if is_mismatch(&e) => {
                failed += 1;
                // The failing test is the last one that ran, and everything
                // before it passed.
                let passed_here = r.passed.len();
                println!("FAIL {shown} — {passed_here} passed, 1 failed: {name}");
                println!("     {e}");
            }
            Some((_, e)) => {
                // A file that failed to parse, or to read, or that raised
                // outside a `(test ...)` is an *error*, not a test failure.
                // The distinction matters: only one is fixed by editing a test.
                errors += 1;
                println!("ERROR {shown} — {e}");
            }
            None => {
                passed += r.passed.len();
                if !quiet {
                    println!("ok   {shown} ({} passed)", r.passed.len());
                }
            }
        }
    }

    println!(
        "\n{} passed, {} failed, {} error{} across {} file{}",
        passed,
        failed,
        errors,
        if errors == 1 { "" } else { "s" },
        files.len(),
        if files.len() == 1 { "" } else { "s" },
    );
    // A suite with zero passing tests is a failure, not a vacuous success: "no
    // tests found" and "all tests passed" must not look the same to CI.
    if passed == 0 {
        eprintln!(
            "ainl test: no (test ...) form ever passed — the suite is empty or every file errored"
        );
        return ExitCode::FAILURE;
    }
    if failed > 0 || errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// A path relative to the working directory when it is below it, so a report
/// reads `tests/foo.ainl` rather than a 90-character absolute path.
fn display_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
    }
    path.display().to_string()
}

/// The `.ainl` files under `path`, sorted.
///
/// Sorted because a suite's report must be identical on every machine: a
/// directory read in inode order is not reproducible, and a CI log that
/// reorders itself between runs is a log nobody can read twice.
fn discover(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    if path.is_dir() {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "ainl") {
                out.push(p);
            }
        }
        out.sort();
        Ok(out)
    } else if path.exists() {
        Ok(vec![path.to_path_buf()])
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ))
    }
}
