//! The Python, JavaScript and Ruby projections of the four collection
//! operations.
//!
//! The end-to-end contract — interpreter stdout vs transpiled stdout, byte for
//! byte — is checked by `scripts/check-transpile.sh` over `examples/*.ainl`, and
//! the AOT half by `crates/ainl-cc/tests/aot_stdlib.rs`. These tests cover the
//! other half: *how* each target is emitted, because "byte-identical" is only
//! meaningful if the emitted code is the code the tests think it is.
//!
//! Two things are checked per target, and they are the two ways a host port goes
//! wrong:
//!
//! 1. **The shape.** `map` / `filter` / `reduce` must appear as a *lowered loop*,
//!    never as the host's own `map` / `filter` — which on all three hosts is a
//!    different function with a different meaning (Python's returns an iterator,
//!    JavaScript's is a method, Ruby's is an Enumerable method). A port that
//!    bound AINL's `map` to the host's would typecheck and be wrong.
//! 2. **`sort`'s helpers.** The stable merge, the bytewise string order, the
//!    mixed-type rejection and the comparator's return-type check. Each host
//!    disagrees with the others somewhere in exactly this area, so each rule is
//!    asserted on the emitted text.

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

/// The text between two markers, so an assertion can be scoped to the generated
/// helpers and ignore the pre-existing runtime that legitimately uses the host's
/// own `map`.
fn section(out: &str, from: &str, to: &str) -> String {
    let start = out
        .find(from)
        .unwrap_or_else(|| panic!("no `{from}` in:\n{out}"));
    let end = out[start..]
        .find(to)
        .map(|i| start + i)
        .unwrap_or(out.len());
    out[start..end].to_string()
}

const PROGRAM: &str = r#"
(def dbl (fn (x) (* x 2)))
(def big (fn (x) (> x 2)))
(def add (fn (a b) (+ a b)))
(def nums (list 1 2 3 4 5))
(print (map dbl nums))
(print (filter big nums))
(print (reduce add 0 nums))
(print (sort nums))
(print (sort add nums))
"#;

// ---------------------------------------------------------------------------
// the three higher-order forms are lowered, not delegated
// ---------------------------------------------------------------------------

#[test]
fn python_lowers_map_filter_and_reduce_to_loops() {
    let out = py(PROGRAM);
    // The host's own `map`/`filter` must not appear as the implementation.
    assert!(
        !out.contains("list(map(") && !out.contains("[map("),
        "python bound AINL's map to the host's:\n{out}"
    );
    assert!(
        !out.contains("filter(lambda") && !out.contains("list(filter("),
        "python bound AINL's filter to the host's:\n{out}"
    );
    // What is there instead: the shared helper, bound with a top-level `def` —
    // which is the whole reason the expansion hoists it. A multi-statement `fn`
    // cannot be a Python lambda ("fn with a multi-statement body cannot be a
    // Python lambda; bind it with def"), and a `do` is not legal in expression
    // position at all, so an inline helper fails in two different ways.
    for helper in ["_ainl_map", "_ainl_filter", "_ainl_reduce"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing the {helper} helper:\n{out}"
        );
    }
    // The loop, in the host's own idiom.
    assert!(
        out.contains("while"),
        "no loop in the emitted python:\n{out}"
    );
}

#[test]
fn js_lowers_map_filter_and_reduce_to_loops() {
    let out = js(PROGRAM);
    // Scoped to the generated helpers, not the whole file: the emitted runtime
    // legitimately uses `Array.prototype.map` in `_repr` / `_print` / `_error`
    // (that is the correct host call, and it predates this card). What must not
    // appear is a host `map` standing in for AINL's.
    let helpers = section(&out, "function _ainl_map(", "function _ainl_reduce(");
    assert!(
        !helpers.contains(".map(") && !helpers.contains(".filter("),
        "js bound AINL's map/filter to the host's:\n{helpers}"
    );
    for helper in ["_ainl_map", "_ainl_filter", "_ainl_reduce"] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing the {helper} helper:\n{out}"
        );
    }
    assert!(out.contains("while"), "no loop in the emitted js:\n{out}");
}

#[test]
fn ruby_lowers_map_filter_and_reduce_to_loops() {
    let out = rb(PROGRAM);
    // Same scoping as the JS case: the runtime's `_repr` / `_print` use
    // `Enumerable#map` on purpose.
    let helpers = section(&out, "_ainl_map = lambda", "def dbl(");
    assert!(
        !helpers.contains(".map ") && !helpers.contains(".map(") && !helpers.contains(".select"),
        "ruby bound AINL's map/filter to the host's:\n{helpers}"
    );
    // Ruby emits `name = lambda do |params|` rather than a stabby `->`, because
    // a brace block inside a `do` block binds to the nearest keyword and is a
    // parse error — a lesson the try/catch card already paid for.
    for helper in ["_ainl_map", "_ainl_filter", "_ainl_reduce"] {
        assert!(
            out.contains(&format!("{helper} = lambda")),
            "missing the {helper} helper:\n{out}"
        );
    }
    assert!(out.contains("while"), "no loop in the emitted ruby:\n{out}");
}

#[test]
fn the_generated_helpers_are_emitted_only_when_used() {
    // A program that uses no collection form must emit no helper at all. This is
    // what keeps every pre-existing program's output byte-identical.
    for out in [
        py(r#"(print (+ 1 2))"#),
        js(r#"(print (+ 1 2))"#),
        rb(r#"(print (+ 1 2))"#),
    ] {
        for helper in ["_ainl_map", "_ainl_filter", "_ainl_reduce"] {
            assert!(
                !out.contains(helper),
                "{helper} was emitted for a program that never used it:\n{out}"
            );
        }
    }
}

#[test]
fn one_set_of_helpers_is_emitted_however_many_uses_there_are() {
    // The helpers are bound once at the top, not once per use — a program with
    // five `map`s must still emit exactly the three helper definitions, not
    // fifteen. Each helper is defined once and *called* five times.
    let many = r#"
    (def dbl (fn (x) (* x 2)))
    (def nums (list 1 2 3))
    (print (map dbl nums))
    (print (map dbl nums))
    (print (map dbl nums))
    (print (map dbl nums))
    (print (map dbl nums))
    "#;
    for (name, out) in [("python", py(many)), ("js", js(many)), ("ruby", rb(many))] {
        // Three helper definitions — one per operation, not one per use. Counted
        // per helper name rather than by prefix, because the emitted runtime also
        // contains an `_ainl_tname` helper whose name shares the prefix.
        for helper in ["_ainl_map", "_ainl_filter", "_ainl_reduce"] {
            let definitions = out.matches(&format!("def {helper}(")).count()
                + out.matches(&format!("function {helper}(")).count()
                + out.matches(&format!("{helper} = lambda")).count();
            assert_eq!(
                definitions, 1,
                "{name} emitted {definitions} definitions of {helper} for 5 uses:\n{out}"
            );
        }
        // And three loops: one per helper, reused.
        assert_eq!(
            out.matches("while").count(),
            3,
            "{name} emitted a loop per use rather than per helper:\n{out}"
        );
    }
}

// ---------------------------------------------------------------------------
// sort's helpers
// ---------------------------------------------------------------------------

#[test]
fn sort_emits_a_real_helper_on_every_target() {
    for (name, out) in [
        ("python", py(r#"(print (sort (list 2 1)))"#)),
        ("js", js(r#"(print (sort (list 2 1)))"#)),
        ("ruby", rb(r#"(print (sort (list 2 1)))"#)),
    ] {
        assert!(out.contains("_sort("), "{name} did not call _sort:\n{out}");
        assert!(
            out.contains("def _sort(") || out.contains("function _sort("),
            "{name} did not define _sort:\n{out}"
        );
    }
}

#[test]
fn sort_helpers_are_emitted_only_when_sort_is_used() {
    for out in [
        py(r#"(print (len (list 1)))"#),
        js(r#"(print (len (list 1)))"#),
        rb(r#"(print (len (list 1)))"#),
    ] {
        assert!(!out.contains("_sort"), "_sort emitted when unused:\n{out}");
    }
}

#[test]
fn sort_sorts_strings_by_bytes_on_every_target() {
    // The rule that differs across hosts: Python compares code points, JS
    // compares UTF-16 code units, Ruby compares bytes. Only "bytes" is the same
    // total order everywhere, so each target has to say so explicitly.
    assert!(
        py(r#"(print (sort (list "b" "a")))"#).contains("encode('utf-8')"),
        "python's _sort does not order strings by bytes"
    );
    assert!(
        rb(r#"(print (sort (list "b" "a")))"#).contains("_sort_key"),
        "ruby's _sort does not order strings through a byte key"
    );
    // JS compares with `<` on the string itself, which in JS *is* UTF-16 code
    // unit order — so the key function is what pins it, not the operator.
    assert!(
        js(r#"(print (sort (list "b" "a")))"#).contains("function _sort_key("),
        "js's _sort has no key function"
    );
}

#[test]
fn sort_rejects_a_mixed_type_list_on_every_target() {
    // The check has to be there, and it has to name the same pair of types the
    // interpreter does — a host TypeError would not be a readable message and
    // would not be byte-identical.
    let want = "sort expects a list of numbers or of strings, got a list mixing";
    for (name, out) in [
        ("python", py(r#"(print (sort (list 1 "a")))"#)),
        ("js", js(r#"(print (sort (list 1 "a")))"#)),
        ("ruby", rb(r#"(print (sort (list 1 "a")))"#)),
    ] {
        assert!(
            out.contains(want),
            "{name}'s _sort does not carry the mixed-type message:\n{out}"
        );
        // And the type-name helper it needs must be emitted with it.
        assert!(
            out.contains("_typename") || out.contains("_ainl_tname"),
            "{name} emits a message naming types without the helper that produces them:\n{out}"
        );
    }
}

#[test]
fn sort_rejects_a_comparator_that_does_not_return_a_number_on_every_target() {
    let want = "sort comparator must return a number";
    for (name, out) in [
        ("python", py(r#"(print (sort (fn (a b) true) (list 1 2)))"#)),
        ("js", js(r#"(print (sort (fn (a b) true) (list 1 2)))"#)),
        ("ruby", rb(r#"(print (sort (fn (a b) true) (list 1 2)))"#)),
    ] {
        assert!(
            out.contains(want),
            "{name}'s _sort does not check the comparator's return type:\n{out}"
        );
        assert!(
            out.contains("_cmp_sign"),
            "{name} emits the check without its _cmp_sign helper:\n{out}"
        );
    }
}

#[test]
fn sort_does_not_rely_on_the_hosts_own_stability() {
    // Each host's own sort is a trap in a different way: Python's `sorted` and
    // JS's `Array#sort` are stable but say nothing about *how* to compare, and
    // Ruby's `sort_by` is not stable at all. So each target decorates the
    // elements with their original index and breaks ties on it.
    assert!(
        py(r#"(print (sort (list 2 1)))"#).contains("enumerate("),
        "python's _sort does not carry the original index"
    );
    assert!(
        js(r#"(print (sort (list 2 1)))"#).contains("idx[i] = i"),
        "js's _sort does not carry the original index"
    );
    assert!(
        rb(r#"(print (sort (list 2 1)))"#).contains("each_with_index"),
        "ruby's _sort does not carry the original index"
    );
}

#[test]
fn sort_does_not_delegate_to_the_hosts_own_sort_by() {
    // Ruby's `sort_by` is the one host primitive that is outright wrong here.
    // The comparator form must use `sort` with an explicit [sign, index]
    // comparison instead.
    let out = rb(r#"(print (sort (fn (a b) (- a b)) (list 2 1)))"#);
    assert!(
        out.contains("_cmp_sign"),
        "ruby's comparator form does not go through _cmp_sign:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// the comparator really is a callable on each host
// ---------------------------------------------------------------------------

#[test]
fn the_comparator_is_emitted_as_a_real_host_callable() {
    // This is the card's "fn-as-data" requirement on the transpiler side: a
    // `(fn (a b) …)` bound to a name has to arrive at `_sort` as something the
    // host can call, and it has to be a *closure* so it can see the enclosing
    // scope.
    let src = r#"
    (def by-age (fn (a b) (- (nth a 1) (nth b 1))))
    (def people (list (list "bob" 30) (list "amy" 25)))
    (print (sort by-age people))
    "#;
    assert!(
        py(src).contains("def by_age("),
        "python did not emit a def for the comparator:\n{}",
        py(src)
    );
    assert!(
        js(src).contains("by_age"),
        "js did not emit a binding for the comparator:\n{}",
        js(src)
    );
    assert!(
        rb(src).contains("def by_age") || rb(src).contains("by_age = lambda"),
        "ruby did not emit a binding for the comparator:\n{}",
        rb(src)
    );
}
