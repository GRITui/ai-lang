//! `http-get` / `http-post` are interpreter-only, and the backends say so out
//! loud.
//!
//! This is the twin of `import_refusal.rs`, and it exists for the same reason:
//! the failure it guards against is silent. A backend that does not handle the
//! HTTP builtins would leave `http-get` as an ordinary call to a free variable,
//! and the program would fail at *runtime* with a name-based lookup error that
//! reads like a codegen bug — or, in the transpilers, would lower onto a host
//! HTTP library and produce a program that builds cleanly, runs, and differs
//! from the interpreter in its header handling, timeouts and redirect behavior.
//!
//! AINL's whole premise is that a program means the same thing on every backend.
//! A backend that silently disagrees is worse than a backend that refuses, so
//! the refusal is the contract, and the contract is: refuse, name the reason,
//! and point at the interpreter.

use ainl_core::parse;

/// The part of the message that makes the refusal actionable.
const REFUSAL: &str = "interpreter-only";

fn aot_err(src: &str) -> String {
    let forms = parse(src).expect("the fixture itself must parse");
    ainl_cc::generate(&forms)
        .expect_err("codegen must refuse an HTTP program")
        .to_string()
}

fn transpile_err(src: &str, target: ainl_transpile::Target) -> String {
    let forms = parse(src).expect("the fixture itself must parse");
    ainl_transpile::transpile(target, &forms, src)
        .expect_err("a transpiler must refuse an HTTP program")
        .to_string()
}

/// The three hosts that must refuse. `Target` is an enum, so this list is the
/// only place the coverage is written down: a backend added to the enum has to
/// be added here too, or the loops below quietly stop covering it.
const HOSTS: &[ainl_transpile::Target] = &[
    ainl_transpile::Target::Python,
    ainl_transpile::Target::JavaScript,
    ainl_transpile::Target::Ruby,
];

// --- the AOT backend -------------------------------------------------------

#[test]
fn aot_refuses_http_get() {
    let msg = aot_err(r#"(http-get "http://127.0.0.1:8080/")"#);
    assert!(
        msg.contains(REFUSAL),
        "refusal must say what is unsupported, got: {msg}"
    );
    assert!(
        msg.contains("http-get"),
        "the refusal must name the offending builtin, got: {msg}"
    );
}

#[test]
fn aot_refuses_http_post() {
    let msg = aot_err(r#"(http-post "http://127.0.0.1:8080/" "b")"#);
    assert!(msg.contains("http-post"), "got: {msg}");
}

/// A nested call is the realistic shape — a helper that fetches — so the
/// refusal must not depend on the call being at the top level.
#[test]
fn aot_refuses_http_nested_in_a_function() {
    let msg = aot_err(r#"(def fetch (fn (u) (http-get u)))"#);
    assert!(msg.contains(REFUSAL), "got: {msg}");
}

/// The byte offset turns "this program uses http-get" into "…on this line".
#[test]
fn aot_refusal_points_at_a_byte_offset() {
    let msg = aot_err("(print 1)\n(print 2)\n(http-get \"http://x/\")\n");
    assert!(msg.contains("byte"), "refusal must locate it, got: {msg}");
}

#[test]
fn aot_refusal_is_a_proper_error_not_a_panic() {
    let forms = parse(r#"(http-get "http://x/")"#).expect("parses");
    match ainl_cc::generate(&forms) {
        Err(e) => {
            let msg = e.message();
            assert!(msg.contains(REFUSAL), "got: {msg}");
        }
        Ok(_) => panic!("codegen accepted a program with http-get"),
    }
}

// --- the transpilers -------------------------------------------------------

#[test]
fn every_transpiler_refuses_http_get() {
    for &t in HOSTS {
        let msg = transpile_err(r#"(http-get "http://127.0.0.1:8080/")"#, t);
        assert!(
            msg.contains(REFUSAL),
            "{}: refusal must be explicit, got: {msg}",
            t.label()
        );
        assert!(
            msg.contains("http-get"),
            "{}: the refusal must name the builtin, got: {msg}",
            t.label()
        );
    }
}

#[test]
fn every_transpiler_refuses_http_post() {
    for &t in HOSTS {
        let msg = transpile_err(r#"(http-post "http://x/" "b")"#, t);
        assert!(msg.contains(REFUSAL), "{}: got: {msg}", t.label());
    }
}

#[test]
fn every_transpiler_refuses_nested_http() {
    for &t in HOSTS {
        let msg = transpile_err(r#"(def f (fn (u) (http-get u)))"#, t);
        assert!(msg.contains(REFUSAL), "{}: got: {msg}", t.label());
    }
}

// --- the boundaries --------------------------------------------------------

/// Quoted data is data. A program that *talks about* an HTTP call — a code
/// generator describing one, for instance — is an ordinary program and must
/// still build on every backend.
#[test]
fn a_quoted_http_call_is_data_not_a_directive() {
    for src in [
        r#"(print (quote (http-get "http://x/")))"#,
        r#"(print (quote (http-post "http://x/" "b")))"#,
    ] {
        let forms = parse(src).expect("parses");
        let c = ainl_cc::generate(&forms).expect("a quoted call is not a directive");
        // The symbol itself must survive into the output — that is what makes
        // it data rather than a directive that was silently dropped. Both
        // cases are checked, since the symbol differs per fixture.
        let sym = if src.contains("http-post") {
            "http-post"
        } else {
            "http-get"
        };
        assert!(c.contains(sym), "generated C lost the quoted `{sym}`");
        for &t in HOSTS {
            let out = ainl_transpile::transpile(t, &forms, src)
                .expect("a quoted call must still transpile");
            assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
        }
    }
}

/// A program with no HTTP in it must be completely unaffected. This is what
/// keeps the refusal from being a regression for every program that does not
/// touch the network.
#[test]
fn a_program_with_no_http_is_untouched_on_every_backend() {
    let src = r#"(def f (fn (x) (* x 2)))
(print (f 21))"#;
    let forms = parse(src).expect("parses");
    assert!(ainl_cc::generate(&forms).is_ok(), "AOT unaffected");
    for &t in HOSTS {
        let out = ainl_transpile::transpile(t, &forms, src)
            .expect("a program with no HTTP is unaffected");
        assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
    }
}

/// `import` and the HTTP builtins share one refusal path, and a program using
/// *both* must still get a clean message naming one of them — not a panic, and
/// not a refusal that forgets which.
#[test]
fn a_program_using_both_is_refused_once_and_cleanly() {
    let msg = aot_err(
        r#"(import "m.ainl")
(http-get "http://x/")"#,
    );
    assert!(msg.contains(REFUSAL), "got: {msg}");
    assert!(
        msg.contains("byte"),
        "the offset must still be reported: {msg}"
    );
}

/// `()` is legal AINL and has no head to skip, so the shared scan must not
/// index into it. Pinned because the scan is shared by all four backends now:
/// a panic here would be a crash in the compiler, on ordinary source.
#[test]
fn empty_lists_do_not_crash_the_scan() {
    for src in [
        "(def f (fn () 1))",
        "()",
        "(list ())",
        "(http-get)",
        "(do ()) (http-post \"http://x/\" \"b\")",
    ] {
        let forms = parse(src).expect("parses");
        let _ = ainl_cc::generate(&forms);
        for &t in HOSTS {
            let _ = ainl_transpile::transpile(t, &forms, src);
        }
    }
}
