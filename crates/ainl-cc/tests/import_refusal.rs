//! `import` is interpreter-only, and the backends say so out loud.
//!
//! This is the test that matters most for correctness, because the failure it
//! guards against is silent. Before these guards, a program containing
//! `(import "m")` passed every backend check and then did the wrong thing:
//!
//! - `ainl compile` emitted a C program that failed at runtime with
//!   `unbound variable: import`, which reads like a codegen bug rather than an
//!   unsupported feature.
//! - the transpilers emitted a call to the *host* language's import machinery,
//!   because `import` is a keyword in Python, Ruby and JavaScript. That program
//!   compiles cleanly, links cleanly, and silently does something else.
//!
//! So the contract is not "we eventually support this" but "we refuse, and the
//! refusal names the reason." Each case below asserts the refusal AND that the
//! message is about interpreter-only, so a generic "unsupported" or a changed
//! error string fails the test.

use ainl_core::{parse, Error};

/// The message every backend uses to refuse an import, and the part that makes
/// the refusal actionable.
const REFUSAL: &str = "interpreter-only";

fn err(src: &str) -> String {
    let forms = parse(src).expect("the fixture itself must parse");
    ainl_cc::generate(&forms)
        .expect_err("codegen must refuse a program with imports")
        .to_string()
}

#[test]
fn aot_refuses_a_top_level_import() {
    let msg = err(r#"(import "lib/math.ainl")"#);
    assert!(
        msg.contains(REFUSAL),
        "refusal must say what is unsupported, got: {msg}"
    );
}

#[test]
fn aot_refuses_a_namespaced_import() {
    let msg = err(r#"(import "lib/math.ainl" as m)"#);
    assert!(msg.contains(REFUSAL), "got: {msg}");
}

/// The AOT backend is a whole-program compiler, so a `def` that *calls* an
/// imported name is just a call — the import only matters at the top. A
/// nested import is still a real import and must still be refused, even though
/// the interpreter rejects it for a different reason (top-level only) before
/// codegen ever runs.
#[test]
fn aot_refuses_an_import_nested_in_a_function() {
    let msg = err(r#"(def f (fn () (import "lib/math.ainl")))"#);
    assert!(msg.contains(REFUSAL), "got: {msg}");
}

/// The byte offset lets a user find the offending line in a long file instead
/// of grepping for it.
#[test]
fn aot_refusal_points_at_a_byte_offset() {
    let msg = err("(print 1)\n(print 2)\n(import \"m.ainl\")\n");
    assert!(
        msg.contains("byte"),
        "refusal must locate the import, got: {msg}"
    );
}

/// `()` is legal AINL — an empty `fn` parameter list is the everyday case — and
/// the import scan walks every subform. An empty list has no head to skip, so
/// slicing the head off it panics. That crash is reachable from ordinary source
/// on any program with a zero-argument function, import or no import, so it is
/// pinned here rather than left to chance.
#[test]
fn an_empty_parameter_list_does_not_crash_the_scan() {
    for src in [
        "(def f (fn () 1))",
        "(def f (fn () (import \"m.ainl\")))",
        "()",
        "(list ())",
    ] {
        let forms = parse(src).expect("parses");
        // The assertion is that we get here at all.
        let _ = ainl_cc::generate(&forms);
        for &t in HOSTS {
            let _ = transpiles(src, t);
        }
    }
}

/// Quoted data is data. A program that *talks about* an import — generating
/// code, in this case, which AINL's `generate` is built for — is a perfectly
/// ordinary program and must still compile to C. Refusing it would make
/// `generate` unable to describe a language feature, which would be a bug in
/// the wrong direction.
#[test]
fn aot_allows_a_quoted_import() {
    let forms = parse(r#"(print (quote (import "m.ainl")))"#).expect("parses");
    let c = ainl_cc::generate(&forms).expect("quoted import is data, not a directive");
    assert!(
        c.contains("import"),
        "the generated C should contain the quoted text"
    );
}

/// The refusal must be an `Error`, not a panic and not an empty string, so the
/// CLI can print it and exit non-zero.
#[test]
fn aot_refusal_is_a_proper_error() {
    let forms = parse(r#"(import "m.ainl")"#).expect("parses");
    match ainl_cc::generate(&forms) {
        Err(Error::Runtime(msg)) => assert!(msg.contains(REFUSAL), "got: {msg}"),
        Err(other) => panic!("expected a runtime error, got: {other:?}"),
        Ok(_) => panic!("codegen accepted a program with imports"),
    }
}

// --- transpilers ---------------------------------------------------------
//
// Each host language gets its own case, because the reason for refusing
// differs: `import` is a keyword in all three, but what it means in each is
// different again, and a transpiler that special-cased one of them correctly
// would be no more correct than one that special-cased none of them.

/// Transpile with an explicit target, so a test failure names the backend
/// rather than reporting three identical errors.
fn transpiles(src: &str, target: ainl_transpile::Target) -> ainl_core::Result<String> {
    let forms = parse(src).expect("parses");
    ainl_transpile::transpile(target, &forms, src)
}

/// The three hosts that must refuse. `Target` is an enum, so this list is the
/// only place the coverage is written down: a backend added to the enum has to
/// be added here too, or the loops below quietly stop covering it.
const HOSTS: &[ainl_transpile::Target] = &[
    ainl_transpile::Target::Python,
    ainl_transpile::Target::JavaScript,
    ainl_transpile::Target::Ruby,
];

#[test]
fn every_transpiler_refuses_a_top_level_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(import "lib/math.ainl")"#, t)
            .expect_err("must refuse an import")
            .to_string();
        assert!(
            msg.contains(REFUSAL),
            "{}: refusal must be explicit, got: {msg}",
            t.label()
        );
    }
}

#[test]
fn every_transpiler_refuses_a_namespaced_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(import "lib/math.ainl" as m)"#, t)
            .expect_err("must refuse a namespaced import")
            .to_string();
        assert!(msg.contains(REFUSAL), "{}: got: {msg}", t.label());
    }
}

#[test]
fn every_transpiler_refuses_a_nested_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(def f (fn () (import "m.ainl")))"#, t)
            .expect_err("must refuse a nested import")
            .to_string();
        assert!(msg.contains(REFUSAL), "{}: got: {msg}", t.label());
    }
}

/// A program with no `import` must be completely unaffected — this is what
/// keeps the refusal from being a regression for everyone who does not use
/// modules. AINL already has an AOT/transpiler parity suite that compares each
/// backend's output against the interpreter's, so the claim that the untouched
/// path still works is covered there; what this adds is that the refusal checks
/// above do not fire for an ordinary program.
#[test]
fn every_transpiler_is_untouched_without_an_import() {
    let src = r#"(def f (fn (x) (* x 2)))
(print (f 21))"#;
    for &t in HOSTS {
        let out = transpiles(src, t).expect("a program with no import is unaffected");
        // Sanity: it really did generate something host-shaped, so this test
        // cannot pass by returning an empty string on both sides.
        assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
    }
    let forms = parse(src).expect("parses");
    assert!(ainl_cc::generate(&forms).is_ok(), "AOT unaffected");
}
