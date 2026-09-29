//! The `db-*` builtins are refused by every transpiler, and the AOT backend is
//! explicitly *not* one of them.
//!
//! This is the twin of `http_refusal.rs`, and the reasoning is the same in one
//! respect and different in another.
//!
//! **Same:** the failure being guarded against is silent. A transpiler that
//! lowered `db-open` onto Python's `open()` would produce a program that builds
//! cleanly, runs, and then diverges from the interpreter in every way that
//! matters — no append-only log, no per-record checksum, no crash-tail recovery,
//! no `fsync` in `db-flush`, and a different answer to "what happens when two
//! writers append at once". A caller who ran the same AINL through two backends
//! would get two different databases and no warning. A refusal is strictly
//! better than that, so the refusal is the contract.
//!
//! **Different:** the refusal is *not* uniform across the four backends. The AOT
//! backend supports all five builtins through the C hand-port in `runtime.c` —
//! libc gives it the file control the host languages' standard libraries do not.
//! So the interesting assertion here is a negative one in both directions: the
//! transpilers must refuse, and the AOT backend must not have been swept up in a
//! blanket "storage is too hard" rule by accident. Both are asserted.

use ainl_core::parse;

/// The part of the message that makes the refusal actionable. Matches the
/// wording the `import` and HTTP refusals already use, so a user who has seen
/// one sees the same shape for the other.
const REFUSAL: &str = "interpreter-only";

/// The storage refusal says **transpiler-only** instead, and deliberately so.
/// "Interpreter-only" would send a user looking for `ainl run` when `ainl
/// compile` — the AOT C backend, which carries a hand-port of the engine — runs
/// the program perfectly well. The message has to name the backends that
/// actually refuse.
const STORAGE_REFUSAL: &str = "transpiler-only";

/// The three hosts that must refuse. `Target` is an enum, so this is the only
/// place the coverage is written down: a backend added to the enum has to be
/// added here too, or the loop quietly stops covering it.
const HOSTS: &[ainl_transpile::Target] = &[
    ainl_transpile::Target::Python,
    ainl_transpile::Target::JavaScript,
    ainl_transpile::Target::Ruby,
];

/// The nine builtins, with a program that calls each.
///
/// The refusal test and the AOT-supports test below are driven by this same
/// list, so they cannot drift apart — a builtin added to one and forgotten in
/// the other would leave it untested on exactly one backend, which is the one
/// place a backend's coverage silently shrinks.
///
/// All nine are expected to be **accepted** by the AOT backend. Asserting that
/// is what keeps a blanket "storage is too hard for a compiled binary" rule
/// from sweeping the AOT backend in by accident: every refusal test could keep
/// passing while the backend that actually ships the binary lost the feature.
const CALLS: &[(&str, &str)] = &[
    ("db-open", r#"(db-open "d.ainl-db")"#),
    ("db-put", r#"(db-put 1 "k" "v")"#),
    ("db-get", r#"(db-get 1 "k")"#),
    ("db-flush", "(db-flush 1)"),
    ("db-close", "(db-close 1)"),
    // The value layer (Tier 4 card 2).
    ("db-set", r#"(db-set 1 "k" 2)"#),
    ("db-get-raw", r#"(db-get-raw 1 "k")"#),
    ("db-del", r#"(db-del 1 "k")"#),
    ("db-keys", "(db-keys 1)"),
    ("db-count", "(db-count 1)"),
];

/// The call program for `sym`, from the table above.
///
/// A helper because the two tests that use `CALLS` must agree on which program
/// goes with which name; a lookup that fell back to a default would let a name
/// be listed and never actually exercised.
fn program_for(sym: &str) -> &'static str {
    CALLS
        .iter()
        .find(|(name, _)| *name == sym)
        .map(|(_, src)| *src)
        .unwrap_or_else(|| panic!("{sym} is in a name list but has no fixture program"))
}

fn transpile_err(src: &str, target: ainl_transpile::Target) -> String {
    let forms = parse(src).expect("the fixture itself must parse");
    ainl_transpile::transpile(target, &forms, src)
        .expect_err("a transpiler must refuse a storage program")
        .to_string()
}

// --- the transpilers refuse -----------------------------------------------

#[test]
fn every_transpiler_refuses_every_storage_builtin() {
    for &(name, src) in CALLS {
        for &t in HOSTS {
            let msg = transpile_err(src, t);
            assert!(
                msg.contains(STORAGE_REFUSAL),
                "{}: refusal for {name} must be explicit, got: {msg}",
                t.label()
            );
            assert!(
                msg.contains(name),
                "{}: the refusal must name {name}, got: {msg}",
                t.label()
            );
            assert!(
                !msg.contains(REFUSAL),
                "{}: {name} is NOT interpreter-only — `ainl compile` runs it, so \
                 pointing the user at `ainl run` would be wrong. Got: {msg}",
                t.label()
            );
        }
    }
}

/// A nested call is the realistic shape — a helper that stores — so the refusal
/// must not depend on the call being at the top level.
#[test]
fn every_transpiler_refuses_storage_nested_in_a_function() {
    for &t in HOSTS {
        let msg = transpile_err(r#"(def put (fn (k v) (db-put 1 k v)))"#, t);
        assert!(msg.contains(STORAGE_REFUSAL), "{}: got: {msg}", t.label());
        assert!(msg.contains("db-put"), "{}: got: {msg}", t.label());
    }
}

/// The byte offset turns "this program uses storage" into "…on this line".
#[test]
fn the_refusal_points_at_a_byte_offset() {
    for &t in HOSTS {
        let msg = transpile_err("(print 1)\n(print 2)\n(db-open \"d\")\n", t);
        assert!(
            msg.contains("byte"),
            "{}: the refusal must locate it, got: {msg}",
            t.label()
        );
    }
}

#[test]
fn the_refusal_is_a_proper_error_not_a_panic() {
    for &t in HOSTS {
        let forms = parse(r#"(db-open "d.ainl-db")"#).expect("parses");
        match ainl_transpile::transpile(t, &forms, r#"(db-open "d.ainl-db")"#) {
            Err(e) => assert!(
                e.message().contains(STORAGE_REFUSAL),
                "{}: got: {e}",
                t.label()
            ),
            Ok(_) => panic!("{} accepted a storage program", t.label()),
        }
    }
}

/// One refusal is enough. A program using several must get a clean message
/// naming one of them — not a panic, and not a refusal that lists nothing.
#[test]
fn a_program_using_several_is_refused_once_and_cleanly() {
    let src = r#"(do (def h (db-open "d.ainl-db"))
            (db-put h "k" "v")
            (db-flush h)
            (db-close h))"#;
    for &t in HOSTS {
        let msg = transpile_err(src, t);
        assert!(msg.contains(STORAGE_REFUSAL), "{}: got: {msg}", t.label());
        let named = CALLS.iter().filter(|(n, _)| msg.contains(n)).count();
        assert!(
            named >= 1,
            "{}: the refusal names no storage builtin: {msg}",
            t.label()
        );
    }
}

// --- the AOT backend does not refuse ---------------------------------------

/// The counterweight to everything above. The AOT C backend implements all nine
/// builtins, so codegen must accept them — if a future change to the shared
/// backend-restriction table swept `db-*` in with the transpiler set, this is
/// the test that catches it, and it is the only place it would be caught.
#[test]
fn aot_accepts_every_storage_builtin() {
    for &(name, src) in CALLS {
        let forms = parse(src).unwrap_or_else(|e| panic!("the {name} fixture must parse: {e}"));
        ainl_cc::generate(&forms)
            .unwrap_or_else(|e| panic!("AOT must support {name}, but refused: {e}"));
    }
}

/// Every name in the two modules' own lists has a fixture above.
///
/// This is the drift guard for the table: a builtin added to `db.rs` or
/// `dbkv.rs` and not to `CALLS` would be refused by the transpilers (the
/// scanner reads the module lists) and accepted by AOT (codegen reads
/// `BUILTIN_IDS`) while *no test in this file exercised it*. Comparing the
/// lists to the table is what turns that into a failure here.
#[test]
fn every_db_name_in_the_modules_has_a_fixture() {
    for &sym in ainl_core::db::DB_BUILTINS
        .iter()
        .chain(ainl_core::dbkv::KV_BUILTINS.iter())
    {
        program_for(sym);
    }
    // Five byte-layer names plus five value-layer names, with `db-get` in both
    // lists and one fixture for it. So ten fixtures for ten distinct names, and
    // the AOT-accepts test above proves each one is really reached.
    assert_eq!(CALLS.len(), 10, "update CALLS when a db-* builtin is added");
}

/// And the whole surface at once, so a *combination* is not what breaks it.
///
/// Both layers in one program, and every id asserted. `db-get` is here with
/// id 69 — the id the byte layer shipped — because the value layer takes that
/// one name over rather than getting an id of its own, and the other four value
/// builtins are the ids appended after it.
#[test]
fn aot_accepts_a_complete_storage_program() {
    let src = r#"(do (def h (db-open "d.ainl-db"))
            (db-put h "k" "v")
            (db-set h "n" 1)
            (db-flush h)
            (db-get h "k")
            (db-get-raw h "k")
            (db-del h "n")
            (db-keys h)
            (db-count h)
            (db-close h))"#;
    let forms = parse(src).expect("parses");
    let c = ainl_cc::generate(&forms).expect("AOT must support the full program");
    // The generated C must emit a `v_builtin(N)` for each call, N being the ID
    // from BUILTIN_IDS. Asserting on the *call sites* rather than on the symbol
    // names is the point: the whole runtime is inlined into the output, so
    // `B_DB_OPEN` is present whether or not the program ever calls it. A
    // generator that accepted the program but emitted nothing for a call would
    // produce a binary failing at runtime with an unbound symbol, and this is
    // the cheapest place to catch that.
    for (name, id) in [
        ("db-open", "67"),
        ("db-put", "68"),
        ("db-get", "69"),
        ("db-flush", "70"),
        ("db-close", "71"),
        ("db-set", "72"),
        ("db-get-raw", "73"),
        ("db-del", "74"),
        ("db-keys", "75"),
        ("db-count", "76"),
    ] {
        assert!(
            c.contains(&format!("v_builtin({id})")),
            "the generated C never emits a call site for {name} (id {id})"
        );
    }
}

// --- the boundaries --------------------------------------------------------

/// Quoted data is data. A program that *talks about* a storage call — a code
/// generator describing one, for instance — is an ordinary program and must still
/// build on every backend.
#[test]
fn a_quoted_storage_call_is_data_not_a_directive() {
    for &(name, _) in CALLS {
        let src = format!("(print (quote ({name} \"x\")))");
        let forms = parse(&src).expect("parses");
        let c = ainl_cc::generate(&forms).expect("a quoted call is not a directive");
        assert!(c.contains(name), "generated C lost the quoted `{name}`");
        for &t in HOSTS {
            let out = ainl_transpile::transpile(t, &forms, &src)
                .expect("a quoted call must still transpile");
            assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
        }
    }
}

/// A program with no storage in it must be completely unaffected on all four
/// backends. This is what keeps the refusal from being a regression for every
/// program that does not touch a database.
#[test]
fn a_program_with_no_storage_is_untouched_on_every_backend() {
    let src = r#"(def f (fn (x) (* x 2)))
(print (f 21))"#;
    let forms = parse(src).expect("parses");
    assert!(ainl_cc::generate(&forms).is_ok(), "AOT unaffected");
    for &t in HOSTS {
        let out = ainl_transpile::transpile(t, &forms, src)
            .expect("a program with no storage is unaffected");
        assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
    }
}

/// A variable *named* like a builtin is not a call. `(def db-get 1)` must not be
/// read as a use of the builtin — the scan looks at call heads, and a rule that
/// matched bare symbols would refuse every program that happens to name a
/// variable `db-get`.
#[test]
fn a_variable_named_like_a_builtin_is_not_a_use() {
    let src = r#"(do (def db-open 1)
            (def db-put 2)
            (print db-open db-put))"#;
    let forms = parse(src).expect("parses");
    assert!(
        ainl_cc::generate(&forms).is_ok(),
        "AOT: a binding is not a use"
    );
    for &t in HOSTS {
        let out = ainl_transpile::transpile(t, &forms, src).expect("a binding is not a use");
        assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
    }
}

/// The Tier 3 refusals and the Tier 4 refusals share one scan, so the older ones
/// must still be refused — including when the program also uses storage. This is
/// the regression that a "widen the set" change to the transpiler policy would
/// most plausibly cause, by accident, on a backend nobody retested.
#[test]
fn the_older_refusals_still_hold_alongside_storage() {
    for &t in HOSTS {
        for src in [
            r#"(import "m.ainl")"#,
            r#"(http-get "http://x/")"#,
            r#"(http-post "http://x/" "b")"#,
        ] {
            let forms = parse(src).expect("parses");
            assert!(
                ainl_transpile::transpile(t, &forms, src).is_err(),
                "{}: `{src}` must still be refused",
                t.label()
            );
        }
    }
}

/// `()` is legal AINL and has no head to skip, so the shared scan must not index
/// into it. Pinned because the scan is shared by four backends now: a panic
/// here is a crash in the compiler, on ordinary source.
#[test]
fn empty_lists_do_not_crash_the_scan() {
    for src in [
        "(def f (fn () 1))",
        "()",
        "(list ())",
        "(db-open)",
        "(db-put 1 2)",
        r#"(do (db-open "d") (db-close 1))"#,
    ] {
        let forms = parse(src).expect("parses");
        let _ = ainl_cc::generate(&forms);
        for &t in HOSTS {
            let _ = ainl_transpile::transpile(t, &forms, src);
        }
    }
}
