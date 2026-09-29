//! Python projection of the Tier 3 byte-oriented string primitives.
//!
//! The behavior contract — byte-identical output on all four backends — is
//! checked end-to-end by `scripts/check-byte-strings.sh` against
//! `tests/byte_strings_parity.ainl`. These tests cover the other half: that each
//! builtin maps to the *host's* byte idiom and that the helpers needed for that
//! mapping are actually emitted.
//!
//! The thing worth asserting is what the helpers must NOT do. Python's `str`
//! indexes by character, so `s[a:b]`, `s.find()` and `ord(s[i])` would each give
//! a different wrong answer; every helper has to go through `encode('utf-8')`
//! instead. A helper that reached for the host's own indexing would pass every
//! ASCII test and be wrong on the first multi-byte one.

use ainl_transpile::transpile_python_src;

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn the_six_builtins_map_to_their_helpers() {
    let out = py(r#"(do (print (substring "a" 0 1))
             (print (char "a" 0))
             (print (code "a"))
             (print (starts-with "a" "a"))
             (print (ends-with "a" "a"))
             (print (index-of "a" "a")))"#);
    for (call, helper) in [
        (r#"_substring("a", 0, 1)"#, "_substring"),
        (r#"_char("a", 0)"#, "_char"),
        (r#"_code("a")"#, "_code"),
        (r#"_starts_with("a", "a")"#, "_starts_with"),
        (r#"_ends_with("a", "a")"#, "_ends_with"),
        (r#"_index_of("a", "a")"#, "_index_of"),
    ] {
        assert!(out.contains(call), "expected {call} in:\n{out}");
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
}

#[test]
fn every_helper_works_on_bytes_rather_than_the_host_string() {
    let out = py(r#"(print (substring "a" 0 1))"#);
    // The shared byte-conversion helper is what makes this correct at all.
    assert!(
        out.contains("def _ainl_b("),
        "the byte helper must be emitted:\n{out}"
    );
    assert!(
        out.contains("encode('utf-8')"),
        "must convert to bytes - a Python str indexes by character:\n{out}"
    );
    // ...and the slice is decoded back, so a str comes out.
    assert!(
        out.contains("decode('utf-8')"),
        "the result must come back as a str:\n{out}"
    );
    // A character-indexed port would be slicing the str directly.
    assert!(
        !out.contains("def _substring(s, start, end):\n    return s[start:end]"),
        "_substring must not slice the host str:\n{out}"
    );
}

#[test]
fn the_boundary_and_index_helpers_are_pulled_in() {
    let out = py(r#"(do (print (substring "a" 0 1)) (print (index-of "a" "a")))"#);
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // A UTF-8 continuation byte cannot start or end a slice — this is the rule
    // that makes a split an error rather than mojibake.
    assert!(
        out.contains("0xC0") && out.contains("0x80"),
        "the boundary check must test for a continuation byte:\n{out}"
    );
    // index-of delegates to the byte string's own find, which is already a byte
    // offset and already answers 0 for an empty needle.
    assert!(
        out.contains("b.find(p)"),
        "index-of must search bytes:\n{out}"
    );
}

#[test]
fn every_failure_raises_through_error_so_catch_can_intercept_it() {
    let out = py(r#"(print (substring "a" 0 1))"#);
    // A host ValueError would escape the generated `except _AinlError` clause
    // and abort with a traceback, which is neither a caught value nor a shared
    // message.
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off"] {
        let start = out.find(&format!("def {helper}(")).expect(helper);
        let body = &out[start..];
        let end = body.find("\n\ndef ").unwrap_or(body.len());
        assert!(
            body[..end].contains("_error("),
            "{helper} must report through _error:\n{out}"
        );
    }
    assert!(
        out.contains("def _error("),
        "_error must be emitted:\n{out}"
    );
}
