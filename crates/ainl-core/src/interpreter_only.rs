//! The shared "this form only some backends can run" scan.
//!
//! Some AINL features are **not portable across every backend**, and each
//! backend that cannot support one says so rather than emitting something that
//! builds and behaves differently:
//!
//! - [`crate::import::IMPORT_SYM`] (`import`) — module resolution happens in the
//!   load-time loader, which a single-file C/JS/Python/Ruby emission has no
//!   phase for. It is worse than merely unsupported: `import` is a *keyword* in
//!   all three transpiler targets, so an unhandled `(import "m")` lowers into
//!   the host's own import machinery — or, in Python, `import("m")`, which
//!   parses and silently does something else.
//! - [`crate::http::HTTP_GET`] / [`crate::http::HTTP_POST`] — a socket plus an
//!   HTTP/1.1 client, which the AOT C runtime would have to carry and the
//!   transpilers would have to map onto three different host HTTP libraries
//!   with different header, redirect and timeout semantics.
//!
//! Both are refused with the same contract, so the rule is worth having in one
//! place: three copies of "walk the tree, skip `quote`, report a byte offset"
//! is three chances to disagree about which programs a backend accepts. This
//! module is the single predicate both `ainl_cc` and `ainl_transpile` call.
//!
//! # Not all of them are refused by the same backends
//!
//! A symbol can be unavailable on *some* backends and work on others, so the
//! set is a **table**, not a list. `db-*` is the case that made the difference
//! matter: the AOT C runtime carries a hand-port of the storage engine (the
//! compiled binary opens the same `.ainl-db` and recovers from the same torn
//! tail — see `crates/ainl-cc/tests/db_crash.rs`), while the three transpilers
//! refuse, because a Python `open()` / a JS `fs` handle has no append-only-log
//! semantics and no way to reproduce the recovery. A flat "interpreter-only"
//! list would have had to either lose the AOT support or leave the transpilers
//! emitting a program that opens a file and behaves nothing like the log.
//!
//! That difference is *why* this is a table: the AOT backend must accept what
//! the transpilers refuse, and the two callers ask different questions of the
//! same scan.
//!
//! The scan is over the **whole tree**, not just the top level, so a nested
//! `import` is refused too — the interpreter reports *that* (it is
//! top-level-only) before codegen ever runs, but a backend still has to know
//! that *some* non-portable form is present, or it will emit a program that
//! fails at runtime with a confusing name-based error.
//!
//! `quote` is skipped, because its contents are data: a program that *talks
//! about* a module path — generating code, for instance — is an ordinary
//! program and must still build on every backend.

use crate::parser::Node;

/// The symbol that introduces a module import.
pub const IMPORT_SYM: &str = crate::import::IMPORT_SYM;

/// How many backends a portable form has to work on, and which ones refuse it.
///
/// The AOT C runtime is *not* in the transpiler set: it is a single-file
/// emission too, but it inlines its dependencies rather than deferring to a
/// host runtime, so the reasons that apply to Python/JS/Ruby do not apply to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The AOT C runtime (`ainl-cc`).
    Aot,
    /// The Python / JavaScript / Ruby transpilers, as one group.
    Transpilers,
}

/// A symbol and the backends that cannot run it.
pub struct Restricted {
    /// The symbol as it appears in source.
    pub sym: &'static str,
    /// The backends that refuse it, with the reason they give.
    pub refused_by: &'static [Backend],
}

/// Every symbol some backend refuses, in the order they are reported (a program
/// using two of them is refused on the first).
///
/// `import` and the HTTP builtins are refused by **both** groups; the `db-*`
/// builtins by the transpilers alone. The order is load-bearing only for which
/// message a two-form program gets, and every message names its own symbol, so
/// no program is left without an actionable refusal.
pub const RESTRICTED: &[Restricted] = &[
    Restricted {
        sym: crate::import::IMPORT_SYM,
        refused_by: &[Backend::Aot, Backend::Transpilers],
    },
    Restricted {
        sym: crate::http::HTTP_GET,
        refused_by: &[Backend::Aot, Backend::Transpilers],
    },
    Restricted {
        sym: crate::http::HTTP_POST,
        refused_by: &[Backend::Aot, Backend::Transpilers],
    },
    Restricted {
        sym: crate::db::DB_OPEN,
        refused_by: &[Backend::Transpilers],
    },
    Restricted {
        sym: crate::db::DB_CLOSE,
        refused_by: &[Backend::Transpilers],
    },
    Restricted {
        sym: crate::db::DB_PUT,
        refused_by: &[Backend::Transpilers],
    },
    Restricted {
        sym: crate::db::DB_GET,
        refused_by: &[Backend::Transpilers],
    },
    Restricted {
        sym: crate::db::DB_FLUSH,
        refused_by: &[Backend::Transpilers],
    },
];

/// The first use of any of `syms` anywhere in `forms`, as a byte offset.
///
/// Returns `None` when the program is portable across all four backends.
pub fn find(forms: &[Node], syms: &[&str]) -> Option<usize> {
    for form in forms {
        let Node::List(items, _) = form else {
            continue;
        };
        if let Some(Node::Sym(head, _)) = items.first() {
            if syms.contains(&head.as_str()) {
                return Some(form.span().start);
            }
            // `quote` is data: nothing inside it is ever evaluated, so there is
            // no directive to refuse.
            if head == "quote" {
                continue;
            }
        }
        // `()` is legal AINL — an empty `fn` parameter list, for one — so skip
        // the head only when there IS a head. Slicing from 1 on an empty list
        // panics, and the crash would be reachable from ordinary source.
        if !items.is_empty() {
            if let Some(at) = find(&items[1..], syms) {
                return Some(at);
            }
        }
    }
    None
}

/// The first use of any symbol `backend` refuses, as `(byte offset, symbol)`.
///
/// `None` means the backend can build this program — which is the question
/// `ainl_cc::generate` and `ainl_transpile::transpile` each ask of the same
/// table.
pub fn find_restricted(forms: &[Node], backend: Backend) -> Option<(usize, &'static str)> {
    for r in RESTRICTED {
        if r.refused_by.contains(&backend) {
            if let Some(at) = find(forms, &[r.sym]) {
                return Some((at, r.sym));
            }
        }
    }
    None
}

// `INTERPRETER_ONLY` and `find_interpreter_only` are the AOT half of the table,
// kept under their original names because `ainl_cc` is the caller that has had
// them since `import` shipped. The name is accurate on their own: nothing in
// this list works in a compiled binary. What changed is the *other* half — the
// transpilers, who now refuse a superset via `find_restricted(_, Transpilers)`.

/// The symbols the **AOT C backend** cannot run, in the order they are reported.
///
/// `db-*` is absent because the C runtime has a hand-port of the engine; see
/// [`crate::db`]. `import` is present even though `generate_program` can inline
/// it, because `generate` — the single-file path, and the one the refusal tests
/// drive — has no directory to resolve against and no way to inline. That
/// distinction is `generate`'s to explain, and it does.
pub const INTERPRETER_ONLY: &[&str] = &[
    crate::import::IMPORT_SYM,
    crate::http::HTTP_GET,
    crate::http::HTTP_POST,
];

/// The byte offset of the first AOT-refused form, or `None`.
pub fn find_interpreter_only(forms: &[Node]) -> Option<(usize, &'static str)> {
    find_restricted(forms, Backend::Aot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn scan(src: &str) -> Option<(usize, &'static str)> {
        find_restricted(&parse(src).expect("parses"), Backend::Aot)
    }

    fn scan_t(src: &str) -> Option<(usize, &'static str)> {
        find_restricted(&parse(src).expect("parses"), Backend::Transpilers)
    }

    #[test]
    fn a_plain_program_is_portable() {
        assert_eq!(scan(r#"(def f (fn (x) (* x 2))) (print (f 1))"#), None);
    }

    #[test]
    fn an_import_is_refused_by_the_aot_backend() {
        assert_eq!(scan(r#"(import "m.ainl")"#).map(|(_, s)| s), Some("import"));
    }

    #[test]
    fn a_nested_import_is_found() {
        let src = "(def f (fn () (import \"m.ainl\")))";
        assert_eq!(scan(src).map(|(_, s)| s), Some("import"));
    }

    #[test]
    fn either_http_builtin_is_refused_by_both() {
        for b in [Backend::Aot, Backend::Transpilers] {
            assert_eq!(
                find_restricted(&parse(r#"(http-get "http://x/")"#).unwrap(), b).map(|(_, s)| s),
                Some("http-get")
            );
            assert_eq!(
                find_restricted(&parse(r#"(http-post "http://x/" "b")"#).unwrap(), b)
                    .map(|(_, s)| s),
                Some("http-post")
            );
            // Nested inside a function, the realistic place for it.
            assert_eq!(
                find_restricted(
                    &parse(r#"(def f (fn () (http-get "http://x/")))"#).unwrap(),
                    b
                )
                .map(|(_, s)| s),
                Some("http-get")
            );
        }
    }

    #[test]
    fn the_db_builtins_work_on_aot_and_are_refused_by_the_transpilers() {
        // The asymmetry this table exists for. A flat list could not express
        // it, and getting it wrong in either direction is a real bug: accept
        // on a transpiler and it emits a program with a different file format;
        // refuse on the AOT backend and the compiled binary loses the engine
        // the card asked for.
        for (src, sym) in [
            (r#"(db-open "x.ainl-db")"#, "db-open"),
            (r#"(db-put 1 "k" "v")"#, "db-put"),
            (r#"(db-get 1 "k")"#, "db-get"),
            (r#"(db-flush 1)"#, "db-flush"),
            (r#"(db-close 1)"#, "db-close"),
        ] {
            let forms = parse(src).expect("parses");
            assert_eq!(
                find_restricted(&forms, Backend::Aot),
                None,
                "the AOT runtime has a hand-port and must accept {sym}"
            );
            assert_eq!(
                find_restricted(&forms, Backend::Transpilers).map(|(_, s)| s),
                Some(sym),
                "the transpilers must refuse {sym}"
            );
        }
    }

    #[test]
    fn a_nested_db_call_is_found_too() {
        let src = r#"(def store (fn (k v) (db-put 1 k v)))"#;
        assert_eq!(
            scan_t(src).map(|(_, s)| s),
            Some("db-put"),
            "the refusal must not depend on the call being at the top level"
        );
        assert_eq!(scan(src), None, "and AOT still builds it");
    }

    #[test]
    fn a_quoted_mention_is_data() {
        // A program that *describes* an import, an HTTP call or a db call is an
        // ordinary program. Refusing it would make the code generators unable
        // to describe the language they are generators for.
        for src in [
            r#"(print (quote (import "m.ainl")))"#,
            r#"(print (quote (http-get "http://x/")))"#,
            r#"(print (quote (db-open "x.ainl-db")))"#,
        ] {
            assert_eq!(scan(src), None, "quoted data was refused: {src}");
            assert_eq!(scan_t(src), None, "quoted data was refused: {src}");
        }
    }

    #[test]
    fn an_empty_list_does_not_crash_the_scan() {
        // `()` is legal AINL and has no head to skip; slicing from 1 panics.
        for src in ["(def f (fn () 1))", "()", "(list ())", "(http-get)"] {
            let _ = scan(src);
            let _ = scan_t(src);
        }
    }

    #[test]
    fn the_offset_points_at_the_offending_form() {
        let src = "(print 1)\n(print 2)\n(http-get \"http://x/\")\n";
        let (at, _) = scan(src).expect("found");
        assert_eq!(
            at,
            src.find("(http-get").expect("form is present"),
            "the offset must be the start of the http-get form, so a user can find the line"
        );
    }

    #[test]
    fn every_restricted_symbol_names_itself_in_the_table() {
        // A duplicate entry would make the second one unreachable, and an entry
        // with an empty `refused_by` would make a symbol unrefusable while
        // still looking restricted — both are silent table bugs.
        let mut seen = std::collections::BTreeSet::new();
        for r in RESTRICTED {
            assert!(
                !r.refused_by.is_empty(),
                "{} is restricted by nothing",
                r.sym
            );
            assert!(seen.insert(r.sym), "{} appears twice in RESTRICTED", r.sym);
            assert!(!r.sym.is_empty(), "an empty symbol cannot be matched");
        }
    }
}
