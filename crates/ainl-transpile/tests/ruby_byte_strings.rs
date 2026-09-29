//! Ruby projection of the Tier 3 byte-oriented string primitives.
//!
//! The behavior contract — byte-identical output on all four backends — is
//! checked end-to-end by `scripts/check-byte-strings.sh` against
//! `tests/byte_strings_parity.ainl`. These tests cover the other half: that each
//! builtin maps to the host's *byte* idiom and that the helpers are emitted.
//!
//! The Ruby trap worth naming: `String#b` looks exactly right and is not. It
//! returns a binary-encoded **String**, not an Array of Integer bytes, so
//! `b[i]` is a one-character String and `b[i] & 0xC0` raises NoMethodError. The
//! helpers need `String#bytes` (an Array), because they index with a single
//! Integer and slice with a length. `_list_dir` uses `.b` for a different job —
//! a sort key — where a binary String is precisely correct.

use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn the_six_builtins_map_to_their_helpers() {
    let out = rb(r#"(do (print (substring "a" 0 1))
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
fn the_byte_helper_uses_bytes_not_the_b_string() {
    let out = rb(r#"(print (substring "a" 0 1))"#);
    assert!(
        out.contains("def _ainl_b("),
        "the byte helper must be emitted:\n{out}"
    );
    assert!(
        out.contains("s.bytes"),
        "must use String#bytes (an Array of Integer):\n{out}"
    );
    // `String#b` is the near-miss that returns a String, not an Array.
    assert!(
        !out.contains("\n  s.b\n"),
        "`s.b` returns a binary String, so `b[i] & 0xC0` raises:\n{out}"
    );
    // The slice is packed back into a String and tagged UTF-8.
    assert!(
        out.contains("pack('C*')") && out.contains("force_encoding('UTF-8')"),
        "the result must come back as a str:\n{out}"
    );
}

#[test]
fn the_boundary_index_and_search_helpers_are_pulled_in() {
    let out = rb(r#"(print (substring "a" 0 1))"#);
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off", "_ainl_seq_find"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // A UTF-8 continuation byte cannot start or end a slice.
    assert!(
        out.contains("0xC0") && out.contains("0x80"),
        "the boundary check must test for a continuation byte:\n{out}"
    );
    // Array#index matches a single Integer, so a multi-byte needle needs the
    // sliding scanner. This is the one helper Ruby cannot inline.
    assert!(
        out.contains("def _ainl_seq_find("),
        "index-of needs the byte-sequence scanner:\n{out}"
    );
}

#[test]
fn substring_does_not_use_end_as_a_parameter_name() {
    // `end` is a Ruby keyword: a helper with `def _substring(s, start, end)`
    // is a SyntaxError, and the transpiled file would not even parse. The
    // emitted helper renames the bound and keeps AINL's own wording in the
    // messages.
    let out = rb(r#"(print (substring "a" 0 1))"#);
    assert!(
        !out.contains("def _substring(s, start, end)"),
        "`end` is a Ruby keyword and cannot be a parameter name:\n{out}"
    );
    assert!(
        out.contains("substring start index is greater than end index"),
        "the shared message must survive the rename:\n{out}"
    );
}

#[test]
fn every_failure_goes_through_error_so_catch_can_intercept_it() {
    let out = rb(r#"(print (substring "a" 0 1))"#);
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off"] {
        let start = out
            .find(&format!("def {helper}("))
            .unwrap_or_else(|| panic!("missing {helper}:\n{out}"));
        let body = &out[start..];
        let end = body.find("\nend").unwrap_or(body.len());
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
