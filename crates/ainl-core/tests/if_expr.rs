//! `if` — the two evaluators agree on arity, value and stack discipline.
//!
//! This is a *parity* test first. `ainl run` uses the bytecode VM, and the VM
//! compiles an `if` to jumps on an operand stack, while the tree-walk
//! evaluator returns a value up the Rust stack. A missing jump is therefore
//! invisible in the tree-walk and catastrophic in the VM: the form leaves
//! **two** values where its parent expects one, and every later form reads a
//! shifted operand. The symptom is not "the wrong answer" but "cannot call a
//! nil" several forms downstream, which is why the cases below all put a 2-arg
//! `if` in a `fn`/`let` body and then do something else.
//!
//! A `while` loop hides the bug entirely: its per-iteration `Pop` eats the
//! surplus value, so a 2-arg `if` inside a loop body passed CI for the whole
//! life of the language. Do not "simplify" these cases into a loop.

use ainl_core::{run_in_tree_walk, run_str};

/// The value of `src` on the bytecode VM (what `ainl run` does), as printed.
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

/// Assert both evaluators agree AND that the shared answer is `expected`.
/// Both halves matter: a VM-only assertion passes with a tree-walk-only bug,
/// and the agreement half is the whole point of the 4-backend rule.
fn both(src: &str, expected: &str) {
    let on_vm = vm(src);
    let on_tree = tree(src);
    assert_eq!(
        on_vm, on_tree,
        "evaluators disagree on {src:?}\n  vm:   {on_vm}\n  tree: {on_tree}"
    );
    assert_eq!(on_vm, expected, "wrong result for {src:?}");
}

// ---- the regression: a 2-arg `if` in a closure body, condition TRUE ----

/// The card's minimal repro, verbatim. The second top-level call is not
/// optional: the fn body's own `Pop` (which sits between forms) eats the
/// surplus `Nil`, so a single `(f 0)` returns the RIGHT value while quietly
/// leaving one extra operand on the caller's stack. The next `Call` then pops
/// shifted operands and pops a `nil` as its callee — "cannot call a nil",
/// which is the exact error the card reported. Asserting the program merely
/// does not error is the point: before the fix this returned
/// `ERR: cannot call a nil`.
#[test]
fn two_arg_if_in_a_fn_body_true_does_not_leak_a_nil() {
    both(
        r#"
        (def f (fn (x)
                 (if (= x 0) (print "hit"))
                 (print "done")))
        (print (f 0))
        (print (f 5))
        "#,
        "nil",
    );
}

/// The same program with the THEN-branch replaced by a value, so the leaked
/// `Nil` is a distinct thing rather than a second `nil`. `(f 0)` leaks
/// `nil` next to the then-value; the caller's stack ends up holding the
/// then-value where the next form's callee belongs.
#[test]
fn two_arg_if_true_then_a_following_call_in_a_fn_body() {
    // `(f 5)` never took the buggy path, so the control is: a regression that
    // only broke the TRUE case would leave this green. Both calls are in one
    // program because the leak only surfaces on the following call.
    both(
        r#"
        (def f (fn (x) (if (= x 0) "hit") (print "done")))
        (f 0)
        (f 5)
        "#,
        "nil",
    );
}

/// The false branch of a 2-arg `if` must still yield `nil` and must still fall
/// through to the next form. The fix adds a jump over the `Nil`; if it were
/// mis-patched this case would return the then-value or skip the form.
#[test]
fn two_arg_if_false_is_nil_and_still_falls_through() {
    both(
        r#"
        (def f (fn (x) (if (= x 0) "then-value") (str "after " (str x))))
        (f 5)
        "#,
        "after 5",
    );
}

/// A 2-arg `if` as the LAST form of a fn body. Here the surplus `Nil` would be
/// the fn's return value, so the bug shows as a wrong return rather than a
/// crash — a different symptom, same missing jump.
#[test]
fn two_arg_if_as_the_last_form_of_a_fn_body_returns_the_then_value() {
    both(r#"(def f (fn (x) (if (= x 0) "hit"))) (f 0)"#, "hit");
    both(r#"(def f (fn (x) (if (= x 0) "hit"))) (f 5)"#, "nil");
}

/// Three forms after the `if`, each a call. The surplus value shifts operands
/// one form at a time, so a single follow-up form proves the fix; three pins
/// that the stack is left with exactly one value and not merely "fewer
/// crashes".
#[test]
fn two_arg_if_leaves_exactly_one_value_on_the_stack() {
    both(
        r#"
        (def f (fn (x)
                 (if (= x 0) "hit")
                 (str "a" (str x))
                 (str "b" (str x))))
        (f 0)
        "#,
        "b0",
    );
}

/// The bug also reached `let` bodies, which compile the same way. `05_organize`
/// hit it through `map` over a closure, not through a hand-written `def`.
#[test]
fn two_arg_if_in_a_let_body_true_does_not_leak_a_nil() {
    both(
        r#"
        (def f (fn (x)
                 (let ((v (if (= x 0) "hit")))
                   (str v "/done"))))
        (f 0)
        "#,
        "hit/done",
    );
}

/// And inside a closure passed to a higher-order builtin — the real-world
/// shape, and the one that produced "cannot call a nil" in the e2e corpus.
/// The `if` is deliberately NOT the last form: its value is discarded (a `do`
/// keeps only the last form), so the assertion is on the *following* form's
/// value, which is what the leaked `Nil` used to shift.
#[test]
fn two_arg_if_in_an_anonymous_closure_does_not_leak_a_nil() {
    both(
        r#"
        (def xs (list "a" "b"))
        (map (fn (s) (if (= s "a") (print "A")) (str s "!")) xs)
        "#,
        r#"("a!" "b!")"#,
    );
}

// ---- the 3-arg form must be untouched by the fix ----

/// The fix is a one-line change to the 2-arg arm. These pin the 3-arg arm
/// (which already had its `Jump`) so a "simplification" that folds the two
/// arms together cannot break the else.
#[test]
fn three_arg_if_still_takes_the_else() {
    both(r#"(if false "t" "e")"#, "e");
    both(r#"(if true "t" "e")"#, "t");
}

/// A 3-arg `if` followed by a form — the pattern the e2e corpus used as its
/// workaround. It must keep working, and it must now be redundant.
#[test]
fn three_arg_if_with_an_explicit_nil_else_still_falls_through() {
    both(
        r#"
        (def f (fn (x) (if (= x 0) "hit" nil) (str "done " (str x))))
        (f 0)
        "#,
        "done 0",
    );
}

/// Nested 2-arg `if`s in both arms: each compiles its own jump pair, so a
/// mis-patched jump target would send control into the wrong arm.
#[test]
fn nested_two_arg_ifs_each_keep_their_own_jump() {
    both(
        r#"
        (def f (fn (x)
                 (if (= x 0)
                     (if (= x 0) "inner-true" "inner-false")
                     (if (= x 1) "else-true" "else-false"))))
        (list (f 0) (f 1) (f 2))
        "#,
        r#"("inner-true" "else-true" "else-false")"#,
    );
}

/// A 2-arg `if` whose then-branch is itself a call, inside a loop. This is the
/// combination the bug was masked in: the loop's per-iteration pop used to eat
/// the surplus, so this shape passed forever. With a follow-up form in the loop
/// body it is the regression the card could not see.
#[test]
fn two_arg_if_in_a_loop_body_with_a_following_form() {
    both(
        r#"
        (def f (fn (n)
                 (let ((i 0) (acc (list)))
                   (while (< i n)
                     (if (= i 1) (print "one"))
                     (def acc (push acc i))
                     (def i (+ i 1)))
                   acc)))
        (f 3)
        "#,
        "(0 1 2)",
    );
}
