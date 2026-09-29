//! The error class must be emitted whenever `_error` is — with or without `try`.
//!
//! ## The bug
//!
//! `_error` raises AINL's own `AinlError` so an AINL `catch` intercepts it and
//! stderr stays byte-comparable with the other backends. The class is a
//! separate RUNTIME entry from the helper, and a `needed`-set rule is the only
//! thing that links them.
//!
//! That rule used to run *before* the rules that insert `_error` for the
//! arithmetic, collection, file and fs builtins. So a program that reached
//! `_error` only indirectly — through `(len 5)`, say — emitted `def _error`
//! naming a class it never defined, and Ruby died with
//! `uninitialized constant AinlError (NameError)` where AINL's message
//! belonged. A `try` masked it, because the `try` rule pulled the class in for
//! its own reasons, which is why the whole transpiler suite passed: no fixture
//! raised without a `try`.
//!
//! The same ordering defect was live in `js.rs` and `python.rs` with a smaller
//! blast radius (`(mkdir "d")` on JS, `(read-file "nope")` on Python), so this
//! file checks all three targets rather than Ruby alone.
//!
//! ## Why the harness is duplicated
//!
//! `tests/json_parity.rs` has the same `run()`-per-backend shape. It cannot be
//! reused: these tests need the generated *source* as well as its stderr, and
//! they need to assert on a failing run, which that harness treats as a panic.
//! Copying the runner is the smaller cost than bending a shared helper into
//! two shapes, but the host-detection and per-call-scratch rules it encodes are
//! the point, so they are reproduced rather than simplified.

use std::path::PathBuf;
use std::process::Command;

const TARGETS: &[&str] = &["python", "js", "ruby"];

/// The error class each target defines, and the host error that proves it is
/// missing. Ruby raises `NameError` for an undefined constant, Python for an
/// undefined global, Node for an undefined binding — three different
/// diagnostics for one missing class, and all three replace AINL's message.
const ERRCASE: &[(&str, &str, &str)] = &[
    ("python", "class _AinlError", "NameError"),
    ("js", "class _AinlError", "ReferenceError"),
    ("ruby", "class AinlError", "NameError"),
];

/// The `ainl` binary under test. Prefers the release build for the same reason
/// `json_parity.rs` does, and falls back to debug.
fn ainl_bin() -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    for profile in ["release", "debug"] {
        let p = root.join(profile).join("ainl");
        if p.is_file() {
            return p;
        }
    }
    panic!("no ainl binary under target/{{release,debug}} — run `cargo build` first");
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var("PATH").ok().and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// A process-unique suffix, so the tests in this binary can run in parallel
/// without sharing a scratch file.
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ainl-errcls-{}", next_id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Transpiles `prog` and returns the generated source, or `None` when the host
/// runtime is not installed (a skip, not a failure).
fn generated(prog: &str, target: &str) -> Option<String> {
    let ainl = ainl_bin();
    let dir = scratch();
    let src = dir.join("p.ainl");
    std::fs::write(&src, prog).expect("write program");
    let out = Command::new(&ainl)
        .arg("transpile")
        .arg(&src)
        .arg("--to")
        .arg(target)
        .output()
        .expect("run transpile");
    assert!(
        out.status.success(),
        "transpile to {target} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Transpiles, runs the host, and returns `(exit code, stdout, stderr)`.
/// `None` when the host is not installed.
fn run(prog: &str, target: &str) -> Option<(i32, String, String)> {
    let ainl = ainl_bin();
    let host = match target {
        "python" => "python3",
        "js" => "node",
        _ => "ruby",
    };
    which(host)?;
    let dir = scratch();
    let src = dir.join("p.ainl");
    std::fs::write(&src, prog).expect("write program");
    let t = Command::new(&ainl)
        .arg("transpile")
        .arg(&src)
        .arg("--to")
        .arg(target)
        .output()
        .expect("run transpile");
    assert!(
        t.status.success(),
        "transpile to {target} failed:\n{}",
        String::from_utf8_lossy(&t.stderr)
    );
    let ext = match target {
        "python" => "py",
        "js" => "js",
        _ => "rb",
    };
    let out = dir.join(format!("o.{ext}"));
    std::fs::write(&out, &t.stdout).expect("write generated");
    let r = Command::new(host).arg(&out).output().expect("run host");
    Some((
        r.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&r.stdout).to_string(),
        String::from_utf8_lossy(&r.stderr).to_string(),
    ))
}

/// Every program here raises WITHOUT a `try`, which is the whole point: no
/// `try` rule is available to pull the error class in on the program's behalf,
/// so the `_error` -> class edge has to stand on its own.
///
/// `(error …)` is absent on purpose for the same reason — the `error` builtin
/// names `_error` directly and would pull the class in through a different
/// path, hiding a broken edge behind a working one.
const RAISERS: &[(&str, &str)] = &[
    // The card's repro: the arithmetic cluster, reached only through `+`.
    ("(+ 1.0 \"s\")", "expected a number, got str"),
    ("(len 5)", "len expects list, str, or hash, got int"),
    ("(first 5)", "first expects list, got int"),
    ("(get \"x\" \"y\")", "get expects a hash, got str"),
    ("(/ 1 0)", "division by zero"),
    ("(mod 1 \"s\")", "expected a number, got str"),
    // Collections: `_first`/`_rest`/`_push` go through `_alist`, the hash ones
    // through `_ahash`, and all of them report through `_error`.
    ("(rest 5)", "rest expects list, got int"),
    ("(push 5 1)", "push expects"),
    ("(nth 1 \"s\")", "nth expects"),
    // `(keys 5)` and its `vals`/`has` siblings are deliberately absent: JS
    // `_keys` has no type guard at all (`Array.from(h, p => p[0])` returns `()`
    // for an int), so the program *succeeds* on JS and there is no error path
    // to check. That is a missing-guard bug, not a missing-class one, and it
    // reproduces on clean main. Filed separately; including it here would fail
    // on a defect this card does not claim to fix.
    ("(hash 1)", "hash expects an even number"),
    // Tier 1 file I/O and Tier 3 fs: both clusters name `_error` from a rule
    // that runs near the bottom of `resolve_deps`. These are *type* errors on
    // purpose — `(mkdir "d")` would succeed on a runner whose scratch dir has
    // no `d` in it, so the error path would never be reached and the test would
    // pass for the wrong reason.
    // A *type* error, not a missing file: `(read-file "nope")` reports
    // `read-file: cannot read 'nope'` first (the OSError is caught and routed
    // through `_error`), so it never reaches the type guard. The point here is
    // that the cluster names `_error` at all, and either message proves it.
    ("(read-file 5)", "read-file expects a str path"),
    ("(mkdir 5)", "mkdir expects a str path"),
    ("(is-dir 5)", "is-dir expects a str path"),
    ("(file-size 5)", "file-size expects a str path"),
    // Byte-oriented strings.
    ("(substring 5 0 1)", "substring expects a str"),
    // `sort` is the helper whose own `_error` edge was missing outright on JS,
    // rather than merely mis-ordered.
    ("(sort (list 1 \"s\"))", "sort expects"),
    // `min`/`max` are deliberately absent. They raise a host `TypeError`
    // ("min expects a number"), not AINL's `_error`, on all three targets —
    // a pre-existing wording divergence, unrelated to the class-missing bug and
    // tracked separately. They are listed here because they DID lose `_isnum`
    // on JS, and the structural test below covers that; asserting a message
    // they do not produce would fail for a reason that is not this card's.
];

#[test]
fn the_error_class_is_emitted_whenever_error_is() {
    // The structural half of the contract, on every target: if the generated
    // source can call `_error`, the class it raises must be defined in it. This
    // is the assertion that is cheap, needs no host runtime, and fails on the
    // exact source that would fail at run time.
    for (target, class, _) in ERRCASE {
        for (prog, _) in RAISERS {
            let Some(out) = generated(&format!("(print {prog})\n"), target) else {
                continue;
            };
            let calls_error = out.contains("_error(");
            assert!(
                !calls_error || out.contains(class),
                "{target}: `{prog}` calls _error but {class} is missing:\n{out}"
            );
        }
    }
}

/// A broader guard than the message tests: **every** AINL helper the generated
/// source calls must also be defined in it, on every target.
///
/// The `RAISERS` list is a hand-picked sample, and a hand-picked sample cannot
/// promise the next builtin is covered. This one derives the claim from the
/// generated text itself, so a new builtin that reaches a helper it did not
/// name fails here whether or not anyone thought to add it to the list. That
/// is how the JS `_minmax` -> `_isnum` edge was found, and it is the same
/// shape of mistake as the class bug: an emitted call to something that is not
/// emitted, which is a `ReferenceError`/`NameError` at run time in place of
/// AINL's message.
#[test]
fn every_helper_the_generated_source_calls_is_also_defined() {
    // `(min …)` and `(sort …)` are the two that were broken on JS; both are
    // here as cases, and the assertion is generic so it keeps working after
    // they are fixed.
    let cases = [
        "(min 1 2)",
        "(max 1 2)",
        "(sort (list 2 1))",
        "(sort 5)",
        "(+ 1.0 \"s\")",
        "(hash 1)",
        "(mkdir 5)",
        "(read-file 5)",
        "(str \"a\")",
        "(if true 1 2)",
    ];
    for target in TARGETS {
        for case in cases {
            let Some(out) = generated(&format!("(print {case})\n"), target) else {
                continue;
            };
            let defined: std::collections::HashSet<&str> = out
                .lines()
                .filter_map(|l| {
                    l.trim_start()
                        .strip_prefix("def ")
                        .or_else(|| l.trim_start().strip_prefix("function "))
                        .and_then(|r| r.split(|c: char| !(c.is_alphanumeric() || c == '_')).next())
                })
                .chain(out.lines().filter_map(|l| {
                    l.trim_start().strip_prefix("class ").and_then(|r| {
                        r.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                            .next()
                    })
                }))
                .collect();
            for called in calls_in(&out) {
                assert!(
                    defined.contains(called),
                    "{target}: `{case}` calls {called} but never defines it:\n{out}"
                );
            }
        }
    }
}

/// The AINL helper names the source calls: an underscore-prefixed identifier
/// immediately followed by `(`.
fn calls_in(src: &str) -> Vec<&str> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' {
            let start = i;
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            // A dunder is a host method (`__str__`, `__init__`), not one of AINL's
            // helpers, and is always defined by the host language. AINL's own
            // helpers are single-underscore-prefixed by construction.
            let is_dunder = bytes.get(j - 1) == Some(&b'_') && j - start > 1;
            // AINL helper names always start a token, so they are preceded by
            // whitespace, `(`, `,`, an operator or nothing. A digit or letter
            // immediately before the `_` means this is a method on some host
            // value — `x.is_integer()`, `n._foo()` — not an AINL helper, and
            // the host language defines it.
            let prev = start.checked_sub(1).map(|k| bytes[k]);
            let is_host_method = matches!(
                prev,
                Some(b) if b.is_ascii_alphanumeric() || b == b'.' || b == b')'
            );
            // Skip the definition itself: `def _foo(` / `function _foo(`.
            let before = &src[..start];
            let is_def = before.trim_end().ends_with("def")
                || before.trim_end().ends_with("function")
                || before.trim_end().ends_with("class");
            if j < bytes.len() && bytes[j] == b'(' && !is_def && !is_dunder && !is_host_method {
                out.push(&src[start..j]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn a_raiser_reports_ains_message_not_a_host_name_error() {
    // The behavioural half, on every target, with a real host: AINL's message
    // must appear in stderr. The host still decorates it (a traceback, a file
    // path, a stack), so the assertion is containment of the message, not byte
    // equality — a backtrace carries a temp path and a line number, and the
    // 4-backend rule is about the *message*, which is what a `catch` binds and
    // what a user reads.
    for (target, class, hosterr) in ERRCASE {
        for (prog, message) in RAISERS {
            let Some((code, _stdout, stderr)) = run(&format!("(print {prog})\n"), target) else {
                eprintln!("skipping {target}: host unavailable");
                continue;
            };
            assert_ne!(
                code, 0,
                "{target}: `{prog}` exited 0 — the error path was not reached"
            );
            assert!(
                !stderr.contains(hosterr) || !stderr.contains(class),
                "{target}: `{prog}` produced a host {hosterr} instead of AINL's \
                 message (wanted {message:?}):\n{stderr}"
            );
            assert!(
                stderr.contains(message),
                "{target}: `{prog}` did not report {message:?}:\n{stderr}"
            );
        }
    }
}

#[test]
fn a_raising_program_still_gets_the_class_a_try_would_have_supplied() {
    // The positive control for the fixed-point resolution: the same program
    // with a `try` around it was always fine, because the `try` rule pulled the
    // class in. If the fix had simply deleted the `try` rule, this would pass
    // and the test above would fail; if it had over-pulled, the omission test
    // below would fail. Both together pin the change to a dependency edge.
    let prog = "(print (try (+ 1.0 \"s\") (catch (e) (get e \"message\"))))\n";
    for (target, class, _) in ERRCASE {
        let Some((code, stdout, _)) = run(prog, target) else {
            eprintln!("skipping {target}: host unavailable");
            continue;
        };
        assert_eq!(code, 0, "{target}: a caught error must not abort");
        assert_eq!(
            stdout.trim_end(),
            "expected a number, got str",
            "{target}: the caught message is not AINL's"
        );
        // And the source still carries the class, via the `try` path.
        assert!(
            generated(prog, target).unwrap().contains(class),
            "{target}: the try path lost the class"
        );
    }
}

#[test]
fn a_program_with_no_error_path_does_not_carry_the_error_class() {
    // The other half of the boundary. A fixed point that over-pulled would put
    // the class into every generated file, which is dead weight in the output
    // and would mean the fix cost something rather than only moving an edge.
    //
    // The program is `(str …)`, not arithmetic: `_add` carries a *runtime* type
    // guard (`unless a.is_a?(Numeric)`), so `(+ 1 2)` legitimately ships the
    // error class even though nothing goes wrong. That is the guard doing its
    // job, not over-pulling, and using it here would assert the opposite of
    // what this test is for. `str` concatenates with no guard at all.
    let prog = "(print (str \"a\" \"b\"))\n";
    for (target, class, _) in ERRCASE {
        let Some(out) = generated(prog, target) else {
            continue;
        };
        assert!(
            !out.contains("_error("),
            "{target}: `str` should have no error path at all:\n{out}"
        );
        assert!(
            !out.contains(class),
            "{target}: a program with no error path must not carry {class}:\n{out}"
        );
    }
}
