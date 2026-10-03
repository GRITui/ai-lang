//! Byte-identical `json-parse` / `json-serialize` across every execution path.
//!
//! The contract is not "each backend handles some JSON". It is that the same
//! AINL program produces the same **bytes** on the interpreter, the AOT binary,
//! and all three transpiler targets — the same claim `scripts/check-aot.sh` and
//! `scripts/check-transpile.sh` make for the rest of the stdlib. Five
//! hand-written implementations make that claim genuinely at risk, so it is
//! checked rather than asserted.
//!
//! `crates/ainl-core/src/json_value.rs` is the normative specification. Every
//! behaviour here is also a unit test there, but those run only the reference
//! implementation; these run the *ports*, which is where divergence actually
//! appeared while writing them.
//!
//! ## Why the corpus avoids whole integers
//!
//! The portable corpus is written so that **no line it prints is a bare
//! integer**, and `the_portable_corpus_contains_no_line_js_cannot_reproduce`
//! enforces that guard. That guard was originally load-bearing: JS had one
//! number type, so an AINL int arrived as a `Number` and `json-serialize`
//! gave it the `.0` that marks a float — the int/float collapse
//! docs/NUMERIC_MODEL.md records for `print` and for integer overflow.
//!
//! The tagged-number fix (numeric-model card 1/6) closed the JS collapse: JS
//! now wraps a float literal in `_Float` and leaves an int a raw `Number`, so
//! `json-serialize 1` → `1` and `json-serialize 1.0` → `1.0` on every backend.
//! The guard is kept as a conservative invariant (it still passes), and the
//! int/float round-trip itself is pinned in
//! `js_now_preserves_the_int_float_distinction_in_json`.

use std::path::PathBuf;
use std::process::Command;

/// A program whose stdout is byte-identical on all five backends.
///
/// Not a single printed line is a bare integer, so JS can be compared with no
/// carve-out: `the_portable_corpus_contains_no_line_js_cannot_reproduce`
/// enforces that, and if someone adds an integer the guard fails first.
const PORTABLE: &str = r#"
(do
  (print (json-serialize (list 1.5 2.75 3.75 true false nil "s")))
  (print (json-serialize (list 1.5 2.5 "s" true nil)))
  (print (json-serialize (hash "b" (list 1.5 (hash "z" (list 2.25))) "a" "x")))
  (print (json-serialize (/ 1.0 3)))
  (print (json-serialize 0.1))
  (print (json-serialize (+ 0.1 0.2)))
  (print (json-serialize 1e21))
  (print (json-serialize 1e-7))
  (print (json-serialize 1.25e17))
  (print (json-parse "\"q\\\"b\\\\s\\nn\\t\\u00e9\\ud83d\\ude00\""))
  (print (json-serialize (json-parse "\"\\u00e9\\ud83d\\ude00\\u0007\"")))
  (print (json-parse "  {\"a\"  :  [ ]  ,  \"b\" : { }  }  "))
  (print (json-serialize (hash "a" (list) "b" (hash))))
  (print (json-parse "-0.5"))
  (print (json-parse "1.5e-3"))
  (print (json-serialize (json-parse "{\"k\":[1.5,2.5]}")))
  (print (json-serialize (json-parse "{\"a\":1.5,\"b\":2.5,\"a\":9.5}"))))
"#;

/// One case per line, for tests that want to name the case that broke. Same
/// rule as `PORTABLE`: no bare-integer output, so JS is included everywhere.
const CASES: &[&str] = &[
    // Floats, including the ones that were hardest to get identical.
    r#"(print (json-serialize 0.5))"#,
    r#"(print (json-serialize -0.5))"#,
    r#"(print (json-serialize 2.25))"#,
    r#"(print (json-serialize (/ 1.0 3)))"#,
    r#"(print (json-serialize (+ 0.1 0.2)))"#,
    r#"(print (json-serialize 1e21))"#,
    r#"(print (json-serialize 1e-7))"#,
    r#"(print (json-serialize 1e300))"#,
    r#"(print (json-serialize 1.25e17))"#,
    r#"(print (json-serialize 123456789012345678.0))"#,
    r#"(print (json-serialize 5e-324))"#,
    // Strings: every escape the writer emits. The control characters are
    // reached through `json-parse` because AINL's own lexer has no `\u`
    // escape — `"\u0007"` is a lexer error in an AINL source string, so a test
    // that used it would be testing the wrong rejection.
    r#"(print (json-serialize "\""))"#,
    r#"(print (json-serialize "\\"))"#,
    r#"(print (json-serialize "/"))"#,
    r#"(print (json-serialize "\n\r\t"))"#,
    r#"(print (json-serialize (json-parse "\"\\u0000\\u001f\\u0007\"")))"#,
    r#"(print (json-serialize (json-parse "\"\\u00e9\\u4e2d\\ud83d\\ude00\"")))"#,
    // The escapes only the reader accepts. Each is reached as a *literal*
    // backslash in the AINL source string (`\\b` in AINL source is a backslash
    // followed by 'b'), which the JSON reader then decodes — AINL's own lexer
    // has no `\b` or `\f` escape, so writing those directly is a lexer error.
    r#"(print (json-serialize (json-parse "\"\\b\\f\\/\\\\\"")))"#,
    // Containers.
    r#"(print (json-serialize (list)))"#,
    r#"(print (json-serialize (hash)))"#,
    r#"(print (json-serialize (list (list 1.5) (list 2.5))))"#,
    r#"(print (json-serialize (hash "a" (hash "b" (hash "c" 1.5)))))"#,
    // Key order is insertion order — not sorted, not sorted-then-restored.
    r#"(print (json-serialize (hash "z" 1.5 "a" 2.5 "m" 3.5)))"#,
    r#"(print (json-parse "{\"z\":1.5,\"a\":2.5,\"m\":3.5}"))"#,
    // A duplicate key keeps its first position and takes the last value, the
    // same rule `hash`/`assoc` implement.
    r#"(print (json-serialize (json-parse "{\"a\":1.5,\"b\":2.5,\"a\":9.5}")))"#,
    // Numbers on input: a decimal or exponential literal is a float. The
    // whole-integer cases live in
    // `an_integer_is_still_distinguishable_from_a_whole_float_where_the_type_allows_it`
    // because `(print 1e3)` exposes the int/float distinction, which the
    // tagged-number fix now carries in JS too.
    r#"(print (json-parse "-0.5"))"#,
    r#"(print (json-parse "1.5"))"#,
    r#"(print (json-parse "1.5e-3"))"#,
    // Whitespace, in the forms RFC 8259 allows and most parsers skip.
    r#"(print (json-parse " \t\r\n [ 1.5 , { \"a\" : true } ] \n "))"#,
];

const TARGETS: &[&str] = &["aot", "python", "js", "ruby"];

/// The `ainl` binary under test.
///
/// `CARGO_BIN_EXE_ainl` is only set for a test that lives in the crate which
/// *defines* that binary, and this one does not — it lives in ainl-transpile and
/// drives the CLI from the workspace root. The release binary is what the other
/// check scripts use too, so prefer it and fall back to the debug build.
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
/// skip — every program here is expected to succeed, so an error surfaces.
///
/// The working directory is unique per call. Cargo runs tests in this binary on
/// several threads at once, and a shared `p.ainl` would have them reading each
/// other's program — which produces failures that look like backend bugs and
/// are not.
fn run(prog: &str, target: &str) -> Option<String> {
    let ainl = ainl_bin();
    let dir = std::env::temp_dir().join(format!("ainl-json-{target}-{}", next_id()));
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
            // A host that is not installed is a skip, not a failure.
            which(host)?;
            // `ainl transpile` writes the generated program to stdout, so it is
            // captured from the pipe and written to a path the host recognises.
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

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        panic!(
            "{target} exited {:?} on:\n{prog}\nstdout: {stdout}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Some(stdout.trim_end().to_string())
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var("PATH").ok().and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// A process-unique suffix for the scratch directory, so the tests in this
/// binary can run in parallel without sharing a file.
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn the_portable_corpus_contains_no_line_js_cannot_reproduce() {
    // Conservative invariant, kept from when JS had one number type and a bare
    // integer line was the one thing it could not reproduce. The tagged-number
    // fix closed that gap, so the guard is no longer load-bearing — but it is
    // cheap and still true, and it keeps the corpus honest if someone later
    // adds a bare integer. If a bare integer appears, quote it or put it beside
    // a float rather than widening an exemption.
    for line in run(PORTABLE, "interp").expect("interpreter").lines() {
        let bare = !line.is_empty()
            && line
                .bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+'))
            && !line.ends_with(".0");
        assert!(
            !bare,
            "PORTABLE prints the bare integer {line:?} — keep the corpus \
             free of bare-integer lines"
        );
    }
}

#[test]
fn the_portable_corpus_agrees_on_all_five_backends() {
    let expected = run(PORTABLE, "interp").expect("interpreter");
    for target in TARGETS {
        let Some(got) = run(PORTABLE, target) else {
            eprintln!("skipping {target}: host unavailable");
            continue;
        };
        assert_eq!(
            expected, got,
            "{target} disagrees with the interpreter on PORTABLE"
        );
    }
}

#[test]
fn every_case_agrees_on_all_five_backends() {
    for case in CASES {
        let expected = run(case, "interp").expect("interpreter");
        for target in TARGETS {
            let Some(got) = run(case, target) else {
                eprintln!("skipping {target} for `{case}`: host unavailable");
                continue;
            };
            assert_eq!(expected, got, "`{case}` differs on {target}");
        }
    }
}

#[test]
fn round_trip_is_value_identity_then_idempotent() {
    // Two distinct properties, checked on the interpreter because a
    // *value*-level property is not observable from any target's stdout:
    // `parse(serialize(v)) == v` is a value equality, whereas
    // `serialize(parse(text))` being a fixed point is about the bytes.
    let prog = r#"
(do
  (def v (json-parse "{\"a\":[1.5,2.5,{\"b\":null}],\"c\":\"x\"}"))
  (print (json-serialize v))
  (print (json-serialize (json-parse (json-serialize v))))
  (print (= v (json-parse (json-serialize v)))))
"#;
    let out = run(prog, "interp").expect("interpreter");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "got:\n{out}");
    assert_eq!(
        lines[0], lines[1],
        "serialization is not idempotent:\n{out}"
    );
    assert_eq!(lines[2], "true", "round trip lost a value:\n{out}");
}

#[test]
fn errors_are_reported_and_never_silently_accepted() {
    // Every rejection, on the interpreter and on the compiled binary: the AOT
    // runtime is a separate C implementation of the same parser, so an error
    // path that exists in one and not the other is exactly the bug this
    // catches.
    let bad = [
        // Malformed documents.
        r#"(json-parse "{")"#,
        r#"(json-parse "[1,]")"#,
        r#"(json-parse "{\"a\"}")"#,
        r#"(json-parse "{\"a\":}")"#,
        r#"(json-parse "tru")"#,
        r#"(json-parse "01")"#,
        r#"(json-parse "1.")"#,
        r#"(json-parse ".5")"#,
        r#"(json-parse "1e")"#,
        // Trailing content.
        r#"(json-parse "1 2")"#,
        r#"(json-parse "{} {}")"#,
        // Bad strings.
        r#"(json-parse "\"unterminated")"#,
        r#"(json-parse "\"\\q\"")"#,
        r#"(json-parse "\"\\u12\"")"#,
        r#"(json-parse "\"\\ud800\"")"#,
        r#"(json-parse "\"\\udc00\"")"#,
        r#"(json-parse "\"\\ud800\\u0041\"")"#,
        // Wrong argument types.
        r#"(json-parse 1)"#,
        r#"(json-parse nil)"#,
        // Values JSON cannot represent. AINL errors on a division by zero and
        // has no negation builtin that reaches -inf, so `+inf` is the only
        // non-finite float reachable from source; it is enough to prove the
        // branch, and the unit tests in ainl-core cover the other two.
        r#"(json-serialize (quote sym))"#,
        r#"(json-serialize (fn (x) x))"#,
        r#"(json-serialize (+ 1e308 1e308))"#,
        // A map key that is not a string has no faithful JSON form.
        r#"(json-serialize (hash 1.5 "v"))"#,
    ];
    for case in bad {
        let ainl = ainl_bin();
        let dir = std::env::temp_dir().join(format!("ainl-json-err-{}", next_id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let src = dir.join("e.ainl");
        std::fs::write(&src, case).expect("write");

        let interp = Command::new(&ainl)
            .arg("run")
            .arg(&src)
            .output()
            .expect("run ainl");
        let bin = dir.join("e.bin");
        let compile = Command::new(&ainl)
            .arg("compile")
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .output()
            .expect("compile ainl");
        assert!(
            compile.status.success(),
            "compile failed for `{case}`:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let aot = Command::new(&bin).output().expect("run the AOT binary");

        for (target, out) in [("interpreter", &interp), ("aot", &aot)] {
            assert!(
                !out.status.success(),
                "`{case}` should fail on the {target} but exited {:?}",
                out.status.code()
            );
            let msg = String::from_utf8_lossy(&out.stderr);
            assert!(
                msg.contains("json-parse") || msg.contains("json-serialize"),
                "`{case}` failed on the {target} without naming the builtin:\n{msg}"
            );
        }
    }
}

#[test]
fn js_now_preserves_the_int_float_distinction_in_json() {
    // Re-pin of the old `js_prints_every_number_as_a_float_because_it_has_one_
    // number_type` pin. That test asserted the documented divergence: JS has
    // one number type, so `json-serialize` turned an AINL int into a float and
    // printed `1` as `1.0`. The tagged-number fix (numeric-model card 1/6)
    // closed it — JS now wraps a float literal in `_Float` and leaves an int a
    // raw `Number`, so the distinction survives the round trip.
    //
    // Both cases now agree with the other four backends, and this pins that the
    // fix held: a whole float comes out `1.0`, a whole int comes out `1`, on
    // every backend including JS. If JS ever collapses the two again, this
    // fails in the int case.
    let float = r#"(print (json-serialize (nth (json-parse "[1.0]") 0)))"#;
    let int = r#"(print (json-serialize (nth (json-parse "[1]") 0)))"#;

    for target in ["interp", "aot", "python", "ruby", "js"] {
        let Some(f) = run(float, target) else {
            continue;
        };
        let Some(i) = run(int, target) else { continue };
        assert_eq!(f, "1.0", "{target} must read 1.0 as a float");
        assert_eq!(i, "1", "{target} must read 1 as an int");
    }
}

#[test]
fn an_integer_is_still_distinguishable_from_a_whole_float_where_the_type_allows_it() {
    // The `.0` suffix is not decoration: it is what keeps a float
    // distinguishable from an int in the output text. The same JSON is `1.0`
    // and `1` on every backend — including JS, whose tagged-number fix now
    // carries the int/float distinction.
    let floats = r#"(print (json-serialize (json-parse "[1.0,2.0]")))"#;
    let ints = r#"(print (json-serialize (json-parse "[1,2]")))"#;
    for target in ["interp", "aot", "python", "ruby", "js"] {
        let Some(f) = run(floats, target) else {
            continue;
        };
        let Some(i) = run(ints, target) else { continue };
        assert_eq!(f, "[1.0,2.0]", "{target} lost the .0 that marks a float");
        assert_eq!(i, "[1,2]", "{target} should emit a bare int");
        assert_ne!(f, i, "{target} collapsed a float and an int");
    }
}
