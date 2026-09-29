//! The six byte-oriented string primitives — the two evaluators, and the edges.
//!
//! The 4-backend rule makes this a *parity* test first and a *behaviour* test
//! second. `run_str` is the bytecode VM and `run_in_tree_walk` is the
//! tree-walking evaluator; both reach the same `BuiltinFn` through different
//! entry points, so a bug in either is invisible to a single-backend test. The
//! AOT and transpiler halves live in the sibling crates (`aot_stdlib.rs`, the
//! `*_stdlib.rs` transpile tests) and in `scripts/check-byte-strings.sh`.
//!
//! The cases are chosen so that a *character*-indexed implementation — the
//! natural one in every host language — fails loudly. `"héllo"` is 6 bytes and
//! 5 characters, so an off-by-one-character answer is a wrong number, not a
//! coincidence.

use ainl_core::{run_in_tree_walk, run_str};

fn vm(src: &str) -> String {
    match run_str(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

fn tree(src: &str) -> String {
    match run_in_tree_walk(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// Assert both evaluators produce the same printed value AND that it is
/// `want`, so a test cannot pass by both backends being wrong together.
fn both(src: &str, want: &str) {
    assert_eq!(vm(src), want, "VM for `{src}`");
    assert_eq!(tree(src), want, "tree-walk for `{src}`");
}

#[test]
fn substring_slices_by_byte_and_takes_an_exclusive_end() {
    both(r#"(substring "abcdef" 0 6)"#, "abcdef");
    both(r#"(substring "abcdef" 2 4)"#, "cd");
    // Both boundaries are legal, and the empty slice is a value, not an error.
    both(r#"(substring "abcdef" 0 0)"#, "");
    both(r#"(substring "abcdef" 6 6)"#, "");
    both(r#"(substring "abcdef" 3 3)"#, "");
}

#[test]
fn substring_and_index_of_report_byte_offsets_not_character_offsets() {
    // "héllo" = h(1) é(2) l(1) l(1) o(1) = 6 bytes, 5 characters.
    // A character index would answer 2 here; the byte index is 3.
    both(r#"(index-of "héllo" "llo")"#, "3");
    both(r#"(substring "héllo" 1 3)"#, "é");
    // Both ends on boundaries: bytes 1,2,3 = "é" + "l".
    both(r#"(substring "héllo" 1 4)"#, "él");
    // A 2-byte slice is ONE character, which is what `len` still counts.
    both(r#"(len (substring "héllo" 1 3))"#, "1");
    // "日本" is 6 bytes: the second character starts at byte 3.
    both(r#"(index-of "日本" "本")"#, "3");
}

#[test]
fn char_returns_a_whole_character_and_skips_by_byte() {
    both(r#"(char "abc" 0)"#, "a");
    both(r#"(char "abc" 2)"#, "c");
    // The whole 3-byte character, never half of it.
    both(r#"(char "日本" 0)"#, "日");
    both(r#"(char "日本" 3)"#, "本");
    // A 4-byte sequence (an emoji) too — the lead byte gives the length.
    both(r#"(char "a😀b" 1)"#, "😀");
}

#[test]
fn code_reads_a_raw_byte_including_a_continuation_byte() {
    both(r#"(code "A")"#, "65");
    both(r#"(code "A" 0)"#, "65");
    // "é" is 0xC3 0xA9; "日" is 0xE6 0x97 0xA5.
    both(r#"(code "é")"#, "195");
    both(r#"(code "é" 1)"#, "169");
    both(r#"(code "日本" 0)"#, "230");
    // Unlike `char`, `code` will hand back a continuation byte: it is the
    // deliberate escape hatch.
    both(r#"(code "日本" 1)"#, "151");
    both(r#"(code "日本" 2)"#, "165");
}

#[test]
fn starts_with_and_ends_with_compare_bytes() {
    both(r#"(starts-with "hello" "he")"#, "true");
    both(r#"(starts-with "hello" "lo")"#, "false");
    both(r#"(ends-with "hello" "lo")"#, "true");
    both(r#"(ends-with "hello" "he")"#, "false");
    // An empty needle is contained in anything, so both are true — the same
    // rule `contains` already follows.
    both(r#"(starts-with "hello" "")"#, "true");
    both(r#"(ends-with "hello" "")"#, "true");
    both(r#"(starts-with "日本" "")"#, "true");
    // A needle longer than the haystack is false, not a bounds error.
    both(r#"(starts-with "ab" "abc")"#, "false");
    both(r#"(ends-with "ab" "abc")"#, "false");
}

#[test]
fn index_of_finds_the_first_occurrence_and_uses_zero_for_an_empty_needle() {
    both(r#"(index-of "hello" "llo")"#, "2");
    both(r#"(index-of "banana" "na")"#, "2");
    both(r#"(index-of "aaaa" "aa")"#, "0");
    both(r#"(index-of "hello" "z")"#, "-1");
    both(r#"(index-of "" "x")"#, "-1");
    // The empty needle is at offset 0 — which is also what keeps this
    // consistent with `contains`, whose empty-needle answer is `true`.
    both(r#"(index-of "hello" "")"#, "0");
    both(r#"(index-of "" "")"#, "0");
}

#[test]
fn a_slice_that_would_split_a_character_is_an_error() {
    // There is no Value that can hold half a character, and the alternative —
    // inventing a replacement character — is what `read-file` already refuses
    // to do for the same reason.
    for src in [
        r#"(substring "héllo" 0 2)"#,
        r#"(char "日本" 1)"#,
        r#"(substring "😀" 0 1)"#,
    ] {
        let msg = vm(src);
        assert!(
            msg.contains("splits a multi-byte character"),
            "VM for `{src}` should reject a split, got: {msg}"
        );
        assert_eq!(msg, tree(src), "the two evaluators must agree on `{src}`");
    }
}

#[test]
fn out_of_range_bounds_are_errors_rather_than_clamped() {
    // Clamping would make `(substring s 0 999)` quietly succeed and turn an
    // off-by-one in a caller's arithmetic into a silently wrong string.
    for (src, want) in [
        (
            r#"(substring "abc" -1 2)"#,
            "substring start index out of bounds",
        ),
        (
            r#"(substring "abc" 0 99)"#,
            "substring end index out of bounds",
        ),
        (
            r#"(substring "abc" 3 1)"#,
            "substring start index is greater than end index",
        ),
        (r#"(char "abc" 3)"#, "char index out of bounds"),
        (r#"(code "abc" 9)"#, "code index out of bounds"),
        (r#"(code "")"#, "code expects a non-empty string"),
    ] {
        let msg = vm(src);
        assert!(
            msg.contains(want),
            "VM for `{src}` expected {want:?}, got: {msg}"
        );
        assert_eq!(msg, tree(src), "the two evaluators must agree on `{src}`");
    }
}

#[test]
fn a_float_index_is_rejected_rather_than_truncated() {
    // Every host disagrees about what `(substring s 1.5 2)` means, so a
    // truncating implementation would make a caller's arithmetic bug invisible
    // on some backends and fatal on others.
    for (src, want) in [
        (
            r#"(substring "abc" 0 1.5)"#,
            "substring expects an int end index, got float",
        ),
        (
            r#"(substring "abc" 1.5 2)"#,
            "substring expects an int start index, got float",
        ),
        (
            r#"(char "abc" 1.5)"#,
            "char expects an int index, got float",
        ),
        (
            r#"(code "abc" 1.5)"#,
            "code expects an int index, got float",
        ),
    ] {
        let msg = vm(src);
        assert!(
            msg.contains(want),
            "VM for `{src}` expected {want:?}, got: {msg}"
        );
        assert_eq!(msg, tree(src), "the two evaluators must agree on `{src}`");
    }
}

#[test]
fn a_non_string_operand_is_rejected_under_the_calling_builtin() {
    for (src, want) in [
        (r#"(substring 1 0 2)"#, "substring expects a str, got int"),
        (r#"(char 1 0)"#, "char expects a str, got int"),
        (r#"(code 1)"#, "code expects a str, got int"),
        (
            r#"(starts-with "a" 1)"#,
            "starts-with expects a str, got int",
        ),
        (r#"(ends-with 1 "a")"#, "ends-with expects a str, got int"),
        (r#"(index-of 1 "a")"#, "index-of expects a str, got int"),
    ] {
        let msg = vm(src);
        assert!(
            msg.contains(want),
            "VM for `{src}` expected {want:?}, got: {msg}"
        );
        assert_eq!(msg, tree(src), "the two evaluators must agree on `{src}`");
    }
}

/// The composition the card was written for: a tokenizer that can find where a
/// call opens and closes. Before these primitives the only way to look inside a
/// string was to `replace` every paren with a space and re-`split`, which cannot
/// report a position.
#[test]
fn the_new_primitives_compose_into_a_tokenizer() {
    both(r#"(index-of "(print 42)" "(")"#, "0");
    both(
        r#"(substring "(print 42)" (+ (index-of "(print 42)" "(") 1) (index-of "(print 42)" ")"))"#,
        "print 42",
    );
    both(r#"(ends-with "(print 42)" ")")"#, "true");
    both(r#"(starts-with "(print 42)" "(")"#, "true");
    both(
        r#"(code (char "(print 42)" (index-of "(print 42)" "(")))"#,
        "40",
    );
    // The offset is a BYTE offset, so a multi-byte operand before the delimiter
    // still puts the delimiter at the right place.
    both(r#"(index-of "é = x" "=")"#, "3");
}
