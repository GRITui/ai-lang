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
///
/// Every assertion in this file goes through the CLI, not the library, because
/// the subject is the generated *source* and the host's own output. That makes
/// the binary an input to the test rather than a byproduct of it — and a
/// `target/release/ainl` left over from an earlier build silently satisfies
/// every assertion in this file while testing code that is not in the tree.
/// That is not hypothetical: these tests were green against deliberately
/// reverted runtime helpers, because the stale binary still had the fix. So the
/// mtime check below is load-bearing, not hygiene.
fn ainl_bin() -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    // The newest *library* source file. Deliberately not the whole crate: the
    // test file itself is newer than the binary every time it is edited, and
    // requiring `cargo build` to outrun its own test file would make the guard
    // fire on every `cargo test` in a fresh checkout. Only `src/` can change
    // what the binary contains.
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
                        "{} is older than the sources it is supposed to be built from \
                         (bin {bin_mtime:?}, newest source {newest:?}).\n\
                         These tests shell out to this binary, so a stale one makes every \
                         assertion in this file pass regardless of the current code.\n\
                         Rebuild it: `cargo build --release`",
                        p.display()
                    );
                }
            }
            return p;
        }
    }
    panic!("no ainl binary under target/{{release,debug}} — run `cargo build` first");
}

/// The most recent modification time of any `.rs` file under `dir`, or `None`
/// if there are none (which the caller treats as "cannot check").
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
                // `target/` under the workspace root is build output, not source.
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
    // `keys`/`vals`/`has` guard through `_ahash` like `get`/`assoc` do, and
    // that guard is what raises. They were deliberately absent while JS's
    // `_keys`/`_vals` had no guard at all (`Array.from(5, …)` is legal and
    // yields `[]`, so the program *succeeded* and printed `()`) and JS's
    // `_has` raised a host `TypeError` instead of AINL's message. Fixed in
    // t_3f1fdac1; listed here so the class edge they need is covered.
    ("(keys 5)", "keys expects a hash, got int"),
    ("(vals 5)", "vals expects a hash, got int"),
    ("(has 5 \"a\")", "has expects a hash, got int"),
    // And the cross-container shape, which is the same hole reached the other
    // way round: a list or str where a hash is expected, which `Array.from`
    // also accepts silently.
    ("(keys (list 1 2))", "keys expects a hash, got list"),
    ("(has \"ab\" \"a\")", "has expects a hash, got str"),
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
    // `min`/`max` used to be absent here. They raised a host `TypeError`
    // ("min/max expects a number"), not AINL's `_error`, on all three targets —
    // so a `catch` could not intercept them, and asserting a message they did
    // not produce would have failed this gate for a reason outside the card
    // that owned it. Fixed in t_457f643f: the six stdlib math builtins now route
    // through a shared `_anumber` guard that reports through `_error`, so the
    // class edge they need is real and is listed here. The rest of the family
    // (`abs`/`floor`/`sqrt`/`sleep`) is covered in
    // `math_builtin_error_parity.rs`; `min`/`max` are listed here as well
    // because this is the file that proves the class is *present* whenever
    // `_error` is called, and these are the two whose absence from the list was
    // a documented gap.
    ("(min 1 \"s\")", "min expects a number, got str"),
    ("(max 1 \"s\")", "max expects a number, got str"),
    // The rest of the `_anumber` family, added with them. These also raised host
    // errors before, and the arity/range half of each is separate from the type
    // half — `(sqrt -1)` reported through a host `ValueError`/`ArgumentError`,
    // which is the same non-catchable defect on the range path.
    ("(abs \"s\")", "abs expects a number, got str"),
    ("(floor \"s\")", "floor expects a number, got str"),
    ("(sqrt \"s\")", "sqrt expects a number, got str"),
    ("(sqrt -1)", "sqrt expects a non-negative number"),
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
        // The three that reached an unguarded host method on JS.
        "(keys 5)",
        "(vals 5)",
        "(has 5 \"a\")",
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

/// The JS hash builtins must CALL `_ahash`, not merely have it emitted.
///
/// The two tests above are behavioural, and a behavioural test is satisfied by
/// any implementation that produces the right message — including a future
/// refactor that inlines a different check. What is worth pinning here is the
/// thing that actually broke, and it is narrower than "errors on a non-hash":
/// it is that these three were the only AINL helpers that reached a host method
/// which accepts a non-collection, and the reason nobody noticed is that JS's
/// two such methods fail in *opposite* directions.
///
///   * `Array.from(5, f)` is legal and yields `[]` — so `(keys 5)` printed `()`
///     and exited 0. The worst failure shape in the transpiler: the program
///     succeeded, and a caller branching on `(len (keys x))` got 0 here and an
///     error on the other four backends.
///   * `(5).some(f)` raises, but with a host `TypeError` carrying a node stack
///     trace. Not `_AinlError`, so an AINL `catch` does not intercept it and the
///     message differs from the interpreter's.
///
/// One guard fixes both, and it is the guard the sibling builtins already use —
/// so the assertion is that these three are indistinguishable from `get` and
/// `assoc`: same helper, same calling convention, AINL's own name passed in so
/// the message leads with the builtin the user wrote.
#[test]
fn js_hash_builtins_call_the_ahash_guard() {
    for (builtin, who, call) in [
        ("_keys", "keys", "(keys 5)"),
        ("_vals", "vals", "(vals 5)"),
        ("_has", "has", "(has 5 \"a\")"),
    ] {
        let out = generated(&format!("(print {call})\n"), "js")
            .expect("js target always transpiles (no host needed)");
        let body = out
            .lines()
            .find(|l| l.contains(&format!("function {builtin}(")))
            .unwrap_or_else(|| panic!("{builtin} is not emitted at all:\n{out}"));
        assert!(
            body.contains(&format!("_ahash(\"{who}\"")),
            "{builtin} does not call the _ahash guard, so a non-hash reaches a host \
             method that accepts one ({call}):\n{body}"
        );
        // The guard's own name must come from the builtin, not be hardcoded:
        // `keys` reports `keys expects a hash`, `vals` reports `vals …`. A
        // shared literal would make all three report whichever name won.
        assert!(
            !body.contains("_ahash(\"get\"") && !body.contains("_ahash(\"assoc\""),
            "{builtin} passes another builtin's name to _ahash, so it reports the \
             wrong message:\n{body}"
        );
    }
}

/// The cross-container shape, which is the same hole reached from the other
/// side: a list or a str where a hash is expected. `Array.from` accepts both
/// silently — `Array.from("ab", p => p[0])` is `["a", "b"]`, no error at all —
/// so these exit 0 with a plausible-looking answer on JS and raise on the other
/// four. Included because they are one missing call away from regressing the
/// same way, and because a guard that only rejected ints would not catch it.
#[test]
fn js_hash_builtins_reject_a_list_and_a_str_as_well_as_an_int() {
    for (call, want) in [
        ("(keys (list 1 2))", "keys expects a hash, got list"),
        ("(vals \"ab\")", "vals expects a hash, got str"),
        ("(has (list 1 2) \"a\")", "has expects a hash, got list"),
    ] {
        let Some((code, stdout, stderr)) = run(&format!("(print {call})\n"), "js") else {
            eprintln!("skipping js: node unavailable");
            continue;
        };
        assert_ne!(
            code,
            0,
            "js: `{call}` exited 0 and printed {:?} — the interpreter raises here",
            stdout.trim_end()
        );
        assert!(
            stderr.contains(want),
            "js: `{call}` did not report {want:?}:\n{stderr}"
        );
        // Not a host TypeError either: that escapes `catch` and prints a stack.
        assert!(
            !stderr.contains("TypeError"),
            "js: `{call}` raised a host TypeError instead of AINL's message:\n{stderr}"
        );
    }
}
