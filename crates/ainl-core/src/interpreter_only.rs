//! The shared "this form only the interpreter can run" scan.
//!
//! Two AINL features are **interpreter-only** today, and each of the three
//! backends that cannot support them says so rather than emitting something
//! that builds and behaves differently:
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
//! The scan is over the **whole tree**, not just the top level, so a nested
//! `import` is refused too — the interpreter reports *that* (it is
//! top-level-only) before codegen ever runs, but a backend still has to know
//! that *some* interpreter-only form is present, or it will emit a program that
//! fails at runtime with a confusing name-based error.
//!
//! `quote` is skipped, because its contents are data: a program that *talks
//! about* a module path — generating code, for instance — is an ordinary
//! program and must still build on every backend.

use crate::parser::Node;

/// The symbol that introduces a module import.
pub const IMPORT_SYM: &str = crate::import::IMPORT_SYM;

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

/// The symbols that make a program interpreter-only, in the order they are
/// reported (a program using two of them is refused on the first).
pub const INTERPRETER_ONLY: &[&str] = &[
    crate::import::IMPORT_SYM,
    crate::http::HTTP_GET,
    crate::http::HTTP_POST,
];

/// The byte offset of the first interpreter-only form, or `None`.
pub fn find_interpreter_only(forms: &[Node]) -> Option<(usize, &'static str)> {
    for sym in INTERPRETER_ONLY {
        if let Some(at) = find(forms, &[sym]) {
            return Some((at, sym));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn scan(src: &str) -> Option<(usize, &'static str)> {
        find_interpreter_only(&parse(src).expect("parses"))
    }

    #[test]
    fn a_plain_program_is_portable() {
        assert_eq!(scan(r#"(def f (fn (x) (* x 2))) (print (f 1))"#), None);
    }

    #[test]
    fn an_import_is_interpreter_only() {
        assert_eq!(scan(r#"(import "m.ainl")"#).map(|(_, s)| s), Some("import"));
    }

    #[test]
    fn a_nested_import_is_found() {
        let src = "(def f (fn () (import \"m.ainl\")))";
        assert_eq!(scan(src).map(|(_, s)| s), Some("import"));
    }

    #[test]
    fn either_http_builtin_is_interpreter_only() {
        assert_eq!(
            scan(r#"(http-get "http://x/")"#).map(|(_, s)| s),
            Some("http-get")
        );
        assert_eq!(
            scan(r#"(http-post "http://x/" "b")"#).map(|(_, s)| s),
            Some("http-post")
        );
        // Nested inside a function, the realistic place for it.
        assert_eq!(
            scan(r#"(def f (fn () (http-get "http://x/")))"#).map(|(_, s)| s),
            Some("http-get")
        );
    }

    #[test]
    fn a_quoted_mention_is_data() {
        // A program that *describes* an import or an HTTP call is an ordinary
        // program. Refusing it would make the code generators unable to
        // describe the language they are generators for.
        for src in [
            r#"(print (quote (import "m.ainl")))"#,
            r#"(print (quote (http-get "http://x/")))"#,
        ] {
            assert_eq!(scan(src), None, "quoted data was refused: {src}");
        }
    }

    #[test]
    fn an_empty_list_does_not_crash_the_scan() {
        // `()` is legal AINL and has no head to skip; slicing from 1 panics.
        for src in ["(def f (fn () 1))", "()", "(list ())", "(http-get)"] {
            let _ = scan(src);
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
}
