use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn function_becomes_declaration() {
    let out = js("(def sq (fn (x) (* x x)))");
    assert!(out.contains("function sq(x) {"), "got:\n{out}");
    // `*` goes through the checked `_mul` helper, not JS's bare `*`: a `catch`
    // binds the message it sees, and JS's `1 * "s"` is `NaN` with no error at
    // all while AINL rejects the operand.
    assert!(out.contains("return _mul(x, x);"), "got:\n{out}");
}

#[test]
fn chained_comparison_expands_to_and() {
    // JS has no chained comparison — must expand.
    assert!(js("(< 1 2 3)").contains("(1 < 2 && 2 < 3)"));
    assert!(js("(= 2 2)").contains("(_eq(2, 2))"));
}

#[test]
fn equality_uses_structural_eq_not_reference_identity() {
    // `===` on JS arrays is reference identity, diverging from the
    // interpreter's (and Python/Ruby's) structural list equality. `=` must
    // go through the `_eq` runtime helper instead of a bare `===`.
    let out = js("(= (list 1 2) (list 1 2))");
    assert!(out.contains("_eq([1, 2], [1, 2])"), "got:\n{out}");
    assert!(out.contains("function _eq("), "got:\n{out}");
}

#[test]
fn eq_helper_omitted_when_equality_unused() {
    let out = js("(+ 1 2)");
    assert!(!out.contains("function _eq("), "got:\n{out}");
}

#[test]
fn if_expression_is_ternary() {
    // In expression position (here, an argument) `if` lowers to a ternary.
    assert!(js("(print (if (< n 2) n 0))").contains("((n < 2) ? n : 0)"));
}

#[test]
fn lambda_is_arrow() {
    assert!(js("(map (fn (x) (* x 2)) xs)").contains("((x) => _mul(x, 2))"));
}

#[test]
fn variadic_is_rest_param() {
    let out = js("(def f (fn (& xs) (len xs)))");
    assert!(out.contains("function f(...xs) {"), "got:\n{out}");
}

#[test]
fn while_and_let_braces() {
    let out = js("(def f (fn (n) (let ((i 0)) (while (< i n) (def i (+ i 1))) i)))");
    assert!(out.contains("var i = 0;"), "got:\n{out}");
    assert!(out.contains("while ((i < n)) {"), "got:\n{out}");
    assert!(
        out.contains("i = _add(i, 1);") || out.contains("var i = _add(i, 1);"),
        "got:\n{out}"
    );
}

#[test]
fn only_used_runtime_is_emitted() {
    assert!(!js("(+ 1 2)").contains("function _print"));
    assert!(js("(print 1)").contains("function _print"));
}

#[test]
fn hash_builtins_dispatch_to_runtime_helpers() {
    let out = js(r#"(get (assoc (hash "a" 1) "b" 2) "a")"#);
    assert!(out.contains("_get(_assoc(_hash("), "got:\n{out}");
    assert!(out.contains("function _hash("), "got:\n{out}");
    assert!(out.contains("function _get("), "got:\n{out}");
    assert!(out.contains("function _assoc("), "got:\n{out}");
    assert!(out.contains("class _Hash extends Array"), "got:\n{out}");
}

#[test]
fn keys_and_vals_return_plain_arrays_not_hashes() {
    // Regression: Array.prototype.map on a _Hash instance returns another
    // _Hash (Symbol.species), which would make `_disp` wrongly render a
    // plain key/value list as a "{...}" hash. _keys/_vals must use
    // Array.from to force a plain array.
    let out = js("(keys h)");
    assert!(out.contains("Array.from(h, p => p[0])"), "got:\n{out}");
    let out2 = js("(vals h)");
    assert!(out2.contains("Array.from(h, p => p[1])"), "got:\n{out2}");
}

#[test]
fn hash_runtime_omitted_when_unused() {
    // `_add` needs `_ainl_tname` to name a bad operand's type, and `_ainl_tname`
    // branches on `instanceof _Hash` — so a program that only does arithmetic
    // now legitimately carries the class. What must still be omitted is the
    // hash *constructor* and the hash *builtins*, which nothing calls.
    let out = js("(+ 1 2)");
    assert!(!out.contains("function _hash("), "got:\n{out}");
    assert!(!out.contains("function _get("), "got:\n{out}");
    assert!(!out.contains("function _assoc("), "got:\n{out}");
}
