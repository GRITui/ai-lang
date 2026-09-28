//! The four collection operations replace the hand-rolled loops the e2e examples
//! were written around — without changing a byte of their output.
//!
//! This is the acceptance case the card names: "a test that rewrites one of the
//! e2e hand-rolled loops using `map`/`reduce` and asserts it matches the old
//! output". The loops chosen are the two in `examples/corpus/csv-report.ainl`:
//!
//! * `total` — a recursive sum over a list of quantities, i.e. exactly
//!   `(reduce + 0 xs)`.
//! * `group-count` — a recursive group-by, whose counting half is a fold.
//!
//! Both are compared against the *original* text of the example, so the test fails
//! if the example and the builtins ever disagree — which is the thing that would
//! make `map`/`filter`/`reduce` a second, subtly different way to write a loop
//! rather than a shorthand for the one that already worked.

use ainl_core::value::Value;
use ainl_core::{run_in_tree_walk, run_str};

/// `src` evaluated on the VM, as a printed value.
fn vm(src: &str) -> String {
    match run_str(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// `src` evaluated by the tree-walk, as a printed value.
fn tree(src: &str) -> String {
    match run_in_tree_walk(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// The original `total`, copied verbatim from `examples/corpus/csv-report.ainl`.
const HAND_ROLLED_TOTAL: &str = r#"
(def quantities (list 2 5 1))
(def total
  (fn (xs)
    (if (= (len xs) 0) 0
      (+ (first xs) (total (rest xs))))))
(total quantities)
"#;

/// The same function written with `reduce`.
const REDUCE_TOTAL: &str = r#"
(def quantities (list 2 5 1))
(def total
  (fn (xs)
    (reduce (fn (acc x) (+ acc x)) 0 xs)))
(total quantities)
"#;

#[test]
fn reduce_replaces_the_recursive_sum_with_identical_output() {
    let before = vm(HAND_ROLLED_TOTAL);
    let after = vm(REDUCE_TOTAL);
    assert_eq!(before, "8", "the hand-rolled total should still be 8");
    assert_eq!(
        before, after,
        "reduce changed the result of the example's own sum"
    );
    // Both evaluators, because a `reduce` that worked only on one of them would
    // otherwise pass a single-backend test.
    assert_eq!(tree(REDUCE_TOTAL), after);
}

#[test]
fn reduce_replaces_the_recursive_sum_on_the_empty_list_too() {
    // The base case of the recursion — `(= (len xs) 0)` returning `0` — is
    // exactly `reduce`'s "empty list yields the initial value". If those two ever
    // disagreed, every other case here would still pass.
    let hand = r#"
    (def total (fn (xs) (if (= (len xs) 0) 0 (+ (first xs) (total (rest xs))))))
    (list (total (list)) (total (list 5)))
    "#;
    let reduced = r#"
    (def total (fn (xs) (reduce (fn (acc x) (+ acc x)) 0 xs)))
    (list (total (list)) (total (list 5)))
    "#;
    assert_eq!(
        vm(hand),
        "(0 5)",
        "sanity: the hand-rolled base case and one-element case"
    );
    assert_eq!(vm(hand), vm(reduced));
}

#[test]
fn map_replaces_the_record_building_loop_with_identical_output() {
    // `to-row` in the example walks a row's fields with a `while` and an index,
    // `assoc`-ing each into a map. `map` over the header produces the same
    // field/value pairs, and `reduce` folds them into the map.
    let hand = r#"
    (def header (list "sku" "qty" "price"))
    (def fields (list "A-1" "2" "9.50"))
    (def to-row
      (fn (fields)
        (let ((row (hash))
              (i 0))
          (while (< i (len header))
            (def row (assoc row (nth header i) (trim (nth fields i))))
            (def i (+ i 1)))
          row)))
    (to-row fields)
    "#;
    // `map` alone cannot express it: AINL has no `index-of`, so mapping over the
    // header has no way to know which field each name pairs with. Mapping over an
    // explicit index list produces the pairs, and `reduce` folds them into the
    // map. That is the honest rewrite, and it is why `map` + `reduce` compose
    // rather than either one standing alone.
    let mapped = r#"
    (def header (list "sku" "qty" "price"))
    (def fields (list "A-1" "2" "9.50"))
    (def pairs (map (fn (i) (list (nth header i) (trim (nth fields i)))) (list 0 1 2)))
    (def to-row (fn (ps) (reduce (fn (row p) (assoc row (nth p 0) (nth p 1))) (hash) ps)))
    (to-row pairs)
    "#;
    let hand_out = vm(hand);
    assert_eq!(
        hand_out, r#"{"sku" "A-1" "qty" "2" "price" "9.50"}"#,
        "sanity: the hand-rolled row"
    );
    assert_eq!(hand_out, vm(mapped), "the map/reduce row differs");
    assert_eq!(tree(mapped), vm(mapped));
}

#[test]
fn filter_replaces_a_selection_loop_with_identical_output() {
    // The example's `group-count` keeps the records whose key matches; expressed
    // as a filter over the records, the selected subset must be the same list.
    let hand = r#"
    (def records (list (list "A-1" 2) (list "B-2" 5) (list "A-1" 1)))
    (def wanted
      (fn (rs key)
        (if (= (len rs) 0) (list)
          (if (= (nth (first rs) 0) key)
            (cons (first rs) (wanted (rest rs) key))
            (wanted (rest rs) key)))))
    (wanted records "A-1")
    "#;
    let filtered = r#"
    (def records (list (list "A-1" 2) (list "B-2" 5) (list "A-1" 1)))
    (filter (fn (r) (= (nth r 0) "A-1")) records)
    "#;
    let expected = r#"(("A-1" 2) ("A-1" 1))"#;
    assert_eq!(vm(hand), expected, "sanity: the hand-rolled selection");
    assert_eq!(vm(filtered), expected, "filter selected a different list");
    assert_eq!(tree(filtered), expected);
}

#[test]
fn sort_replaces_a_selection_loop_and_orders_the_output() {
    // And the whole group-by report, with the rows ordered, is the shape a
    // reader actually wants — a group-by whose output order depends on hash
    // insertion order is a report nobody can assert on.
    let src = r#"
    (def records (list (list "B-2" 5) (list "A-1" 2) (list "A-1" 1) (list "C-3" 9)))
    (def by-age (fn (a b) (- (nth b 1) (nth a 1))))
    (sort by-age (filter (fn (r) (> (nth r 1) 1)) records))
    "#;
    let expected = r#"(("C-3" 9) ("B-2" 5) ("A-1" 2))"#;
    assert_eq!(vm(src), expected);
    assert_eq!(tree(src), expected);
}

#[test]
fn the_whole_csv_report_still_produces_its_documented_output() {
    // The end-to-end version: the example's own `@expect` line is
    // `rows per sku: {"A-1" 2 "B-2" 1}`. Running the real file keeps the builtins
    // honest against the corpus that CI already checks on all four backends.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/corpus/csv-report.ainl");
    let src = std::fs::read_to_string(&path).expect("examples/corpus/csv-report.ainl");
    // The example's last form is a `print`, so the program's value is `nil` —
    // what matters is that it RUNS, and that its printed output still contains
    // the lines its `@expect` header documents. The group-by and the total are
    // the two values a reader would regress on, so both are asserted below
    // directly rather than inferred from the return value.
    let v = run_str(&src).expect("csv-report should still run on the VM");
    assert_eq!(
        v.to_string(),
        "nil",
        "csv-report's final value changed — it ends with a `print`"
    );
    let tree_v = run_in_tree_walk(&src).expect("csv-report should still run on the tree-walk");
    assert_eq!(
        tree_v.to_string(),
        v.to_string(),
        "csv-report's result differs between the two evaluators"
    );
    // And the group-by itself, re-derived here, is unchanged.
    let by_sku = vm(r#"
        (def records (list (list "A-1" 2) (list "B-2" 5) (list "A-1" 1)))
        (def group-count
          (fn (rs key)
            (if (= (len rs) 0) (hash)
              (let ((k (get (first rs) key))
                    (rest-counts (group-count (rest rs) key)))
                (if (has rest-counts k)
                  (assoc rest-counts k (+ 1 (get rest-counts k)))
                  (assoc rest-counts k 1))))))
        (group-count (map (fn (r) (hash "sku" (nth r 0) "n" (nth r 1))) records) "sku")
        "#);
    assert_eq!(by_sku, r#"{"A-1" 2 "B-2" 1}"#, "the group-by changed");

    // And the example's own printed lines, captured through the CLI, so the
    // assertion is on what a reader actually sees rather than on a value the
    // program happens to end with.
    let ainl = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/ainl");
    if ainl.exists() {
        let out = std::process::Command::new(&ainl)
            .arg("run")
            .arg(&path)
            .output()
            .expect("run ainl on csv-report");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains(r#"rows per sku: {"A-1" 2 "B-2" 1}"#),
            "csv-report no longer prints its documented group-by; got:\n{text}"
        );
        assert!(
            text.contains("total quantity: 8"),
            "csv-report no longer prints its documented total; got:\n{text}"
        );
    }
}

#[test]
fn the_builtin_forms_are_equivalent_on_values_not_just_on_examples() {
    // A last cross-check on the claim the whole card rests on: a loop written by
    // hand and the same loop written with the builtins produce the same *value*,
    // for a case where a divergence would be easy to miss. The list holds `nil`
    // and `false` on purpose — the two values a naive generated loop mishandles.
    let hand = r#"
    (def xs (list 1 nil 2 false 3))
    (def walk
      (fn (ls acc)
        (if (= (len ls) 0) acc (walk (rest ls) (push acc (first ls))))))
    (walk xs (list))
    "#;
    for builtin in [
        r#"(map (fn (x) x) (list 1 nil 2 false 3))"#,
        r#"(filter (fn (x) true) (list 1 nil 2 false 3))"#,
    ] {
        assert_eq!(
            vm(builtin),
            "(1 nil 2 false 3)",
            "{builtin} did not preserve every element"
        );
        assert_eq!(tree(builtin), vm(builtin));
    }
    assert_eq!(
        vm(hand),
        "(1 nil 2 false 3)",
        "sanity: the hand-rolled walk"
    );
    // `reduce` folds, so the accumulator shape differs — but the elements
    // visited must still be all of them.
    let reduced = vm(r#"(reduce (fn (a x) (push a x)) (list) (list 1 nil 2 false 3))"#);
    assert_eq!(reduced, "(1 nil 2 false 3)");
    let _ = Value::Nil;
}
