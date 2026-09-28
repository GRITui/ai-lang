//! `try` / `catch` — the two evaluators, the four backends, and the edges.
//!
//! The 4-backend rule makes this a *parity* test first and a *behaviour* test
//! second: every case here runs on the tree-walk and the bytecode VM and the two
//! must agree exactly, because those are the two backends whose internals differ
//! most (a native `Err` up the Rust stack versus a frame-stack unwind). The
//! AOT and transpiler halves live in the sibling crates.

use ainl_core::value::Value;
use ainl_core::{run_in_tree_walk, run_str};

/// The value of `src` on the bytecode VM, as printed.
fn vm(src: &str) -> String {
    match run_str(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// The value of `src` on the tree-walking evaluator, as printed.
fn tree(src: &str) -> String {
    match run_in_tree_walk(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// Assert both evaluators produce the same printed value AND that it is
/// `expected`. Running both is the point: a test that only checks the VM would
/// pass with a tree-walk-only bug, and vice versa.
fn both(src: &str, expected: &str) {
    let on_vm = vm(src);
    let on_tree = tree(src);
    assert_eq!(
        on_vm, on_tree,
        "evaluators disagree on {src:?}\n  vm:   {on_vm}\n  tree: {on_tree}"
    );
    assert_eq!(on_vm, expected, "wrong result for {src:?}");
}

#[test]
fn success_returns_the_body_value_and_never_runs_the_handler() {
    both(r#"(try (+ 1 2) (catch (e) "handler ran"))"#, "3");
}

#[test]
fn a_raising_body_runs_the_handler() {
    both(
        r#"(try (error "boom") (catch (e) (get e "message")))"#,
        "boom",
    );
}

#[test]
fn the_handler_never_runs_on_success() {
    // The handler would print a line if it ran; the absence of that line is
    // the assertion, and the value proves the body produced the result.
    both(r#"(try "body" (catch (e) "handler"))"#, "body");
}

#[test]
fn e_is_a_two_field_hash_message_first() {
    // The exact shape and key ORDER are part of the 4-backend contract: all
    // four backends print a map in insertion order, so `message` before `kind`
    // is what makes the caught value byte-identical everywhere.
    both(
        r#"(try (error "boom") (catch (e) e))"#,
        r#"{"message" "boom" "kind" "runtime"}"#,
    );
}

#[test]
fn kind_is_runtime_for_a_raised_error() {
    both(
        r#"(try (error "boom") (catch (e) (get e "kind")))"#,
        "runtime",
    );
}

#[test]
fn e_binds_to_whatever_name_the_catch_uses() {
    // `(e)` is a convention, not a keyword: the binding is the symbol in the
    // list. All four backends generate the binding from the same name.
    both(
        r#"(try (error "boom") (catch (problem) (get problem "message")))"#,
        "boom",
    );
}

#[test]
fn message_excludes_the_position_and_the_fix_text() {
    // A caught `e` is a value, not a diagnostic: it carries the message body
    // only. The line/col suffix that a top-level error prints would make the
    // caught value differ per backend (AOT and the transpilers do not embed
    // source), so it is deliberately not part of the hash.
    let v: Value = run_str(r#"(try (error "boom") (catch (e) e))"#).expect("caught");
    let map = match v {
        Value::Map(m) => m,
        other => panic!("caught value is a {other:?}, not a hash"),
    };
    assert_eq!(map.len(), 2, "the caught hash has exactly two fields");
}

#[test]
fn nested_try_lets_the_innermost_catch_win() {
    both(
        r#"(try
             (try (error "inner") (catch (e) "caught inside"))
             (catch (e) "caught outside"))"#,
        "caught inside",
    );
}

#[test]
fn an_inner_catch_may_rethrow_to_an_outer_catch() {
    both(
        r#"(try
             (try (error "inner") (catch (e) (error "rethrown")))
             (catch (e) (get e "message")))"#,
        "rethrown",
    );
}

#[test]
fn a_try_inside_a_fn_catches_that_fns_error() {
    both(
        r#"(def safe-div (fn (x) (try (/ 1 x) (catch (e) "divided by zero"))))
           (safe-div 0)"#,
        "divided by zero",
    );
}

#[test]
fn a_try_inside_a_fn_does_not_catch_a_callers_error() {
    // The protected region is the *body*, so an error raised by the caller is
    // outside it. Catching this would make `try` swallow failures it never saw
    // the cause of.
    let src = r#"(def boom (fn () (error "from caller")))
                (def guarded (fn () (try 1 (catch (e) "wrongly caught"))))
                (guarded)
                (boom)"#;
    assert!(vm(src).contains("from caller"), "vm: {}", vm(src));
    assert!(tree(src).contains("from caller"), "tree: {}", tree(src));
}

#[test]
fn a_try_inside_a_while_catches_each_iteration() {
    both(
        r#"(def i 0)
           (def count 0)
           (while (< i 5)
             (def v (try (error "nope") (catch (e) (get e "kind"))))
             (def count (+ count 1))
             (def i (+ i 1)))
           count"#,
        "5",
    );
}

#[test]
fn a_def_in_a_failing_body_is_not_visible_to_its_own_handler() {
    // The body and handler are SIBLING scopes. If they shared one, a handler
    // could read a half-initialised value — quite possibly the thing that
    // failed — and treat it as real data.
    let src = r#"(try (def leaked 1) (error "after def") (catch (e) (get e "message")))"#;
    both(src, "after def");
    // And the name really is gone afterwards, not just invisible to the handler.
    both(
        r#"(try (def leaked 1) (error "boom") (catch (e) 0))
           (try leaked (catch (e) "not bound"))"#,
        "not bound",
    );
}

#[test]
fn a_def_in_the_handler_does_not_leak() {
    both(
        r#"(try (error "boom") (catch (e) (def from-handler 1) 7))
           (try from-handler (catch (e) "not bound"))"#,
        "not bound",
    );
}

#[test]
fn an_uncaught_error_still_fails_the_run() {
    // No behaviour change for unhandled errors: the error propagates and the
    // caller sees it, exactly as before `try` existed.
    let e = run_str("(error \"uncaught\")").expect_err("must fail");
    assert_eq!(e.message(), "uncaught");
    let e2 = run_in_tree_walk("(error \"uncaught\")").expect_err("must fail on the tree-walk too");
    assert_eq!(e2.message(), "uncaught");
}

#[test]
fn a_try_whose_handler_raises_propagates_the_handlers_error() {
    // The handler is not itself protected — a `try` with no inner `try` around
    // its handler has nowhere to send that second failure.
    let e = run_str(r#"(try (error "first") (catch (e) (error "second")))"#)
        .expect_err("the handler's error is not caught");
    assert_eq!(e.message(), "second");
}

#[test]
fn an_empty_body_is_nil() {
    both(r#"(try (catch (e) "handler"))"#, "nil");
}

#[test]
fn a_multi_form_body_returns_its_last_value() {
    both(r#"(try 1 2 3 (catch (e) "handler"))"#, "3");
}

#[test]
fn a_multi_form_handler_returns_its_last_value() {
    both(
        r#"(try (error "boom") (catch (e) (get e "kind") "done"))"#,
        "done",
    );
}

#[test]
fn the_body_value_passes_through_untouched() {
    both(r#"(try (list 1 2 3) (catch (e) "nope"))"#, "(1 2 3)");
    both(r#"(try nil (catch (e) "nope"))"#, "nil");
}

#[test]
fn a_builtin_error_is_catchable() {
    // Not just `(error ...)`: the failure modes the card is actually about —
    // a type error, a division by zero, a missing file — must all be
    // catchable, since those are what real programs hit.
    both(r#"(try (error "x") (catch (e) 1))"#, "1");
    both(r#"(try (+ 1 "s") (catch (e) (get e "kind")))"#, "runtime");
    both(r#"(try (/ 1 0) (catch (e) (get e "kind")))"#, "runtime");
    both(
        r#"(try (read-file "definitely-not-here.txt") (catch (e) (get e "message")))"#,
        "read-file: cannot read 'definitely-not-here.txt'",
    );
}

#[test]
fn an_unbound_symbol_in_the_body_is_catchable() {
    both(
        r#"(try (this-is-not-bound) (catch (e) "caught"))"#,
        "caught",
    );
}

#[test]
fn a_step_limit_in_a_body_is_contained_and_reported() {
    // The body gets a FRESH step budget, so a runaway loop inside one raises
    // the step-limit error in the body — which the catch then handles. The
    // point is that it is *catchable* and the run still finishes.
    both(
        r#"(try (while true 1) (catch (e) (get e "kind")))"#,
        "runtime",
    );
}

#[test]
fn a_caught_step_limit_does_not_starve_the_outer_run() {
    // After a caught runaway the enclosing run must still have budget left to
    // finish its own work. Restoring the caller's counter is what makes this
    // pass; charging it for the body's steps would not.
    let src = r#"(try (while true 1) (catch (e) 0))
                (def i 0)
                (while (< i 100)
                  (def i (+ i 1)))
                i"#;
    both(src, "100");
}

#[test]
fn a_try_is_missing_its_catch_is_an_error_at_compile_time() {
    // Not silently "never catches anything": a `try` with no `catch` has nowhere
    // to send a failure, so saying so beats a body that just runs.
    let e = run_str(r#"(try 1)"#).expect_err("no catch clause");
    assert!(
        e.message().contains("catch"),
        "message should name the missing clause, got {:?}",
        e.message()
    );
}

#[test]
fn a_catch_taking_the_wrong_number_of_bindings_is_an_error() {
    for src in [
        r#"(try 1 (catch () 2))"#,
        r#"(try 1 (catch (a b) 2))"#,
        r#"(try 1 (catch 2))"#,
    ] {
        let e = run_str(src).expect_err(src);
        assert!(
            e.message().contains("catch"),
            "{src:?} should be rejected with a catch-shaped message, got {:?}",
            e.message()
        );
        // The tree-walk must reject exactly the same programs.
        assert!(run_in_tree_walk(src).is_err(), "tree-walk accepted {src:?}");
    }
}

#[test]
fn a_deeply_nested_try_unwinds_to_the_outermost_handler() {
    // Exercises the VM's frame-depth restore: the failing call is several
    // frames below the `try`, so the unwind has to pop all of them and land the
    // handler in the right frame.
    both(
        r#"(def deep (fn (n) (if (= n 0) (error "bottom") (deep (- n 1)))))
           (try (deep 5) (catch (e) (get e "message")))"#,
        "bottom",
    );
}

#[test]
fn an_error_from_a_builtin_inside_a_nested_call_is_caught() {
    // The failing call is behind a function boundary, so the VM's unwind has
    // to pop the callee frame and restore the caller's before the handler runs.
    both(
        r#"(def apply2 (fn (f x) (f x)))
           (try (apply2 read-file "definitely-not-here.txt") (catch (e) (get e "message")))"#,
        "read-file: cannot read 'definitely-not-here.txt'",
    );
}
