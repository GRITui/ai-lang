//! The stdlib math builtins must report a bad operand through AINL's own
//! `_error`, not a host `TypeError`.
//!
//! ## The bug
//!
//! `abs`, `floor`, `sqrt`, `sleep`, `min` and `max` each validated their numeric
//! operand with an inline `throw new TypeError(...)` / `raise TypeError, ...`.
//! Two things were wrong with that, and the second is the one that matters.
//!
//! 1. **Wording.** The interpreter reports `min expects a number, got str`. The
//!    host raised `min expects a number` — no type name — and Python collapsed
//!    `min`/`max` into a single literal, `min/max expects a number`. Three
//!    different messages for one condition across the three targets, and none
//!    of them AINL's.
//!
//! 2. **Not catchable.** A host `TypeError` is not `AinlError`/`_AinlError`, so
//!    it escaped a generated `rescue AinlError` / `except _AinlError` /
//!    `instanceof _AinlError` check entirely: the program died on the error path
//!    with a host traceback where a `catch` should have bound the message. This
//!    is the same reasoning the fs builtins already carry in their comments, and
//!    these six were simply never converted.
//!
//! ## What is asserted, and why behaviourally
//!
//! Each case is checked twice: once uncaught (AINL's message must appear in the
//! host's stderr) and once through a `try` (the `catch` must bind the message and
//! the process must exit 0). The second is the load-bearing half — it fails on
//! the original code on all three targets even where the uncaught wording
//! happened to look close, because a host exception is not what `catch` looks
//! for.
//!
//! The harness is copied from `error_class_parity.rs` rather than shared, for
//! the reason that file documents: these tests need to assert on a *failing*
//! run and on the generated source, which its sibling harness treats as a
//! panic.
//!
//! The `abs`/`floor`/`sqrt`/`sleep` cases are here, not just `min`/`max`: the
//! card that found this scoped the fix to `min`/`max` but noted the family
//! shared the `_isnum` shape, and probing confirmed the whole family raised a
//! host error on all three targets. A fix covering two of six would be a
//! narrower fix than the bug.

use std::path::PathBuf;
use std::process::Command;

const TARGETS: &[&str] = &["python", "js", "ruby"];

/// The AINL error class each target defines, and the host error that proves it
/// is missing — the same pair `error_class_parity.rs` uses.
const ERRCASE: &[(&str, &str, &str)] = &[
    ("python", "class _AinlError", "NameError"),
    ("js", "class _AinlError", "ReferenceError"),
    ("ruby", "class AinlError", "NameError"),
];

/// The `ainl` binary under test. Prefers the release build and falls back to
/// debug, with the same staleness guard `error_class_parity.rs` uses: these
/// tests shell out to the binary, so a stale one satisfies every assertion
/// regardless of what the tree contains.
fn ainl_bin() -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    let crate_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let core_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/ainl-core/src");
    let newest_src = newest_mtime(&crate_src)
        .into_iter()
        .chain(newest_mtime(&core_src))
        .max();
    for profile in ["release", "debug"] {
        let p = root.join(profile).join("ainl");
        if p.is_file() {
            let bin_mtime = p
                .metadata()
                .and_then(|m| m.modified())
                .expect("stat the ainl binary");
            if let Some(newest) = newest_src {
                if bin_mtime < newest {
                    panic!(
                        "{} is older than the sources it is built from. Rebuild it: \
                         `cargo build --release`",
                        p.display()
                    );
                }
            }
            return p;
        }
    }
    panic!("no ainl binary under target/{{release,debug}} — run `cargo build` first");
}

fn newest_mtime(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    newest = Some(match newest {
                        Some(n) if n >= t => n,
                        _ => t,
                    });
                }
            }
        }
    }
    newest
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var("PATH").ok().and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

fn next_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("ainl-matherr-{}-{}", std::process::id(), next_id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Transpiles, runs the host, and returns `(exit code, stdout, stderr)`.
/// `None` when the host runtime is not installed (a skip, not a failure).
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
    let _ = std::fs::remove_dir_all(&dir);
    Some((
        r.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&r.stdout).to_string(),
        String::from_utf8_lossy(&r.stderr).to_string(),
    ))
}

/// Transpiles `prog` and returns the generated source (no host needed).
fn generated(prog: &str, target: &str) -> String {
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
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "transpile to {target} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Every case, with the message the interpreter gives it.
///
/// The interpreter and the AOT C runtime are the two authorities: both already
/// reported these through AINL's own text, and the AINL message is the spec the
/// transpilers are catching up to — not the host's.
///
/// `(min "s")` is here on purpose and is not a duplicate of `(min 1 "s")`. The
/// Python `_minmax` looped over `xs[1:]`, skipping the guard on the seed, so a
/// SINGLE non-numeric argument returned it with exit 0 while every other backend
/// raised. Two-argument and one-argument cases are separate code paths.
const CASES: &[(&str, &str)] = &[
    // The card's repro, and its `max` sibling.
    ("(min 1 \"s\")", "min expects a number, got str"),
    ("(max 1 \"s\")", "max expects a number, got str"),
    // The one-argument shape — the seed-only fold, which is where Python's
    // `xs[1:]` slice lost the guard entirely.
    ("(min \"s\")", "min expects a number, got str"),
    ("(max \"s\")", "max expects a number, got str"),
    // The reverse order, to prove the guard is on every element and not just
    // the first one it happens to reach.
    ("(min \"s\" 1)", "min expects a number, got str"),
    // Zero arguments: an arity error, not a type error, and it too has to be
    // AINL's for a `catch` to bind it.
    ("(min)", "min expects at least 1 argument"),
    ("(max)", "max expects at least 1 argument"),
    // The rest of the family. Same `_isnum` shape, same host-TypeError failure.
    ("(abs \"s\")", "abs expects a number, got str"),
    ("(floor \"s\")", "floor expects a number, got str"),
    ("(sqrt \"s\")", "sqrt expects a number, got str"),
    ("(sleep \"s\")", "sleep expects a number, got str"),
    // Range errors on the same builtins, which also raised host classes
    // (`ValueError` / `ArgumentError`) rather than AINL's error.
    ("(sqrt -1)", "sqrt expects a non-negative number"),
    ("(sleep -1)", "sleep expects a non-negative number"),
    // A bool where a number is expected. Python's bool is a subclass of int, so
    // `isinstance` would accept it and the message would be wrong on the one
    // backend that can express the mistake.
    ("(min true)", "min expects a number, got bool"),
    ("(max false 1)", "max expects a number, got bool"),
];

/// The uncaught case: AINL's message must reach stderr, and the host's own
/// error class must not be what got there.
#[test]
fn a_bad_operand_reports_ains_message_not_a_host_type_error() {
    for (target, class, hosterr) in ERRCASE {
        for (prog, message) in CASES {
            let Some((code, stdout, stderr)) = run(&format!("(print {prog})\n"), target) else {
                eprintln!("skipping {target}: host unavailable");
                continue;
            };
            assert_ne!(
                code,
                0,
                "{target}: `{prog}` exited 0 and printed {:?} — the error path was not reached",
                stdout.trim_end()
            );
            assert!(
                !stderr.contains(hosterr) || !stderr.contains(class),
                "{target}: `{prog}` produced a host {hosterr} instead of AINL's message \
                 (wanted {message:?}):\n{stderr}"
            );
            assert!(
                stderr.contains(message),
                "{target}: `{prog}` did not report {message:?}:\n{stderr}"
            );
            // The specific failure this card is about: a host `TypeError` is not
            // `_AinlError`, so it bypassed `catch`. Naming it here documents the
            // class of the bug even though the message assertion above already
            // fails on the original code.
            assert!(
                !stderr.contains("TypeError"),
                "{target}: `{prog}` raised a host TypeError instead of AINL's message:\n{stderr}"
            );
        }
    }
}

/// The caught case — the one that actually matters.
///
/// A host exception is not what `catch` looks for, so before the fix this
/// program died with a traceback on all three targets. After the fix the `catch`
/// binds the message and the process exits 0. The assertion is exact equality on
/// stdout, not containment: the interpreter and the AOT runtime produce this
/// byte for byte, and a divergence here is the bug, not decoration.
#[test]
fn a_catch_binds_the_math_builtin_error_instead_of_a_host_exception() {
    for (target, class, _) in ERRCASE {
        for (prog, message) in CASES {
            let src = format!("(print (try {prog} (catch (e) (get e \"message\"))))\n");
            let Some((code, stdout, stderr)) = run(&src, target) else {
                eprintln!("skipping {target}: host unavailable");
                continue;
            };
            assert_eq!(
                code, 0,
                "{target}: `(try {prog} …)` aborted instead of catching — a host exception \
                 is not the type `catch` intercepts:\n{stderr}"
            );
            assert_eq!(
                stdout.trim_end(),
                *message,
                "{target}: the caught message for `{prog}` is not AINL's"
            );
            // And the class still has to be present, or the `except`/`rescue`/
            // `instanceof` clause would name something undefined.
            let out = generated(&src, target);
            assert!(
                out.contains(class),
                "{target}: `{prog}` catches but {class} is missing:\n{out}"
            );
        }
    }
}

/// The structural half: the math builtins must not raise a host error at all.
///
/// The two behavioural tests above can both be satisfied by a program that
/// happens to produce the right string. What actually broke is a *call to the
/// host's error mechanism from AINL's own helper*, so the assertion worth
/// pinning is that the generated source contains no host error raise at all for
/// these builtins.
///
/// The guard's name is asserted as well, because the bug's second half is silent
/// if a future edit inlines the check and hard-codes one builtin's name: `min`
/// would then report `abs expects a number`, and every behavioural case above
/// would fail — but for a reason this test names directly.
#[test]
fn the_math_builtins_route_through_the_shared_ainl_guard() {
    // A program that reaches the guard, per builtin. `min`/`max` are included:
    // the fold is the helper that lost `_error` on all three targets, and the
    // two share one implementation, so the guard has to be on the fold's path.
    let cases = [
        ("(abs \"s\")", "_abs"),
        ("(floor \"s\")", "_floor"),
        ("(sqrt \"s\")", "_sqrt"),
        ("(min 1 \"s\")", "_minmax"),
        ("(max 1 \"s\")", "_minmax"),
    ];
    for target in TARGETS {
        for (prog, helper) in cases {
            let out = generated(&format!("(print {prog})\n"), target);
            // The shared guard must be present and called.
            assert!(
                out.contains("_anumber(") && out.contains("function _anumber")
                    || out.contains("def _anumber"),
                "{target}: `{prog}` does not route through the shared _anumber guard:\n{out}"
            );
            // And no host error raise may remain in it. A `throw new TypeError`
            // or `raise TypeError, …` in one of these helpers is the original bug
            // in its original form, whatever the runtime happens to print.
            for host_raise in [
                "throw new TypeError",
                "throw new Error",
                "throw new RangeError",
                "raise TypeError",
                "raise ArgumentError",
                "raise ValueError",
            ] {
                assert!(
                    !out.contains(host_raise),
                    "{target}: `{prog}` still raises a host error ({host_raise}) instead of \
                     AINL's _error:\n{out}"
                );
            }
            // The helper that owns the failure must be the one emitting the call.
            assert!(
                out.contains(&format!("function {helper}("))
                    || out.contains(&format!("def {helper}(")),
                "{target}: `{prog}` did not emit {helper}:\n{out}"
            );
        }
    }
}

/// `min` and `max` share one fold helper, so the shared guard has to be called
/// with the *caller's* name. A hard-coded literal — the shape the original
/// Python code had (`'min/max expects a number'`, which named both builtins in
/// one message) — would make `max` report `min`.
///
/// This is a separate test from the one above because the failure is invisible
/// there: the guard is present and called, the source carries no host error, and
/// only the message text is wrong.
#[test]
fn min_and_max_pass_their_own_name_to_the_shared_guard() {
    for target in TARGETS {
        let min = generated("(print (min 1 \"s\"))\n", target);
        let max = generated("(print (max 1 \"s\"))\n", target);
        // `_min`/`_max` are the only callers that know which builtin it was.
        for (src, who, other) in [(&min, "min", "max"), (&max, "max", "min")] {
            assert!(
                src.contains(&format!("\"{who}\""))
                    || src.contains(&format!("'{who}'"))
                    || src.contains(&format!("'{who}',")),
                "{target}: `{who}` does not pass its own name down, so it cannot report \
                 itself correctly:\n{src}"
            );
            assert!(
                !src.contains(&format!("\"{other}\"")) && !src.contains(&format!("'{other}'")),
                "{target}: `{who}`'s generated source mentions `{other}` — the two share one \
                 fold helper, so a hard-coded name reports the wrong builtin:\n{src}"
            );
        }
    }
}
