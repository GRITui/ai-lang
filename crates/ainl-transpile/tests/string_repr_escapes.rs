//! The transpiler `_repr` helper must escape a string's own `"` and `\`, the
//! way the interpreter and the AOT C runtime already do.
//!
//! ## The bug
//!
//! A string rendered inside a container goes through `_repr`, which wraps it in
//! double quotes. All three transpilers did that and nothing else:
//!
//! ```text
//! (print (str (list "a\"b")))
//! ```
//!
//! | backend | output |
//! |---|---|
//! | interpreter (`Value::repr`, Rust's `{:?}`) | `("a\"b")` |
//! | AOT C (`value_repr`)                      | `("a\"b")` |
//! | python / js / ruby                        | `("a"b")` |
//!
//! The second row is ambiguous — the reader cannot tell a closing quote from a
//! quote inside the string. The reference escaped it and the transpilers did
//! not.
//!
//! It survived because `_repr` is only reachable from a *container*: `(print
//! "a\"b")` prints through `_disp`, which is a different helper and was always
//! correct, and no example, fixture or corpus program put a quote or a
//! backslash inside a list.
//!
//! ## What is asserted, and why these cases
//!
//! The end-to-end contract — five runners, one program's bytes — is
//! `scripts/check-string-repr-parity.sh` plus
//! `fixtures/string_repr_parity.ainl`, which needs a release binary and the
//! three host runtimes. These tests cover the half that runs in the normal
//! `cargo test` pass: the *emitted* helper, per target. "Byte-identical" is
//! only meaningful if the emitted code is the code that gets run, and a test
//! that shells out to three interpreters to check one `.replace` chain is a
//! test that stops running the moment a host is missing.
//!
//! Three properties are pinned, because a fix that gets two of them is still
//! broken:
//!
//! 1. **The five characters.** `"`, `\`, newline, tab, CR — the exact table the
//!    C runtime's `value_repr` switches on. Escaping only `"` would fix the
//!    card's repro and leave a lone backslash rendering as one.
//! 2. **Backslash first.** Every target here rewrites with ordered passes, so
//!    the order is load-bearing: escaping anything else first means the
//!    backslash that pass introduced gets escaped again, turning `\\n` into
//!    `\\\n`. The `"\\\\n"` case is the one that catches a quote-first fix.
//! 3. **In `_repr`, not `_disp`.** `_disp` is the bare-string path and must stay
//!    untouched; a fix in the wrong helper still passes every case above.

use ainl_transpile::{transpile_js_src, transpile_python_src, transpile_ruby_src};

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("python transpile failed for `{src}`: {e}"))
}

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("js transpile failed for `{src}`: {e}"))
}

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("ruby transpile failed for `{src}`: {e}"))
}

/// The body of one emitted helper — the text from its signature to the next
/// top-level definition, so an assertion about `_repr` cannot be satisfied by
/// the same text in `_disp`, and vice versa.
///
/// Bounding matters more than it looks: slicing to end-of-file makes the
/// "dispatch" body of the last helper emitted swallow every helper after it,
/// which turns "this helper does X" into "somewhere after this helper, X
/// happens" and would have let the `_disp` test pass on `_repr`'s escaping.
fn helper_body(out: &str, signature: &str) -> String {
    let start = out
        .find(signature)
        .unwrap_or_else(|| panic!("no `{signature}` in:\n{out}"));
    let rest = &out[start..];
    // The next top-level `def `/`function ` at column 0 ends this body.
    let end = rest
        .lines()
        .skip(1)
        .position(|l| l.starts_with("def ") || l.starts_with("function "))
        .map(|i| {
            rest.lines()
                .take(i + 1)
                .map(|l| format!("{l}\n"))
                .collect::<String>()
                .len()
        })
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

/// A program whose only content is a container holding a quoted string — the
/// shortest thing that pulls `_repr` into the emitted output.
const PROGRAM: &str = r#"(print (str (list "a\"b")))"#;

/// Every target's emitted `_repr`, keyed the way the assertions want it.
fn emitted(target: &str) -> String {
    match target {
        "python" => helper_body(&py(PROGRAM), "def _repr("),
        "js" => helper_body(&js(PROGRAM), "function _repr("),
        "ruby" => helper_body(&rb(PROGRAM), "def _repr("),
        other => panic!("unknown target {other}"),
    }
}

const TARGETS: &[&str] = &["python", "js", "ruby"];

/// Each target must escape the double-quote. This is the card's repro, and the
/// one case where "did you fix it" and "did you fix it the intended way" are the
/// same question.
#[test]
fn the_repr_helper_escapes_a_double_quote() {
    for target in TARGETS {
        let body = emitted(target);
        assert!(
            body.contains("\\\"") || body.contains("\\\\\""),
            "{target}: _repr does not emit an escaped double-quote, so a quoted string \
             renders ambiguously:\n{body}"
        );
        // The old shape — wrap and concatenate with no escaping at all — must
        // be gone. Asserted as a whole so a re-implementation that escapes
        // somewhere else still passes.
        assert!(
            !body.contains(r#"'"' + x + '"'"#) && !body.contains(r#""\"" + x + "\"""#),
            "{target}: _repr still wraps the string with no escaping:\n{body}"
        );
    }
}

/// A backslash has to be escaped too. The card's repro is a quote, and a fix
/// that escapes only the quote leaves `(list "\\")` rendering as `("\")` — the
/// same ambiguity, one character down.
#[test]
fn the_repr_helper_escapes_a_backslash() {
    for target in TARGETS {
        let body = emitted(target);
        assert!(
            body.contains("\\\\"),
            "{target}: _repr does not escape a backslash:\n{body}"
        );
    }
}

/// The rest of the reference's table. The C runtime's `value_repr` switches on
/// exactly these five, and the interpreter (Rust's `{:?}`) agrees on all five.
/// A newline or tab emitted raw would break a line-oriented diff of the output,
/// which is how every parity gate in this repo compares.
#[test]
fn the_repr_helper_escapes_the_whole_reference_table() {
    for target in TARGETS {
        let body = emitted(target);
        for (name, needle) in [
            ("newline", "\\n"),
            ("tab", "\\t"),
            ("carriage return", "\\r"),
        ] {
            assert!(
                body.contains(needle),
                "{target}: _repr does not escape a {name}:\n{body}"
            );
        }
    }
}

/// Backslash first, and this is the assertion a quote-first fix fails.
///
/// The passes are ordered, so the one that escapes `\` has to run before the
/// ones that introduce backslashes. If a quote pass runs first, the backslash it
/// writes into `a"b` is then itself escaped, and the string renders as three
/// backslashes where the reference writes two.
#[test]
fn the_backslash_pass_comes_first_so_escapes_are_not_re_escaped() {
    for target in TARGETS {
        let body = emitted(target);
        // Locate each pass by the pattern it MATCHES on, then compare
        // positions. The match argument is the raw character, so it is a
        // literal `"` on all three targets and only the surrounding call syntax
        // differs: Python `.replace('"', …)`, JS `.replace(/"/g, …)`,
        // Ruby `.gsub('"')`. Matching on the *replacement* instead would be
        // wrong — every replacement starts with the same backslash, so it
        // could not tell the passes apart.
        let (backslash_pass, quote_pass) = match *target {
            "python" => (r#"replace('\\',"#, r#"replace('"',"#),
            "js" => (r#".replace(/\\/g,"#, r#".replace(/"/g,"#),
            "ruby" => (r#"gsub('\\')"#, r#"gsub('"')"#),
            other => panic!("unknown target {other}"),
        };
        let bi = body
            .find(backslash_pass)
            .unwrap_or_else(|| panic!("{target}: no backslash pass in _repr:\n{body}"));
        let qi = body
            .find(quote_pass)
            .unwrap_or_else(|| panic!("{target}: no quote pass in _repr:\n{body}"));
        assert!(
            bi < qi,
            "{target}: _repr escapes the quote before the backslash, so the backslash the \
             quote pass introduces is escaped again — \"\\\\n\" would render as three \
             backslashes where the reference writes two:\n{body}"
        );
    }
}

/// `_disp` is the bare-string path and must stay unescaped.
///
/// A fix that put the escaping in `_disp` instead of `_repr` would pass every
/// assertion above and break `(print "a\"b")`, which has to print `a"b`. Both
/// helpers are emitted for any program that prints a container, so this checks
/// the one that should be untouched really is.
#[test]
fn the_disp_helper_is_left_alone() {
    for target in TARGETS {
        let out = match *target {
            "python" => py(PROGRAM),
            "js" => js(PROGRAM),
            "ruby" => rb(PROGRAM),
            other => panic!("unknown target {other}"),
        };
        let disp = match *target {
            "python" => helper_body(&out, "def _disp("),
            "js" => helper_body(&out, "function _disp("),
            "ruby" => helper_body(&out, "def _disp("),
            other => panic!("unknown target {other}"),
        };
        // `_disp` delegates a string to the host's own `str`/`String`/`to_s`.
        // That is correct and unescaped: a bare string is shown as it is.
        let host_string_call = match *target {
            "python" => "return str(x)",
            "js" => "return String(x)",
            "ruby" => "x.to_s",
            other => panic!("unknown target {other}"),
        };
        assert!(
            disp.contains(host_string_call),
            "{target}: _disp no longer returns the host's own string form:\n{disp}"
        );
        // The escaping call itself must be absent — checked as the call, not
        // as a bare `\n`/`\t`, because a substring search for a backslash
        // escape matches ordinary source text (a Python literal `'\n'` appears
        // in the float branch of `_disp` for reasons unrelated to strings).
        // A `_repr` fix copied wholesale into `_disp` always brings the call
        // with it, so this catches the wrong-helper fix without the false
        // positive that made the earlier form of this assertion fail.
        let escaping_call = match *target {
            "python" => "replace('\"',",
            "js" => ".replace(/",
            "ruby" => "gsub(",
            other => panic!("unknown target {other}"),
        };
        assert!(
            !disp.contains(escaping_call),
            "{target}: _disp escapes, but it is the DISPLAY path — a bare string must print \
             as it is. The escaping belongs in _repr:\n{disp}"
        );
    }
}

/// The escaping has to be in the emitted source at all, on every target.
///
/// Structural rather than behavioural, and deliberately: a target that never
/// emits `_repr` for a program containing a quoted string in a container would
/// otherwise pass the tests above by omission.
#[test]
fn every_target_emits_a_repr_helper_for_a_quoted_string_in_a_container() {
    for target in TARGETS {
        let out = match *target {
            "python" => py(PROGRAM),
            "js" => js(PROGRAM),
            "ruby" => rb(PROGRAM),
            other => panic!("unknown target {other}"),
        };
        let signature = match *target {
            "python" | "ruby" => "def _repr(",
            "js" => "function _repr(",
            other => panic!("unknown target {other}"),
        };
        assert!(
            out.contains(signature),
            "{target}: no _repr helper was emitted for a container holding a quoted string:\n{out}"
        );
    }
}
