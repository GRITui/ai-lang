//! JS projection of the Tier 3 byte-oriented string primitives.
//!
//! The behavior contract — byte-identical output on all four backends — is
//! checked end-to-end by `scripts/check-byte-strings.sh` against
//! `tests/byte_strings_parity.ainl`. These tests cover the other half: that each
//! builtin maps to the host's *byte* idiom and that the helpers are emitted.
//!
//! JS is the most dangerous of the three hosts here, and the reason is worth
//! stating: a JS string is a sequence of UTF-16 *code units*. `s.slice(a, b)`
//! slices code units, `s.indexOf` returns a code-unit offset, and
//! `s.charCodeAt(i)` will happily return half a surrogate pair. A port that
//! reached for any of those would pass every ASCII test and be wrong on the
//! first non-BMP character. So every helper goes through `Buffer` — the same
//! reasoning as `_list_dir`, which already sorts by `Buffer.compare` to match
//! the interpreter's byte order.

use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn the_six_builtins_map_to_their_helpers() {
    let out = js(r#"(do (print (substring "a" 0 1))
             (print (char "a" 0))
             (print (code "a"))
             (print (starts-with "a" "a"))
             (print (ends-with "a" "a"))
             (print (index-of "a" "a")))"#);
    for (call, helper) in [
        // Int indices are BigInt literals (`0n`), so the boundary helper
        // converts them back to a host number at the slice.
        (r#"_substring("a", 0n, 1n)"#, "_substring"),
        (r#"_char("a", 0n)"#, "_char"),
        (r#"_code("a")"#, "_code"),
        (r#"_starts_with("a", "a")"#, "_starts_with"),
        (r#"_ends_with("a", "a")"#, "_ends_with"),
        (r#"_index_of("a", "a")"#, "_index_of"),
    ] {
        assert!(out.contains(call), "expected {call} in:\n{out}");
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
}

#[test]
fn every_helper_works_on_a_buffer_rather_than_the_host_string() {
    let out = js(r#"(print (substring "a" 0 1))"#);
    assert!(
        out.contains("function _ainl_b("),
        "the byte helper must be emitted:\n{out}"
    );
    // Buffer.from(s, "utf8") is the only way to get a byte view of a JS string.
    assert!(
        out.contains(r#"Buffer.from(s, "utf8")"#),
        "must convert to a Buffer:\n{out}"
    );
    // ...and the slice is decoded back to a str.
    assert!(
        out.contains(r#"toString("utf8")"#),
        "the result must come back as a str:\n{out}"
    );
    // The code-unit traps, named explicitly so a future edit cannot reintroduce
    // one without failing here.
    for trap in [r#"s.slice(start, end)"#, "charCodeAt("] {
        assert!(
            !out.contains(trap),
            "a code-unit API ({trap}) must not appear in the byte helpers:\n{out}"
        );
    }
}

#[test]
fn the_boundary_and_index_helpers_are_pulled_in() {
    let out = js(r#"(print (substring "a" 0 1))"#);
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off"] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // A UTF-8 continuation byte cannot start or end a slice.
    assert!(
        out.contains("0xC0") && out.contains("0x80"),
        "the boundary check must test for a continuation byte:\n{out}"
    );
    // A float index must be refused. The test is `typeof i !== "bigint"` and
    // NOT `Number.isInteger`: an AINL int is a BigInt here, so
    // `Number.isInteger` would reject every VALID index while accepting a
    // whole float — the exact opposite of the rule. (Before the BigInt switch
    // `Number.isInteger` was the right test here, because JS had one number
    // type; the tag/float split moved it.)
    assert!(
        out.contains(r#"typeof i !== "bigint""#),
        "a float index must be rejected on this target:\n{out}"
    );
    assert!(
        !out.contains("Number.isInteger"),
        "Number.isInteger cannot classify a BigInt index:\n{out}"
    );
}

#[test]
fn every_failure_goes_through_error_so_catch_can_intercept_it() {
    let out = js(r#"(print (substring "a" 0 1))"#);
    for helper in ["_ainl_b", "_ainl_idx", "_ainl_off"] {
        let start = out
            .find(&format!("function {helper}("))
            .unwrap_or_else(|| panic!("missing {helper}:\n{out}"));
        let body = &out[start..];
        let end = body.find("\n\nfunction ").unwrap_or(body.len());
        assert!(
            body[..end].contains("_error("),
            "{helper} must report through _error:\n{out}"
        );
    }
    assert!(
        out.contains("function _error("),
        "_error must be emitted:\n{out}"
    );
}
