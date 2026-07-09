use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn function_becomes_declaration() {
    let out = js("(def sq (fn (x) (* x x)))");
    assert!(out.contains("function sq(x) {"), "got:\n{out}");
    assert!(out.contains("return (x * x);"), "got:\n{out}");
}

#[test]
fn chained_comparison_expands_to_and() {
    // JS has no chained comparison — must expand.
    assert!(js("(< 1 2 3)").contains("(1 < 2 && 2 < 3)"));
    assert!(js("(= 2 2)").contains("(2 === 2)"));
}

#[test]
fn if_expression_is_ternary() {
    // In expression position (here, an argument) `if` lowers to a ternary.
    assert!(js("(print (if (< n 2) n 0))").contains("((n < 2) ? n : 0)"));
}

#[test]
fn lambda_is_arrow() {
    assert!(js("(map (fn (x) (* x 2)) xs)").contains("((x) => (x * 2))"));
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
        out.contains("i = (i + 1);") || out.contains("var i = (i + 1);"),
        "got:\n{out}"
    );
}

#[test]
fn only_used_runtime_is_emitted() {
    assert!(!js("(+ 1 2)").contains("function _print"));
    assert!(js("(print 1)").contains("function _print"));
}
