//! Pins the JS target's int/float collapse — a *known, documented* divergence
//! that the rest of the suite is written to avoid rather than tolerate.
//!
//! ## What is measured here
//!
//! AINL has two number types on the interpreter, the AOT binary, and the Python
//! and Ruby targets. JavaScript has one (`number`), so `3.0` and `3` are the
//! same value with the same `String()` rendering, and the transpiler emits
//! `_print(3.0)` for `(print 3.0)` — the `.0` is in the generated *source* and is
//! erased when JS parses it. So on this target a whole float prints without its
//! marker, while the other four print `3.0`.
//!
//! This is pinned rather than fixed because the fix is a numeric-model
//! decision, not a display tweak: recovering the marker at print time requires
//! knowing which expressions are floats, and JS offers nothing at runtime that
//! distinguishes them. See docs/NUMERIC_MODEL.md.
//!
//! ## Why a pin rather than a fix
//!
//! The obvious one-line fix — "append `.0` to any integer-valued number" — is
//! wrong, and `whole_ints_must_keep_printing_as_bare_ints` is the assertion that
//! says so: it would render `(print 3)` as `3.0` on this target only, breaking
//! agreement on the far more common int case to repair the rare float one.
//!
//! `a_float_index_is_still_rejected_where_the_backend_can_see_it` pins the
//! sharper consequence: the collapse is not confined to display, and a program
//! that errors on four backends silently succeeds on this one.

use std::path::PathBuf;
use std::process::Command;

/// The two backends that have an int/float distinction and therefore agree
/// with each other, plus JS which does not.
const TYPED: [&str; 4] = ["interp", "aot", "python", "ruby"];

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

/// Runs `prog` on one backend and returns its stdout.
///
/// `None` means the backend could not be *invoked* (no `node`, no `ruby`), which
/// is a skip. A program that runs and reports an error is a real result, not a
/// skip — `expect_ok` callers must not tolerate a failing program, and the
/// error-parity test below uses [`run_raw`] instead.
///
/// Each call gets a unique working directory: cargo runs tests in this binary
/// concurrently, and a shared `p.ainl` would have them reading each other's
/// program, which looks exactly like a backend bug.
fn run(prog: &str, target: &str) -> Option<String> {
    let (status, stdout, stderr) = run_raw(prog, target)?;
    if !status {
        panic!("{target} exited non-zero on:\n{prog}\nstdout: {stdout}\nstderr: {stderr}");
    }
    Some(stdout.trim_end().to_string())
}

/// As [`run`], but reports success rather than panicking, so a test can compare
/// error *behaviour* and not just output.
fn run_raw(prog: &str, target: &str) -> Option<(bool, String, String)> {
    let ainl = ainl_bin();
    let dir = std::env::temp_dir().join(format!("ainl-collapse-{target}-{}", next_id()));
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("p.ainl");
    std::fs::write(&src, prog).ok()?;

    let out = match target {
        "interp" => Command::new(ainl).arg("run").arg(&src).output().ok()?,
        "aot" => {
            let bin = dir.join("p.bin");
            let c = Command::new(ainl)
                .arg("compile")
                .arg(&src)
                .arg("-o")
                .arg(&bin)
                .output()
                .ok()?;
            if !c.status.success() {
                panic!(
                    "ainl compile failed:\n{}",
                    String::from_utf8_lossy(&c.stderr)
                );
            }
            Command::new(&bin).output().ok()?
        }
        _ => {
            let host = match target {
                "python" => "python3",
                "js" => "node",
                _ => "ruby",
            };
            which(host)?;
            let out = match target {
                "python" => dir.join("out.py"),
                "js" => dir.join("out.js"),
                _ => dir.join("out.rb"),
            };
            let t = Command::new(&ainl)
                .arg("transpile")
                .arg(&src)
                .arg("--to")
                .arg(target)
                .output()
                .ok()?;
            if !t.status.success() {
                panic!(
                    "transpile to {target} failed:\n{}",
                    String::from_utf8_lossy(&t.stderr)
                );
            }
            std::fs::write(&out, &t.stdout).ok()?;
            Command::new(host).arg(&out).output().ok()?
        }
    };

    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var("PATH").ok().and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn whole_floats_print_without_their_marker_on_js_only() {
    // The divergence the card asked about, pinned exactly.
    //
    // Each case is a whole-valued float written a different way — a literal, a
    // division, and an arithmetic result that promotes to float. They all lose
    // the `.0` on JS and all keep it on the other four.
    let cases = [
        ("(print 3.0)", "3.0"),
        ("(print (/ 4 2))", "2.0"),
        ("(print (+ 1.5 1.5))", "3.0"),
        ("(print (- 5.0 2))", "3.0"),
        ("(print (sqrt 4.0))", "2.0"),
    ];
    for (prog, expected) in cases {
        for target in TYPED {
            let Some(out) = run(prog, target) else {
                continue;
            };
            assert_eq!(out, expected, "{target} disagrees on `{prog}`");
        }
        if let Some(js) = run(prog, "js") {
            let without = expected.strip_suffix(".0").unwrap_or(expected);
            assert_eq!(
                js, without,
                "JS has one number type, so a whole float loses its marker; \
                 this pins that. If this now passes with the `.0`, the collapse \
                 was fixed and this test should be inverted, not deleted."
            );
        }
    }
}

#[test]
fn a_non_whole_float_agrees_everywhere() {
    // The half that is *not* divergent, and the reason the divergence is easy
    // to describe badly: `0.5` survives JS's single number type intact,
    // because a non-whole value has no int/float boundary to cross.
    for prog in ["(print 0.5)", "(print (/ 1 2.0))", "(print (- 1.0 0.5))"] {
        for target in TYPED.iter().copied().chain(["js"]) {
            let Some(out) = run(prog, target) else {
                continue;
            };
            assert!(
                out.contains('.') || out.contains('e'),
                "{target} lost the fractional part of `{prog}`: {out}"
            );
        }
    }
}

#[test]
fn whole_ints_must_keep_printing_as_bare_ints() {
    // Why the display-only fix is the wrong fix, asserted rather than argued.
    //
    // "Append `.0` to any integer-valued number" would make `(print 3)` print
    // `3.0` on JS and only on JS — trading a divergence on every integer in
    // every program for one on every whole float. The int rendering is the
    // correct current behaviour and is shared with all four other backends, so
    // a change here must not take it away.
    let prog = "(print (+ 1 2))";
    for target in TYPED.iter().chain(&["js"]) {
        let Some(out) = run(prog, target) else {
            continue;
        };
        assert_eq!(out, "3", "{target} must print a whole int as `3`");
    }
}

#[test]
fn a_float_index_is_still_rejected_where_the_backend_can_see_it() {
    // The collapse is not only a display artefact, and this is the sharper
    // consequence: `(substring "abc" 0 1.0)` is a type error on every backend
    // that can tell 1.0 from 1 — but JS cannot, so it accepts the float and
    // returns a string. A program that aborts everywhere else quietly produces
    // output here, which is the worst shape this class of bug can take.
    let prog = r#"(print (substring "abc" 0 1.0))"#;

    for target in TYPED {
        let Some((ok, _stdout, stderr)) = run_raw(prog, target) else {
            continue;
        };
        assert!(!ok, "{target} accepted a float index: AINL rejects it");
        assert!(
            stderr.contains("expects an int") && stderr.contains("got float"),
            "{target} rejected it with the wrong message:\n{stderr}"
        );
    }

    if let Some((ok, stdout, _)) = run_raw(prog, "js") {
        assert!(
            ok,
            "JS still accepts a float index. If that is now fixed, this test \
             should be inverted to assert every backend rejects it — that would \
             be the real parity fix landing."
        );
        assert_eq!(
            stdout.trim(),
            "a",
            "JS returns a slice rather than erroring; this pins that"
        );
    }
}

#[test]
fn a_whole_float_is_named_an_int_in_an_error_message() {
    // The collapse reaches error *text*, not only printed values.
    //
    // `_ainl_tname` picks "int" vs "float" with `Number.isInteger`, so `1.0` is
    // reported as an int on this target. AINL rejects a non-str object key in
    // `json-serialize` and names the key's type, which makes this a one-line
    // program that disagrees with the other four backends on stderr.
    //
    // A non-whole float is unaffected — `1.5` is not an integer by this test
    // either — so the divergence is confined to whole floats, like the display
    // one above.
    let prog = r#"(print (json-serialize (hash 1.0 "v")))"#;

    for target in TYPED {
        let Some((ok, _, stderr)) = run_raw(prog, target) else {
            continue;
        };
        assert!(!ok, "{target} accepted a float object key");
        assert!(
            stderr.contains("got float"),
            "{target} must name 1.0 a float:\n{stderr}"
        );
    }

    if let Some((ok, _, stderr)) = run_raw(prog, "js") {
        assert!(!ok);
        assert!(
            stderr.contains("got int"),
            "JS calls a whole float an int; this pins that. If it now says \
             'got float', the type naming was fixed and this test should be \
             rewritten to assert all five agree."
        );
    }
}
