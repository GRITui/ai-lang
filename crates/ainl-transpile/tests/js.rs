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
    //
    // The condition goes through `_truthy`, NOT the bare host `?:`. AINL says
    // only `nil` and `false` are falsey, so `(if 0 a b)` must take the `a`
    // branch — but `0 ? a : b` in JS takes `b`. This is the assertion that
    // changed when `_truthy` was introduced; the ternary shape did not.
    assert!(
        js("(print (if (< n 2) n 0))").contains("(_truthy((n < 2)) ? n : 0)"),
        "got:\n{}",
        js("(print (if (< n 2) n 0))")
    );
}

#[test]
fn if_uses_ainl_truthiness_not_javascripts() {
    // The reason `if` goes through `_truthy` at all. In JS, `0` and `""` are
    // falsey; in AINL they are TRUTHY. A bare `?:`/`if` would silently make
    // every `(if 0 ...)` take the else branch on this target only.
    let out = js("(print (if 0 \"a\" \"b\"))");
    assert!(
        out.contains("_truthy(0)"),
        "an `if` on 0 must be guarded, got:\n{out}"
    );
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

#[test]
fn and_emits_the_conjunction_not_the_disjunction() {
    // Regression. `shared::logic` receives the HOST operator (`&&` / `||`) but
    // chose its identity and its final join by testing `op == "and"` — the AINL
    // form name, which never arrives. Both tests were therefore always false,
    // so every `and` took the `or` branch and was emitted as `||`.
    //
    // It passed silently on any `and` whose operands were already boolean, and
    // blew up otherwise: `(and (not (= self nil)) (= (file-exists self) nil))`
    // became a disjunction, which does not short-circuit, so the second
    // operand ran with a nil path and the host raised a TypeError.
    let out = js("(print (and a b))");
    // The conjunction is a conditional whose `else` arm is the falsey operand
    // `a`, so a falsey `a` is returned unchanged. A disjunction would have put
    // `b` there instead.
    assert!(out.contains("_print((_truthy(a) ? b : a))"), "got:\n{out}");
    // Scoped to the emitted expression: `_truthy`'s own body contains `||`, so
    // a whole-file `!contains` would fail on the helper's null check.
    let expr = out.lines().find(|l| l.contains("_print(")).unwrap();
    assert!(
        !expr.contains("||"),
        "an `and` must not be a disjunction, got:\n{expr}"
    );
    assert!(
        !expr.contains("&&"),
        "an `and` must not be a boolean fold, got:\n{expr}"
    );
}

#[test]
fn and_or_return_an_operand_not_a_boolean() {
    // AINL's `and` yields the first FALSEY OPERAND and `or` the first TRUTHY
    // OPERAND. A host `&&` / `||` yields a boolean, so the whole chain has to
    // be a conditional: `_truthy(a) ? b : a` for `and`, `_truthy(a) ? a : b` for
    // `or`. Coercing the operands to booleans and returning one loses the
    // value: `(or 0 "")` must be `0` (0 is truthy in AINL), and a boolean fold
    // answers `true`.
    let out = js(r#"(print (or 0 ""))"#);
    assert!(
        out.contains(r#"_print((_truthy(0) ? 0 : ""))"#),
        "got:\n{out}"
    );
    let expr = out.lines().find(|l| l.contains("_print(")).unwrap();
    assert!(
        !expr.contains("||"),
        "`or` must not be a boolean fold, got:\n{expr}"
    );
    assert!(
        !expr.contains("&&"),
        "`or` must not be a boolean fold, got:\n{expr}"
    );
}

#[test]
fn and_returns_the_falsey_operand_unchanged() {
    // `(and nil false 3)` is `nil` in AINL, because the chain returns the
    // first falsey operand and `nil` is the first one. A host `&&` would
    // answer `false`.
    let out = js("(print (and nil false 3))");
    assert!(
        out.contains("_truthy(null) ? (_truthy(false) ? 3 : false) : null"),
        "got:\n{out}"
    );
    let expr = out.lines().find(|l| l.contains("_print(")).unwrap();
    assert!(!expr.contains("&&"), "got:\n{expr}");
}

#[test]
fn logic_emits_the_truthy_helper_it_calls() {
    // Regression. `logic` is generic over `ExprEmit`, so it could not call a
    // target-specific `need("_truthy")`. It called none, and JS/Ruby have no
    // other reason to emit the helper, so a program using `(and ...)` with no
    // `if` anywhere transpiled to source referencing an undefined function
    // and died at run time with `ReferenceError: _truthy is not defined`.
    let out = js("(print (and a b))");
    assert!(
        out.contains("function _truthy("),
        "the emitted program calls _truthy but never defines it, got:\n{out}"
    );
}
