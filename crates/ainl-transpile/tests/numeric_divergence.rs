//! Pins the numeric-model behaviour documented in docs/NUMERIC_MODEL.md.
//!
//! **What this file is (numeric card 5/6):** the integration gate. Cards 1–4
//! each changed one runtime and re-pinned its own tests; this file is where
//! **all five backends are held to the same bytes at once, out of range**. Its
//! purpose is the inverse of what it used to be: before the numeric chain it
//! pinned the *known divergence* — the interpreter, Python and Ruby were exact
//! while JavaScript rounded, and the third test asserted that JS "diverges by
//! design". Card 4 (JS `BigInt`) retired that test, and card 5 (this one)
//! flipped the file's purpose outright: it now pins that **there is no
//! divergence**. The centerpiece is
//! `all_five_backends_agree_byte_for_byte_on_the_headline_corpus`, which runs
//! the three headline programs through the interpreter, the **AOT C binary**
//! (the backend this file previously did not include), JS, Python and Ruby,
//! and asserts byte-for-byte equality on **both** stdout and stderr.
//!
//! **What card 4 did (context):** JavaScript is no longer the odd one out. An
//! AINL `int` is emitted as a native JS `BigInt`, so JS integers are exact and
//! unbounded exactly as they are on the interpreter, the bytecode VM, the AOT
//! C binary, Python and Ruby. All five backends now agree on every integer
//! AINL can express — this file used to assert the one remaining divergence
//! and now asserts its absence.
//!
//! The old file's third test (`the_interpreter_still_diverges_from_js_by_design`)
//! is what card 4 retired: it asserted that JS rounded where the other four
//! were exact, "if this ever fails, JS moved to BigInt and the doc needs
//! updating". It failed, and the doc has been updated.
//!
//! Two properties are pinned here rather than just the headline numbers:
//!
//! * **The comparison rule.** int-vs-int compares EXACTLY (as the interpreter
//!   does), but any pair involving a float compares as **f64**. That is not the
//!   same as exact BigInt math, and it is the trap this switch sets: JS's
//!   native `<` compares a BigInt against a float exactly, so a transpiler that
//!   simply emitted the host operator would introduce a fresh divergence on
//!   precisely the values BigInt was adopted to fix. `_cmp` picks the
//!   interpreter's rule; these cases are what hold it there.
//!
//! * **Euclidean `mod`.** JS's `%` is truncated (the remainder takes the
//!   dividend's sign) and AINL's is Euclidean (`[0, |b|)`), so `(mod -7 3)` is 2
//!   and not -2. Three small cases, because the sign-of-`b` interaction is where
//!   a "reasonable" fix goes wrong.
//!
//! Gracefully skips a target whose runtime isn't installed, matching
//! `scripts/check-transpile.sh`.

use ainl_transpile::{transpile_js_src, transpile_python_src, transpile_ruby_src};
use std::process::Command;

/// The corpus from docs/NUMERIC_MODEL.md §"Measured agreement": the two values
/// that used to be out of `i64` range, plus 100! for a bignum many limbs wide.
const SRC: &str = r#"(print (* 9223372036854775807 2))
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
(print (fact 100))"#;

/// The exact result, now shared by ALL FIVE backends.
const EXACT: &str = concat!(
    "18446744073709551614\n",
    "15511210043330985984000000\n",
    "9332621544394415268169923885626670049071596826438162146859296389521759",
    "9993229915608941463976156518286253697920827223758251185210916864000000",
    "000000000000000000",
);

/// `100!` has 158 digits; the constant above is spelled in two halves above only
/// so the source line stays readable, so the two must be concatenated back into
/// the same 158 digits. Asserted below rather than trusted.
fn exact() -> String {
    let s = EXACT.to_string();
    assert_eq!(
        s.lines().count(),
        3,
        "the expected corpus is three lines: two products and a factorial"
    );
    s
}

/// The `ainl` CLI binary (release first, then debug).
///
/// Resolved once and shared by the interpreter and AOT helpers below; a missing
/// binary is a broken checkout, not a skip.
fn ainl_bin() -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    ["release", "debug"]
        .iter()
        .map(|p| root.join(p).join("ainl"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| {
            panic!("no ainl binary under target/{{release,debug}} — run `cargo build` first")
        })
}

/// The interpreter's STDOUT for `src`.
///
/// Not `run_str`, which answers the *value* of the last form: a program that
/// ends in `(print ...)` therefore returns nil, and a value-only comparison
/// would assert nil against the printed text. These cases are about what a
/// program PRINTS, so they read the CLI's stdout.
fn interp_stdout(src: &str) -> String {
    let tmp = temp_for("interp");
    std::fs::write(&tmp, src).unwrap();
    let out = Command::new(ainl_bin())
        .arg("run")
        .arg(&tmp)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ainl: {e}"));
    let _ = std::fs::remove_file(&tmp);
    assert!(
        out.status.success(),
        "interpreter errored: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The AOT C backend's RAW (stdout, stderr) for `src`.
///
/// Compiles `src` to a standalone binary with `ainl compile` — the same path
/// `scripts/parity5.sh` and `aot_numeric.rs` use — and runs it. Returns None
/// when `cc` is not installed, matching the graceful-skip of the other runners:
/// a missing C compiler is an environment gap, not a backend bug.
///
/// Raw (untrimmed) bytes: the byte-for-byte comparison in
/// `all_five_backends_agree_byte_for_byte_on_the_headline_corpus` includes the
/// trailing newline `print` emits, so nothing is stripped here.
fn aout(src: &str) -> Option<(String, String)> {
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("skipping aot: `cc` not installed");
        return None;
    }
    let src_path = temp_for("aot-src");
    let bin_path = temp_for("aot-bin");
    std::fs::write(&src_path, src).unwrap();
    let compile = Command::new(ainl_bin())
        .args([
            "compile",
            &src_path.to_string_lossy(),
            "-o",
            &bin_path.to_string_lossy(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke ainl compile: {e}"));
    let _ = std::fs::remove_file(&src_path);
    if !compile.status.success() || !bin_path.is_file() {
        panic!(
            "ainl compile failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }
    let out = Command::new(&bin_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run aot binary: {e}"));
    let _ = std::fs::remove_file(&bin_path);
    assert!(
        out.status.success(),
        "aot binary exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// The interpreter's RAW (stdout, stderr) for `src` — no trim, for the
/// byte-for-byte comparison.
fn interp_raw(src: &str) -> (String, String) {
    let tmp = temp_for("interp-raw");
    std::fs::write(&tmp, src).unwrap();
    let out = Command::new(ainl_bin())
        .arg("run")
        .arg(&tmp)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ainl: {e}"));
    let _ = std::fs::remove_file(&tmp);
    assert!(
        out.status.success(),
        "interpreter errored: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Run `code` under `runner`, returning the RAW (stdout, stderr) — no trim, so
/// the byte-for-byte comparison includes the trailing newline `print` emits.
/// Asserts a zero exit: a non-zero exit on one backend while the others print
/// is itself a divergence. Returns None when the runner isn't installed.
fn raw_run(runner: &str, code: &str, tag: &str) -> Option<(String, String)> {
    if Command::new(runner).arg("--version").output().is_err() {
        eprintln!("skipping {tag}: `{runner}` not installed");
        return None;
    }
    let tmp = temp_for(tag);
    std::fs::write(&tmp, code).unwrap();
    let out = Command::new(runner)
        .arg(&tmp)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {runner}: {e}"));
    std::fs::remove_file(&tmp).ok();
    assert!(
        out.status.success(),
        "{tag} exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// A temp path unique to this process AND this call.
///
/// Cargo runs the tests in one binary concurrently, and a shared filename makes
/// them read each other's program — which looks exactly like a backend bug.
/// Every helper here writes a program and immediately runs it, so the window is
/// small but real, and the failure is a mystery diff rather than a clear error.
fn temp_for(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ainl-numeric-divergence-{}-{}-{tag}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn run(runner: &str, code: &str, tag: &str) -> Option<String> {
    if Command::new(runner).arg("--version").output().is_err() {
        eprintln!("skipping {tag}: `{runner}` not installed");
        return None;
    }
    let tmp = temp_for(tag);
    std::fs::write(&tmp, code).unwrap();
    let out = Command::new(runner)
        .arg(&tmp)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {runner}: {e}"));
    std::fs::remove_file(&tmp).ok();
    assert!(
        out.status.success(),
        "{tag} exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn js_now_computes_the_exact_arbitrary_precision_result() {
    // The headline of this card. JS used to answer `18446744073709552000` and
    // `1.5511210043330986e+25` here because it held every integer as an f64; a
    // BigInt int makes it exact, so all three lines now match the other four.
    if let Some(out) = run("node", &transpile_js_src(SRC).unwrap(), "js") {
        assert_eq!(out, exact());
    }
}

#[test]
fn python_computes_the_exact_arbitrary_precision_result() {
    let py = transpile_python_src(SRC).unwrap();
    if let Some(out) = run("python3", &py, "python") {
        assert_eq!(out, exact());
    }
}

#[test]
fn ruby_computes_the_exact_arbitrary_precision_result() {
    let rb = transpile_ruby_src(SRC).unwrap();
    if let Some(out) = run("ruby", &rb, "ruby") {
        assert_eq!(out, exact());
    }
}

#[test]
fn js_agrees_with_python_and_ruby_byte_for_byte() {
    // The cross-language pairings, now that JS has arbitrary-precision integers
    // too. Before the switch this assertion could not have existed for JS.
    let js_out = run("node", &transpile_js_src(SRC).unwrap(), "js-cmp");
    let py_out = run("python3", &transpile_python_src(SRC).unwrap(), "python-cmp");
    let rb_out = run("ruby", &transpile_ruby_src(SRC).unwrap(), "ruby-cmp");
    if let (Some(js_out), Some(py_out)) = (&js_out, &py_out) {
        assert_eq!(js_out, py_out);
    }
    if let (Some(js_out), Some(rb_out)) = (&js_out, &rb_out) {
        assert_eq!(js_out, rb_out);
    }
    if let (Some(py_out), Some(rb_out)) = (&py_out, &rb_out) {
        assert_eq!(py_out, rb_out);
    }
}

#[test]
fn the_interpreter_agrees_exactly_with_every_transpiler() {
    // The VM and the tree-walk are compared on the expression's VALUE, which is
    // what they return.
    //
    // The transpiler is compared on what it PRINTS, so the expression is
    // wrapped in `(print …)`: a bare top-level expression produces no stdout on
    // any of the three targets, and comparing "" against the digits would fail
    // for a reason that has nothing to do with the arithmetic.
    let prelude = "(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))\n";
    for (expr, want) in [
        ("(* 9223372036854775807 2)", "18446744073709551614"),
        ("(fact 25)", "15511210043330985984000000"),
    ] {
        let src = format!("{prelude}{expr}");
        let vm = ainl_core::run_str(&src).unwrap_or_else(|e| panic!("vm {expr}: {e}"));
        let tw =
            ainl_core::run_in_tree_walk(&src).unwrap_or_else(|e| panic!("tree-walk {expr}: {e}"));
        assert_eq!(format!("{vm}"), want, "vm on {expr}");
        assert_eq!(format!("{tw}"), want, "tree-walk on {expr}");

        let printed = format!("{prelude}(print {expr})");
        let js = transpile_js_src(&printed).unwrap();
        if let Some(out) = run("node", &js, "js-vs-interp") {
            assert_eq!(out, want, "js on {expr}");
        }
        let py = transpile_python_src(&printed).unwrap();
        if let Some(out) = run("python3", &py, "python-vs-interp") {
            assert_eq!(out, want, "python on {expr}");
        }
        let rb = transpile_ruby_src(&printed).unwrap();
        if let Some(out) = run("ruby", &rb, "ruby-vs-interp") {
            assert_eq!(out, want, "ruby on {expr}");
        }
    }
}

#[test]
fn all_five_backends_agree_byte_for_byte_on_the_headline_corpus() {
    // THE integration gate (numeric card 5/6). The three headline programs from
    // docs/NUMERIC_MODEL.md — i64::MAX * 2, (fact 25), (fact 100) — must come
    // out byte-for-byte identical on EVERY backend, on BOTH streams.
    //
    // This is the test the whole chain was built to make true: cards 1–4 each
    // changed one runtime and re-pinned its own tests, and this is the first
    // place all five backends are held to the SAME bytes at once, out of range.
    // It is deliberately the inverse of what this file used to be — before card
    // 4 it pinned the *divergence* (four different answers); now it pins the
    // *absence* of one. If any backend drifts, this fails with the two outputs
    // in the message, so a regression is a visible diff, not a silent one.
    //
    // The interpreter is the reference. AOT is compiled with `ainl compile`
    // (the same path parity5.sh and aot_numeric.rs use); JS/Python/Ruby are
    // transpiled and run under their host. Each is compared to the
    // interpreter's RAW stdout and RAW stderr — untrimmed, so the trailing
    // newline `print` emits is part of the bytes, and a stderr difference
    // (an error on one backend, a print on another) is caught too.
    let (i_out, i_err) = interp_raw(SRC);

    // AOT C binary — the backend that was NOT in this file before card 5.
    if let Some((a_out, a_err)) = aout(SRC) {
        assert_eq!(
            a_out, i_out,
            "AOT stdout diverges from the interpreter.\n--- aot ---\n{a_out}\n--- interp ---\n{i_out}"
        );
        assert_eq!(
            a_err, i_err,
            "AOT stderr diverges from the interpreter.\n--- aot ---\n{a_err}\n--- interp ---\n{i_err}"
        );
    }

    let js = transpile_js_src(SRC).unwrap();
    let py = transpile_python_src(SRC).unwrap();
    let rb = transpile_ruby_src(SRC).unwrap();
    if let Some((out, err)) = raw_run("node", &js, "gate-js") {
        assert_eq!(
            out, i_out,
            "JS stdout diverges.\n--- js ---\n{out}\n--- interp ---\n{i_out}"
        );
        assert_eq!(
            err, i_err,
            "JS stderr diverges.\n--- js ---\n{err}\n--- interp ---\n{i_err}"
        );
    }
    if let Some((out, err)) = raw_run("python3", &py, "gate-py") {
        assert_eq!(
            out, i_out,
            "Python stdout diverges.\n--- py ---\n{out}\n--- interp ---\n{i_out}"
        );
        assert_eq!(
            err, i_err,
            "Python stderr diverges.\n--- py ---\n{err}\n--- interp ---\n{i_err}"
        );
    }
    if let Some((out, err)) = raw_run("ruby", &rb, "gate-rb") {
        assert_eq!(
            out, i_out,
            "Ruby stdout diverges.\n--- rb ---\n{out}\n--- interp ---\n{i_out}"
        );
        assert_eq!(
            err, i_err,
            "Ruby stderr diverges.\n--- rb ---\n{err}\n--- interp ---\n{i_err}"
        );
    }
}

#[test]
fn int_equality_is_exact_but_a_float_comparison_goes_through_f64() {
    // The rule `_cmp` implements, pinned on both sides of it.
    //
    // 2^53+1 and 2^53 are DIFFERENT ints (exact BigInt equality says false) but
    // the SAME f64, so `=` answers true — which is what the interpreter does,
    // because `Value`'s `PartialEq` sends a float-involving pair through
    // `to_f64`. A JS target that used native `===` on a BigInt against a
    // number would answer `true` here for the right reason by accident, and
    // `false` on the case below for the wrong one.
    let src = "(print (= 9007199254740993 (+ 9007199254740992 0.5)))\n\
               (print (= 9007199254740993 9007199254740992))\n\
               (print (= 9007199254740993 (* 9007199254740993 1)))";
    let want = "true\nfalse\ntrue";

    assert_eq!(interp_stdout(src), want, "interpreter");
    if let Some(out) = run("node", &transpile_js_src(src).unwrap(), "js-eq") {
        assert_eq!(out, want, "js");
    }
}

#[test]
fn relational_comparison_of_a_big_int_and_a_float_follows_the_interpreter() {
    // The case that makes native `<` wrong: exact BigInt math answers `true`
    // (2^53+1 < 2^53+2), while the interpreter converts both to f64
    // (2^53+1 rounds DOWN to 2^53) and answers `false`. `_cmp` has to pick the
    // f64 rule to stay byte-identical.
    let src = "(print (< 9007199254740993 (+ 9007199254740992 1.0)))\n\
               (print (< 9007199254740995 (+ 9007199254740992 1.0)))";
    let want = "false\nfalse";

    assert_eq!(interp_stdout(src), want, "interpreter");
    if let Some(out) = run("node", &transpile_js_src(src).unwrap(), "js-cmp2") {
        assert_eq!(out, want, "js");
    }
}

#[test]
fn mod_is_euclidean_not_javascripts_truncated_percent() {
    // `rem_euclid` is in `[0, |b|)`, so the answer does not depend on b's sign.
    // JS's `%` is truncated, so all four negative-operand rows below are exactly
    // where a naive port differs, and `r += b` (rather than `r += |b|`) is the
    // subtle one: it gives 1 where AINL gives 2 for `(mod -7 -3)`.
    let src = "(print (mod 7 3))\n\
               (print (mod 7 -3))\n\
               (print (mod -7 3))\n\
               (print (mod -7 -3))\n\
               (print (mod 0 3))";
    let want = "1\n1\n2\n2\n0";

    assert_eq!(interp_stdout(src), want, "interpreter");
    if let Some(out) = run("node", &transpile_js_src(src).unwrap(), "js-mod") {
        assert_eq!(out, want, "js");
    }
}

#[test]
fn a_big_int_travels_through_math_json_and_a_float_without_a_type_error() {
    // The conversion boundaries the card calls out as the real risk, exercised
    // together in one program: a value beyond f64's exact-integer range (so
    // every conversion is lossy by construction and has to be deliberate),
    // through `Math.sqrt` (f64 in, float out), through `json-serialize`
    // (BigInt would throw in `JSON.stringify`), and mixed with a float in
    // arithmetic (which is a `TypeError` in JS if left implicit).
    // Deliberately no `(* big 1.5)` / `(+ big 0.5)` row: those land on an f64
    // whose exact expansion and shortest round-trip form differ, and the
    // interpreter prints the exact one while every transpiler prints the
    // shortest. That is a documented float-DISPLAY rule (NUMERIC_MODEL.md), it
    // is identical on the pre-BigInt baseline, and it is not what this card is
    // about — so the rows below stay on values where the two agree.
    let src = "(def big (* 9223372036854775807 2))\n\
               (print big)\n\
               (print (+ big 1))\n\
               (print (* big 2))\n\
               (print (sqrt big))\n\
               (print (json-serialize big))\n\
               (print (min big 1))\n\
               (print (abs (- 0 big)))";
    let want = interp_stdout(src);

    if let Some(out) = run("node", &transpile_js_src(src).unwrap(), "js-boundaries") {
        assert_eq!(out, want, "js boundary program");
    }
}

#[test]
fn the_int_float_index_rule_is_unaffected_by_the_switch() {
    // `substring` takes an int index, and an int is a BigInt now, so the
    // rejection of a float index has to be re-pinned: the test cannot be
    // `Number.isInteger` any more (it would reject every valid index), and a
    // coercion would silently accept `(substring "abc" 0 1.0)`.
    let src = "(print (substring \"abc\" 0 1.0))";
    let vm = ainl_core::run_str(src).unwrap_err();
    assert!(
        vm.to_string().contains("expects an int"),
        "interpreter must reject a float index: {vm}"
    );
    let js = transpile_js_src(src).unwrap();
    let tmp = temp_for("floatidx");
    std::fs::write(&tmp, &js).unwrap();
    let out = Command::new("node").arg(&tmp).output().unwrap();
    let _ = std::fs::remove_file(&tmp);
    assert!(
        !out.status.success(),
        "js must reject a float index too; it printed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("expects an int"),
        "js must word it like the other backends: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
