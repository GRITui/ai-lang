//! `map` / `filter` / `reduce` / `sort` — the two evaluators, and the edges.
//!
//! The 4-backend rule makes this a *parity* test first and a *behaviour* test
//! second. The interpreter and the bytecode VM are the two backends whose
//! internals differ most here: `map`/`filter`/`reduce` are special forms lowered
//! to loops (once, before either evaluator sees the program), while `sort` is a
//! real builtin whose comparator is called through a different entry point in
//! each. A bug that hit only one of them would be invisible to a single-backend
//! test, so every case runs on both. The AOT and transpiler halves live in the
//! sibling crates.

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

// ---------------------------------------------------------------------------
// map
// ---------------------------------------------------------------------------

#[test]
fn map_applies_the_fn_to_each_element() {
    both(r#"(map (fn (x) (* x 2)) (list 1 2 3 4 5))"#, "(2 4 6 8 10)");
}

#[test]
fn map_is_pure_and_does_not_touch_the_input() {
    // The input list is named so a mutation would be visible in the second
    // result. Purity is a stated property of all four operations, and the list
    // representation is shared cons cells — so a buggy implementation that
    // reused the input would show up here as a changed `xs`.
    both(
        r#"
        (def xs (list 1 2 3))
        (def ys (map (fn (x) (+ x 100)) xs))
        (list xs ys)
        "#,
        "((1 2 3) (101 102 103))",
    );
}

#[test]
fn map_of_an_empty_list_is_empty() {
    both(r#"(map (fn (x) (* x 2)) (list))"#, "()");
}

#[test]
fn map_over_a_single_element() {
    both(r#"(map (fn (x) (* x 2)) (list 7))"#, "(14)");
}

#[test]
fn map_over_a_non_list_is_a_type_error() {
    // The message names the builtin the generated loop reached first, not
    // `map`. That is a deliberate non-goal of the desugaring rather than an
    // oversight: naming `map` would mean a `list?` predicate, and AINL has no
    // type predicates at all (no `list?`, no `str?`, no `int?`), so the only
    // way to produce that wording from the expansion would be to hard-code an
    // error per call site — four more strings to keep in step across six
    // backends, in exchange for a nicer word in a message the user can already
    // read. The failure is immediate and unambiguous either way, which is the
    // property that actually matters. (It is `len` and not `first` because the
    // loop's termination test is a length test — see the `nil`-in-a-list
    // regression below for why that is not optional.)
    both(
        r#"(map (fn (x) x) 5)"#,
        "ERR: len expects list, str, or hash, got int",
    );
}

// ---------------------------------------------------------------------------
// filter
// ---------------------------------------------------------------------------

#[test]
fn filter_keeps_elements_where_the_fn_is_truthy() {
    both(r#"(filter (fn (x) (> x 2)) (list 1 2 3 4 5))"#, "(3 4 5)");
}

#[test]
fn filter_uses_ainl_truthiness_not_the_hosts() {
    // `nil` and `false` are the only falsey values; `0` and `""` are TRUTHY.
    // A host that used `if (x)` here would drop the zero and the empty string,
    // and Ruby in particular would drop `""` — so this is the case that catches
    // a boolean-typed port.
    both(
        r#"(filter (fn (x) x) (list 0 1 false nil 2 ""))"#,
        r#"(0 1 2 "")"#,
    );
}

#[test]
fn filter_can_keep_everything_or_nothing() {
    both(r#"(filter (fn (x) true) (list 1 2))"#, "(1 2)");
    both(r#"(filter (fn (x) false) (list 1 2))"#, "()");
}

#[test]
fn filter_of_an_empty_list_is_empty() {
    both(r#"(filter (fn (x) (> x 0)) (list))"#, "()");
}

#[test]
fn filter_of_a_non_list_is_a_type_error() {
    both(
        r#"(filter (fn (x) x) "nope")"#,
        "ERR: first expects list, got str",
    );
}
// ---------------------------------------------------------------------------
// reduce
// ---------------------------------------------------------------------------

#[test]
fn reduce_folds_left_with_the_accumulator_first() {
    // Argument order is the trap: the callback takes (acc x), NOT (x acc).
    both(
        r#"(reduce (fn (acc x) (+ acc x)) 0 (list 1 2 3 4 5))"#,
        "15",
    );
}

#[test]
fn reduce_of_an_empty_list_is_the_initial_value() {
    both(r#"(reduce (fn (acc x) (+ acc x)) 99 (list))"#, "99");
}

#[test]
fn reduce_can_build_a_list() {
    // The accumulator is an ordinary value, so it can be a list. This is the
    // shape the card calls out explicitly, and the one the e2e loops used to
    // hand-roll.
    both(
        r#"(reduce (fn (acc x) (push acc (* x 10))) (list) (list 1 2 3))"#,
        "(10 20 30)",
    );
}

#[test]
fn reduce_is_left_to_right_not_a_rewind() {
    // A non-commutative fold exposes the order. `(cons x acc)` prepends, so
    // folding [1 2 3] left-to-right builds 3,2,1 — the list reversed. A right
    // fold would produce (1 2 3). Both the direction and the accumulator
    // position are pinned here.
    both(
        r#"(reduce (fn (acc x) (cons x acc)) (list) (list 1 2 3))"#,
        "(3 2 1)",
    );
}

#[test]
fn reduce_over_a_non_list_is_a_type_error() {
    both(
        r#"(reduce (fn (a b) a) 0 7)"#,
        "ERR: len expects list, str, or hash, got int",
    );
}

// ---------------------------------------------------------------------------
// nesting
// ---------------------------------------------------------------------------

#[test]
fn collection_forms_nest() {
    // A `map` over a `map`: the inner form is lowered too, and the inner result
    // is a list the outer loop walks. This is why the list operand is passed as
    // a *parameter* to the generated helper rather than re-evaluated per
    // element — re-evaluating would re-run the inner loop on every step.
    both(
        r#"(map (fn (x) (* x 10)) (map (fn (x) (+ x 1)) (list 1 2 3)))"#,
        "(20 30 40)",
    );
}

#[test]
fn a_collection_form_inside_a_def_and_inside_a_call() {
    // Both are expression positions, which is where an inline `do` or a
    // multi-statement `fn` would be refused by the transpilers. The expansion
    // has to be legal there.
    both(
        r#"(def d (map (fn (x) (* x 3)) (list 1 2))) (list d (len d))"#,
        "((3 6) 2)",
    );
}

#[test]
fn reduce_over_a_mapped_list() {
    both(
        r#"(reduce (fn (a x) (+ a x)) 0 (map (fn (x) (* x x)) (list 1 2 3 4)))"#,
        "30",
    );
}

#[test]
fn a_collection_form_works_inside_a_function_body() {
    // A generated loop's `def`s must land in the *enclosing function's* scope.
    // The generated helper is itself a `fn`, so this is one function call inside
    // another — and it is the case that would break if the expansion bound its
    // accumulators in the caller's scope instead.
    both(
        r#"((fn (xs) (map (fn (x) (* x 2)) xs)) (list 1 2 3))"#,
        "(2 4 6)",
    );
}

// ---------------------------------------------------------------------------
// the generated code is hygienic
// ---------------------------------------------------------------------------

#[test]
fn the_generated_names_do_not_collide_with_program_variables() {
    // `_ainl_*` is reserved. A program that happens to use one of those names
    // must still get the right answer — either the program's value wins where it
    // should, or the generated code shadows it only inside its own helper.
    both(
        r#"
        (def _ainl_out "mine")
        (def _ainl_cur 99)
        (list _ainl_out (map (fn (x) x) (list 1 2)))
        "#,
        r#"("mine" (1 2))"#,
    );
}

#[test]
fn a_conditional_branch_containing_a_collection_form_works() {
    both(
        r#"(if true (map (fn (x) (* x 2)) (list 1 2)) (list 9))"#,
        "(2 4)",
    );
}

#[test]
fn a_loop_containing_a_collection_form_works() {
    // The generated loop is a `fn` called from inside a `while`, so the two
    // loop bodies must not share state.
    both(
        r#"
        (def out (list))
        (def i 0)
        (while (< i 3)
          (def out (push out (map (fn (x) (* x 2)) (list i))))
          (def i (+ i 1)))
        out
        "#,
        "((0) (2) (4))",
    );
}

// ---------------------------------------------------------------------------
// malformed forms
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_arity_is_reported_not_silently_accepted() {
    // The lowering leaves a wrong-arity form alone so ordinary dispatch reports
    // it. What matters is that it FAILS — a form that quietly sorted or mapped
    // something anyway would be worse than an error.
    let on_vm = vm(r#"(map (fn (x) x))"#);
    assert!(
        on_vm.starts_with("ERR: "),
        "a one-argument map should not have succeeded, got {on_vm:?}"
    );
    let four = vm(r#"(map (fn (x) x) (list 1) (list 2))"#);
    assert!(
        four.starts_with("ERR: "),
        "a three-argument map should not have succeeded, got {four:?}"
    );
}

#[test]
fn a_malformed_reduce_arity_is_reported() {
    let on_vm = vm(r#"(reduce (fn (a b) a) 0)"#);
    assert!(
        on_vm.starts_with("ERR: "),
        "a two-argument reduce should not have succeeded, got {on_vm:?}"
    );
}

#[test]
fn both_evaluators_agree_on_the_malformed_forms_too() {
    // The *message* is allowed to differ (each backend words its own arity
    // error), but both must fail, and both must fail on the same input.
    for src in [
        r#"(map (fn (x) x))"#,
        r#"(map (fn (x) x) (list 1) (list 2))"#,
        r#"(filter (fn (x) x))"#,
        r#"(reduce (fn (a b) a) 0)"#,
    ] {
        assert!(
            vm(src).starts_with("ERR: ") && tree(src).starts_with("ERR: "),
            "both evaluators should reject {src:?}\n  vm:   {}\n  tree: {}",
            vm(src),
            tree(src)
        );
    }
}

#[test]
fn a_list_containing_nil_is_walked_to_the_end() {
    // Regression test for the original termination test.
    //
    // `(= (first cur) nil)` looks like an exact "am I empty?" check and is
    // not: `nil` is a legal ELEMENT. Every early version of the generated loop
    // used it, and it silently dropped everything after the first `nil`:
    // `(map identity (list 1 nil 2))` returned `(1)`. Nothing on a numeric-only
    // test would have caught it, which is exactly why this case is pinned for
    // all three of `map`, `filter` and `reduce`.
    both(r#"(map (fn (x) x) (list 1 nil 2))"#, "(1 nil 2)");
    both(r#"(filter (fn (x) x) (list 1 nil 2 3))"#, "(1 2 3)");
    both(
        r#"(reduce (fn (acc x) (push acc x)) (list) (list nil 1 nil 2))"#,
        "(nil 1 nil 2)",
    );
}

#[test]
fn a_list_containing_false_is_walked_to_the_end() {
    // The other falsey value, and the one most likely to be caught by a
    // boolean-typed port rather than by the termination test.
    both(
        r#"(map (fn (x) x) (list false true false))"#,
        "(false true false)",
    );
}

#[test]
fn a_list_of_nils_alone_is_walked_to_the_end() {
    both(r#"(map (fn (x) x) (list nil nil nil))"#, "(nil nil nil)");
    both(r#"(map (fn (x) x) (list))"#, "()");
}

// ---------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------

#[test]
fn sort_orders_numbers_by_value() {
    both(r#"(sort (list 3 1 2))"#, "(1 2 3)");
    both(r#"(sort (list -1 5 0))"#, "(-1 0 5)");
}

#[test]
fn sort_orders_strings_bytewise() {
    both(
        r#"(sort (list "pear" "apple" "fig"))"#,
        r#"("apple" "fig" "pear")"#,
    );
    // Uppercase sorts before lowercase bytewise (0x41 < 0x61), which is the
    // opposite of a case-insensitive or locale-aware order — and the opposite of
    // what a default `LC_ALL` collation would give on some hosts.
    both(r#"(sort (list "b" "A" "a"))"#, r#"("A" "a" "b")"#);
}

#[test]
fn sort_is_stable() {
    // Equal keys keep their input order. Without this the four backends could
    // each be "correct" and still disagree, which is the failure the card's
    // stability requirement exists to prevent.
    both(
        r#"(sort (fn (a b) (- (nth a 1) (nth b 1)))
                  (list (list "bob" 30) (list "amy" 25) (list "cid" 30) (list "dan" 25)))"#,
        r#"(("amy" 25) ("dan" 25) ("bob" 30) ("cid" 30))"#,
    );
    // A single stable pass over an all-equal list is the degenerate case: every
    // backend must return it in input order, not reversed.
    both(r#"(sort (list 1 1 1 1 1 1 1 1))"#, "(1 1 1 1 1 1 1 1)");
    // Descending, where an unstable sort would most visibly scramble ties.
    both(
        r#"(sort (fn (a b) (- b a)) (list 1 2 2 2 3 3 1))"#,
        "(3 3 2 2 2 1 1)",
    );
}

#[test]
fn sort_returns_a_new_list_and_leaves_the_input_alone() {
    both(
        r#"
        (def xs (list 3 1 2))
        (def ys (sort xs))
        (list xs ys)
        "#,
        "((3 1 2) (1 2 3))",
    );
}

#[test]
fn sort_of_an_empty_or_single_element_list() {
    both(r#"(sort (list))"#, "()");
    both(r#"(sort (list 7))"#, "(7)");
}

#[test]
fn sort_with_a_comparator_sorts_ascending_and_descending() {
    both(r#"(sort (fn (a b) (- a b)) (list 3 1 2))"#, "(1 2 3)");
    both(r#"(sort (fn (a b) (- b a)) (list 3 1 2))"#, "(3 2 1)");
}

#[test]
fn sort_mixes_int_and_float_by_value() {
    // `(= 1 1.0)` is true in AINL, so a `sort` that ordered by type TAG would
    // answer a different question than the language's own equality does. The
    // `<` operator is int-only (`(< 1 1.0)` is false both ways), so this is the
    // one place the two are mixed and `sort` deliberately disagrees with `<`.
    //
    // The equal pair keeps its input order — stability again — so `(1.0 1)`
    // stays as it came in.
    both(r#"(sort (list 2 1.5 1))"#, "(1 1.5 2)");
    both(r#"(sort (list 1.0 1))"#, "(1.0 1)");
    both(r#"(sort (list 1 1.0))"#, "(1 1.0)");
}

#[test]
fn sort_rejects_a_mixed_type_list_with_a_readable_message() {
    // The card calls for a model-readable message. The point of rejecting rather
    // than defining an order is that a defined-but-arbitrary order would return
    // a stable, reproducible answer to a program that has a bug in it, and the
    // bug would surface later as a wrong number instead of here.
    let msg = vm(r#"(sort (list 1 "a"))"#);
    assert_eq!(
        msg, "ERR: sort expects a list of numbers or of strings, got a list mixing int and str",
        "unhelpful message for a mixed-type sort"
    );
    // Both orders of the mix, and a non-numeric non-string.
    for src in [
        r#"(sort (list "a" 1))"#,
        r#"(sort (list 1 nil))"#,
        r#"(sort (list true 1))"#,
    ] {
        assert!(
            vm(src).starts_with("ERR: sort expects a list of numbers or of strings"),
            "expected the mixed-type error for {src:?}, got {:?}",
            vm(src)
        );
    }
}

#[test]
fn sort_with_a_comparator_may_sort_a_list_of_records() {
    // The comparator form is exempt from the mixed-type rule — that is the
    // whole point of having it. A list of lists is neither numbers nor strings,
    // and a program must still be able to order it by a key it chooses.
    both(
        r#"(sort (fn (a b) (- (nth a 1) (nth b 1)))
                  (list (list "bob" 30) (list "amy" 25)))"#,
        r#"(("amy" 25) ("bob" 30))"#,
    );
}

#[test]
fn sort_rejects_a_comparator_that_does_not_return_a_number() {
    // A comparator returning a bool would silently read as 0/1 and look like a
    // working sort, leaving the list in near-input-order. It has to be named.
    let msg = vm(r#"(sort (fn (a b) true) (list 1 2 3))"#);
    assert!(
        msg.starts_with("ERR: sort comparator must return a number"),
        "expected the comparator-type error, got {msg:?}"
    );
    let nil_msg = vm(r#"(sort (fn (a b) nil) (list 1 2 3))"#);
    assert!(
        nil_msg.starts_with("ERR: sort comparator must return a number"),
        "expected the comparator-type error, got {nil_msg:?}"
    );
}

#[test]
fn sort_rejects_a_non_list_and_a_non_fn_comparator() {
    assert!(vm(r#"(sort 5)"#).starts_with("ERR: sort expects a list"));
    assert!(vm(r#"(sort "x" (list 1))"#).starts_with("ERR: sort expects a fn"));
    assert!(vm(r#"(sort)"#).starts_with("ERR: sort expects (sort list)"));
    assert!(vm(r#"(sort (list 1) (list 2) (list 3))"#).starts_with("ERR: sort expects (sort list)"));
}

#[test]
fn sort_is_reachable_as_a_value() {
    // `sort` is an ordinary builtin, unlike `map` / `filter` / `reduce` which
    // are special forms with no binding. `(def s sort)` has to work, or a
    // higher-order program could not pass it on.
    both(r#"(def s sort) (s (list 2 1))"#, "(1 2)");
}

#[test]
fn sort_composes_with_map_filter_and_reduce() {
    both(
        r#"(reduce (fn (a x) (+ a x)) 0 (sort (map (fn (x) (* x x)) (list 3 1 2))))"#,
        "14",
    );
    both(
        r#"(sort (filter (fn (x) (> x 2)) (list 4 1 3 2)))"#,
        "(3 4)",
    );
}

// ---------------------------------------------------------------------------
// a program that uses no collection form is untouched
// ---------------------------------------------------------------------------

#[test]
fn a_program_with_no_collection_form_is_returned_unchanged() {
    // The lowering is a no-op when unused. This is what keeps `ainl ast` and
    // every AOT / transpile output byte-identical for pre-existing programs: no
    // helper `def`s, no generated names, nothing.
    let src = "(def x 1) (+ x 1)";
    let forms = ainl_core::parse(src).unwrap();
    let lowered = ainl_core::collection_forms::lower(&forms).unwrap();
    assert_eq!(
        forms.len(),
        lowered.len(),
        "lowering a program with no collection form changed its shape"
    );
    for (before, after) in forms.iter().zip(lowered.iter()) {
        assert_eq!(
            before, after,
            "lowering altered a form that has nothing to do with collections"
        );
    }
}

#[test]
fn a_program_that_uses_one_gets_exactly_the_three_helpers() {
    // Exactly three: no per-use helper, so a program with fifty `map`s does not
    // grow fifty copies of the loop.
    let src = "(map (fn (x) x) (list 1)) (map (fn (x) x) (list 2))";
    let forms = ainl_core::parse(src).unwrap();
    let lowered = ainl_core::collection_forms::lower(&forms).unwrap();
    assert_eq!(
        lowered.len(),
        5,
        "expected 3 helper defs plus the 2 program forms, got {}",
        lowered.len()
    );
}

// ---------------------------------------------------------------------------
// the lowering is shared, not per-backend
// ---------------------------------------------------------------------------

#[test]
fn every_backend_lowers_through_the_same_function() {
    // Not a behavioural test — a structural one. The reason the four backends
    // cannot drift is that they all call `lower`; if one of them stopped, the
    // parity suites would still pass for a while (a program that used no
    // collection form would emit identically) and the divergence would land
    // later, on a user program. This pins the call sites by name.
    let core = include_str!("../src/collection_forms.rs");
    let _ = core;

    let vm_src = include_str!("../src/vm.rs");
    assert!(
        vm_src.contains("collection_forms::lower"),
        "the VM must lower through the shared function"
    );
    let eval_src = include_str!("../src/eval.rs");
    assert!(
        eval_src.contains("collection_forms::lower"),
        "the tree-walk must lower through the shared function"
    );
}

#[test]
fn a_non_function_operand_is_named_after_the_form_not_the_helper() {
    // `fn`-as-data has to be a *checked* boundary, and the check has to name
    // the form the reader wrote. Left to the desugared loop, `(map 5 …)` failed
    // in five different ways and only one of them said "map":
    //
    //   interpreter  cannot call a int
    //   python       'int' object is not callable
    //   js           _ainl_f is not a function     <- the generated name
    //
    // The operand is checked before the rewrite, so all six evaluators raise
    // the same message by construction. `message()` is compared, not the full
    // error, because the interpreter additionally carries a position.
    for (src, want) in [
        ("(map 5 (list 1))", "map expects a fn, got int"),
        ("(filter \"x\" (list 1))", "filter expects a fn, got str"),
        ("(reduce 1.5 0 (list 1))", "reduce expects a fn, got float"),
    ] {
        let tree_err = run_in_tree_walk(src).expect_err(src).message().to_string();
        assert_eq!(tree_err, want, "tree-walk, {src}");
        let vm_err = run_str(src).expect_err(src).message().to_string();
        assert_eq!(vm_err, want, "vm, {src}");
    }
}

#[test]
fn a_named_operand_is_still_allowed_because_it_may_hold_a_function() {
    // The static check must not reject a bare symbol: that is what a callback
    // normally looks like, and `(def dbl (fn (x) (* x 2)))` then
    // `(map dbl …)` is the ordinary way to write this.
    both(
        r#"(def dbl (fn (x) (* x 2))) (map dbl (list 1 2 3))"#,
        "(2 4 6)",
    );
}
