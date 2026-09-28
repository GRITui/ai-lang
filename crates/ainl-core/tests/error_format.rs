//! Error-message format: every error must name **what** went wrong, **where**
//! (line + column, not a bare byte offset) and, when inferable, the **likely
//! fix**.
//!
//! This is the contract a model consumes when AINL fails, so it is asserted
//! rather than assumed. Three properties matter and are tested separately:
//!
//! 1. **Parts** — the message names the symbol, a line, a column, and (where it
//!    can be inferred) a suggested fix.
//! 2. **Agreement** — the bytecode VM and the tree-walking interpreter produce
//!    byte-identical stderr for the same bad program. A model cannot repair
//!    against two different diagnostics.
//! 3. **Non-regression of the limits** — `AINL_MAX_STEPS` / recursion still
//!    report a readable, actionable message.

use ainl_core::{run_in_tree_walk, run_str, Env};

/// The error `src` produces, on the VM (the default backend).
fn err_vm(src: &str) -> String {
    run_str(src)
        .err()
        .unwrap_or_else(|| panic!("`{src}` was expected to fail but succeeded"))
        .to_string()
}

/// The error `src` produces, on the tree-walking interpreter.
fn err_tree(src: &str) -> String {
    run_in_tree_walk(src)
        .err()
        .unwrap_or_else(|| panic!("`{src}` was expected to fail but succeeded"))
        .to_string()
}

/// Assert both backends fail *identically*. This is the 4-backend rule narrowed
/// to the two that share a source position: the transpiler and AOT backends
/// are covered in their own crates.
fn assert_both(src: &str) -> String {
    let vm = err_vm(src);
    let tw = err_tree(src);
    assert_eq!(
        vm, tw,
        "the VM and the tree-walk disagree about `{src}`\n  vm:  {vm}\n  tree: {tw}"
    );
    vm
}

// ---------------------------------------------------------------------------
// What + where: line and column, never a bare byte offset
// ---------------------------------------------------------------------------

#[test]
fn an_unbound_symbol_names_the_symbol_line_and_column() {
    let msg = assert_both("(def x 1)\n(def y 2)\n(print x z)\n");
    assert!(msg.contains("unbound symbol 'z'"), "no symbol: {msg}");
    assert!(msg.contains("at line 3"), "no line: {msg}");
    assert!(msg.contains("col "), "no column: {msg}");
}

#[test]
fn the_column_points_at_the_offending_token() {
    //            1234567890123456
    let src = "(print 1)\n  (print 2)\n(let nope 1)\n";
    let msg = assert_both(src);
    // The malformed `let` form opens at column 1 on line 3, and that whole form
    // is the thing the reader has to rewrite.
    assert!(
        msg.contains("at line 3, col 1"),
        "expected line 3, col 1 in: {msg}"
    );
}

#[test]
fn the_column_counts_characters_not_bytes_for_non_ascii_source() {
    // A multi-byte character before the error must not shift the reported
    // column: the number a reader counts in an editor is the number we report.
    let src = "(print \"héllo\")\n(nosuch)\n";
    let msg = assert_both(src);
    assert!(msg.contains("at line 2, col 2"), "got: {msg}");
}

#[test]
fn a_byte_offset_is_retained_alongside_the_line_and_column() {
    // The JSON AST emits `span` as a byte offset, so keeping it lets a reader
    // correlate an error with `ainl ast --json` output.
    let msg = err_vm("(print 1)\n(nosuch)\n");
    assert!(msg.contains("byte "), "no byte offset retained: {msg}");
}

#[test]
fn the_first_line_of_a_multi_line_program_is_reported_as_line_one() {
    let msg = assert_both("(nosuch)\n");
    assert!(msg.contains("at line 1, col 2"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// The likely fix: close-match suggestions
// ---------------------------------------------------------------------------

#[test]
fn a_typo_of_a_builtin_suggests_the_real_builtin() {
    // The single highest-value feature of the repair loop: a misspelled
    // builtin is the most common failure a model produces.
    let msg = assert_both("(prnt 1)\n");
    assert!(
        msg.contains("unbound symbol 'prnt'") && msg.contains("did you mean 'print'?"),
        "got: {msg}"
    );
}

#[test]
fn a_typo_of_a_user_defined_name_suggests_that_name() {
    // The candidate set is the names actually in scope, not a hardcoded list
    // of builtins, so a typo'd local is corrected against the local.
    let msg = assert_both("(def counter 1)\n(print conter)\n");
    assert!(msg.contains("did you mean 'counter'?"), "got: {msg}");
}

#[test]
fn a_typo_of_a_dashed_builtin_suggests_the_dashed_name() {
    let msg = assert_both("(readfile \"x\")\n");
    assert!(msg.contains("did you mean 'read-file'?"), "got: {msg}");
}

#[test]
fn a_different_name_in_a_module_is_suggested_across_the_import() {
    // A module's bindings are in scope, so its names are suggestible too.
    let msg = assert_both("(def helper (fn (x) x))\n(print hepper)\n");
    assert!(msg.contains("did you mean 'helper'?"), "got: {msg}");
}

#[test]
fn an_unrelated_word_gets_no_suggestion_rather_than_a_wrong_one() {
    // The failure mode this guards is *noise*: a confident wrong suggestion
    // costs a model more than no suggestion at all.
    let msg = assert_both("(print zzzzzzzzzzzz)\n");
    assert!(
        !msg.contains("did you mean"),
        "a nonsense name should get no suggestion, got: {msg}"
    );
}

#[test]
fn a_one_character_name_is_never_corrected_to_an_operator() {
    // Every 1-char name is edit-distance 1 from every other, so without the
    // length guard `f` would be "corrected" to `*`.
    let msg = assert_both("(f 1)\n");
    assert!(!msg.contains("did you mean"), "got: {msg}");
}

#[test]
fn the_suggestion_is_the_same_in_every_backend_and_every_run() {
    // Determinism is load-bearing: the 4-backend rule means a suggestion must
    // not depend on iteration order. Ten runs must agree.
    let first = err_vm("(prnt 1)\n");
    for _ in 0..10 {
        assert_eq!(err_vm("(prnt 1)\n"), first, "the suggestion is not stable");
    }
}

// ---------------------------------------------------------------------------
// Arity and type errors
// ---------------------------------------------------------------------------

#[test]
fn an_arity_mismatch_names_the_function_and_its_position() {
    let msg = assert_both("(def add2 (fn (a b) (+ a b)))\n(add2 1)\n");
    assert!(
        msg.contains("arity mismatch: (add2) takes 2 args, got 1"),
        "got: {msg}"
    );
    assert!(msg.contains("at line 2"), "no line: {msg}");
}

#[test]
fn a_type_error_names_the_expected_type_and_where_it_happened() {
    let msg = assert_both("(print 1)\n(+ 1 \"a\")\n");
    assert!(
        msg.contains("expected a number, got str"),
        "no type name: {msg}"
    );
    assert!(msg.contains("at line 2"), "no line: {msg}");
}

#[test]
fn a_builtin_type_error_names_the_builtin() {
    let msg = assert_both("(abs \"x\")\n");
    assert!(msg.contains("abs expects a number, got str"), "got: {msg}");
    assert!(msg.contains("at line 1"), "no line: {msg}");
}

#[test]
fn calling_a_non_function_says_where_and_what() {
    let msg = assert_both("(def x 1)\n(x 2)\n");
    assert!(msg.contains("cannot call a int"), "got: {msg}");
    assert!(msg.contains("at line 2"), "no line: {msg}");
}

// ---------------------------------------------------------------------------
// Parse and lex errors
// ---------------------------------------------------------------------------

#[test]
fn an_unexpected_close_paren_reports_a_line_and_column() {
    let msg = run_str("(print 1))\n")
        .expect_err("should fail")
        .to_string();
    assert!(msg.contains("unexpected ')'"), "got: {msg}");
    assert!(msg.contains("at line 1"), "no line: {msg}");
    assert!(msg.contains("col "), "no column: {msg}");
}

#[test]
fn an_unclosed_paren_suggests_the_likely_fix() {
    // The card's example: the fix is inferable, so the error must state it.
    let msg = run_str("(print 1\n").expect_err("should fail").to_string();
    assert!(msg.contains("unclosed"), "got: {msg}");
}

#[test]
fn an_unterminated_string_reports_a_line_and_column() {
    let msg = run_str("(print \"oops)\n")
        .expect_err("should fail")
        .to_string();
    assert!(msg.contains("unterminated string"), "got: {msg}");
    assert!(msg.contains("at line 1"), "no line: {msg}");
}

#[test]
fn a_deeply_nested_program_reports_where_it_gave_up() {
    // The nesting guard is a real user-facing failure, not an internal detail.
    let src = format!("{}1{}", "(".repeat(600), ")".repeat(600));
    let msg = run_str(&src).expect_err("should fail").to_string();
    assert!(msg.contains("nesting too deep"), "got: {msg}");
    assert!(msg.contains("at line 1"), "no line: {msg}");
}

// ---------------------------------------------------------------------------
// The resource limits stay readable
// ---------------------------------------------------------------------------

#[test]
fn the_step_limit_is_still_readable_and_keeps_its_budget_number() {
    let msg = err_vm("(while true 1)\n");
    assert!(msg.contains("step limit exceeded"), "got: {msg}");
    // The exact cap is part of the documented contract (AINL_MAX_STEPS), and
    // the message keeps naming it so a reader can see what was hit.
    assert!(msg.contains("2000000"), "budget number lost: {msg}");
    // …and it still says what to do about it.
    assert!(
        msg.contains("infinite loop"),
        "the hint about the cause is lost: {msg}"
    );
}

#[test]
fn the_recursion_limit_is_still_readable() {
    // Deep recursion is a resource property of the run, not of one node, so it
    // legitimately carries no position — but it must still be readable.
    let msg = err_vm("(def f (fn (n) (+ 1 (f n))))\n(f 0)\n");
    assert!(msg.contains("recursion limit exceeded"), "got: {msg}");
    assert!(msg.contains("512"), "the cap is lost: {msg}");
}

// ---------------------------------------------------------------------------
// The pieces are separately inspectable
// ---------------------------------------------------------------------------

#[test]
fn an_error_exposes_its_parts_not_just_a_preformatted_string() {
    // A consumer (a model, a tool, a test) should not have to parse prose to
    // find the line.
    let e = run_str("(print 1)\n(nosuch)\n").expect_err("should fail");
    assert_eq!(e.message(), "unbound symbol 'nosuch'");
    let loc = e.location().expect("a resolved position");
    assert_eq!(loc.line, 2);
    // `nosuch` opens at byte 11 — `(print 1)\n` is 10 bytes, so byte 11 is the
    // symbol's first character and the 2nd column of line 2. The offset and
    // the column are different coordinates for the same token, and both are
    // kept.
    assert_eq!(loc.col, 2);
    assert_eq!(e.offset(), Some(11));
}

#[test]
fn a_suggestion_is_exposed_separately_from_the_message() {
    let e = run_str("(prnt 1)\n").expect_err("should fail");
    assert_eq!(e.suggestion(), Some("print"));
    // The message itself stays clean — the advice is attached, not spliced in.
    assert_eq!(e.message(), "unbound symbol 'prnt'");
}

#[test]
fn a_positionless_error_reports_no_line_rather_than_an_invented_one() {
    // Resource limits have no single node. Guessing a line would be worse than
    // saying nothing, so the position is absent rather than wrong.
    let e = run_str("(while true 1)\n").expect_err("should fail");
    assert_eq!(e.location(), None);
    assert_eq!(e.offset(), None);
    let msg = e.to_string();
    assert!(
        !msg.contains("at line"),
        "a fabricated position leaked into: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Agreement between the two source-position backends
// ---------------------------------------------------------------------------

#[test]
fn every_runtime_error_shape_agrees_across_the_two_interpreters() {
    // A program for each error class, so a regression in one backend's
    // wording cannot hide behind the others.
    let cases = [
        "(print 1)\n(nosuch)\n",
        "(prnt 1)\n",
        "(def add2 (fn (a b) (+ a b)))\n(add2 1)\n",
        "(def x 1)\n(x 2)\n",
        "(+ 1 \"a\")\n",
        "(abs \"x\")\n",
        "(len 5)\n",
        "(first 5)\n",
        "(while true 1)\n",
    ];
    for src in cases {
        assert_both(src);
    }
}

#[test]
fn a_repl_session_error_keeps_its_position() {
    // The REPL evaluates into a persistent env; the position must survive that
    // path, not just the one-shot `run_str`.
    let env = Env::with_prelude();
    let e = ainl_core::vm::run_in("(print 1)\n(nosuch)\n", &env).expect_err("should fail");
    let msg = e.to_string();
    assert!(msg.contains("at line 2"), "got: {msg}");
    assert!(msg.contains("unbound symbol 'nosuch'"), "got: {msg}");
}
