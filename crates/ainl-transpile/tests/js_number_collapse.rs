//! Pins the JS target's int/float behaviour **after** the tagged-number fix
//! (numeric-model card 1/6). The old file pinned a *known, documented*
//! divergence — JS has one number type (`number`), so it could not tell `3.0`
//! from `3` — and asserted that JS silently lost the float marker on display,
//! accepted a float index, and named a whole float an "int" in error text.
//!
//! ## What changed
//!
//! The JS emitter now carries an explicit int/float **tag** in the value (a
//! `_Float` wrapper; ints stay raw `Number`s). The tag drives **display and
//! type-checks only**, not `=`. As a result the three divergent cases now
//! agree with the other four backends, and this file re-pins them to the
//! *fixed* behaviour so the parity cannot drift back. This is a deliberate,
//! documented re-pin of the collapse: the tests that asserted the divergence
//! now assert its absence. See `docs/NUMERIC_MODEL.md` for the model and the
//! one accepted divergence that remains (JS does not reproduce i64-overflow
//! promotion).
//!
//! The two tests that were *not* divergent (`a_non_whole_float_agrees_everywhere`,
//! `whole_ints_must_keep_printing_as_bare_ints`) are unchanged — they held
//! before and hold now.

use std::path::PathBuf;
use std::process::Command;

/// The four backends that always had an int/float distinction, plus JS, which
/// now has one too. All five agree on every case in this file.
const ALL: [&str; 5] = ["interp", "aot", "python", "ruby", "js"];

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
fn whole_floats_keep_their_marker_on_all_backends() {
    // The old divergence, now fixed: a whole-valued float keeps its `.0` on
    // every backend, including JS.
    //
    // Each case is a whole-valued float written a different way — a literal, a
    // division, and arithmetic results that promote to float. Before the tag
    // they all lost the `.0` on JS and kept it on the other four; now all five
    // agree.
    let cases = [
        ("(print 3.0)", "3.0"),
        ("(print (/ 4 2))", "2.0"),
        ("(print (+ 1.5 1.5))", "3.0"),
        ("(print (- 5.0 2))", "3.0"),
        ("(print (sqrt 4.0))", "2.0"),
    ];
    for (prog, expected) in cases {
        for target in ALL {
            let Some(out) = run(prog, target) else {
                continue;
            };
            assert_eq!(out, expected, "{target} disagrees on `{prog}`");
        }
    }
}

#[test]
fn a_non_whole_float_agrees_everywhere() {
    // The half that was *never* divergent, and the reason the old divergence
    // was easy to describe badly: `0.5` survives JS's single number type
    // intact, because a non-whole value has no int/float boundary to cross.
    // It still agrees everywhere.
    for prog in ["(print 0.5)", "(print (/ 1 2.0))", "(print (- 1.0 0.5))"] {
        for target in ALL {
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
    // correct behaviour and is shared with all five backends, so a change here
    // must not take it away. The tag must not leak into ints.
    let prog = "(print (+ 1 2))";
    for target in ALL {
        let Some(out) = run(prog, target) else {
            continue;
        };
        assert_eq!(out, "3", "{target} must print a whole int as `3`");
    }
}

#[test]
fn a_float_index_is_rejected_on_all_backends() {
    // The old file pinned the sharper consequence of the collapse:
    // `(substring "abc" 0 1.0)` was a type error on four backends but JS
    // silently accepted the float and returned `"a"` — a program that aborts
    // everywhere else quietly producing output is the worst shape this class
    // of bug can take.
    //
    // Now that JS carries the float tag, it rejects the index too, with the
    // same message as the other four. This pins the fix: every backend errors,
    // and names the offending value a float.
    let prog = r#"(print (substring "abc" 0 1.0))"#;

    for target in ALL {
        let Some((ok, _stdout, stderr)) = run_raw(prog, target) else {
            continue;
        };
        assert!(!ok, "{target} accepted a float index: AINL rejects it");
        assert!(
            stderr.contains("expects an int") && stderr.contains("got float"),
            "{target} rejected it with the wrong message:\n{stderr}"
        );
    }
}

#[test]
fn a_whole_float_is_named_a_float_in_an_error_message() {
    // The old file pinned that the collapse reached error *text*: `_ainl_tname`
    // picked "int" vs "float" with `Number.isInteger`, so a whole float like
    // `1.0` was reported as an int on JS only. AINL rejects a non-str object
    // key in `json-serialize` and names the key's type, which makes this a
    // one-line program that disagreed with the other four backends on stderr.
    //
    // Now that JS wraps a whole float in `_Float`, `_ainl_tname` sees the tag
    // and names it a float on every backend. This pins the fix: all five agree
    // on `got float`.
    let prog = r#"(print (json-serialize (hash 1.0 "v")))"#;

    for target in ALL {
        let Some((ok, _, stderr)) = run_raw(prog, target) else {
            continue;
        };
        assert!(!ok, "{target} accepted a float object key");
        assert!(
            stderr.contains("got float"),
            "{target} must name 1.0 a float:\n{stderr}"
        );
    }
}
