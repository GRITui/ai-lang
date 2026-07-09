use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn function_becomes_lambda() {
    let out = rb("(def sq (fn (x) (* x x)))");
    assert!(out.contains("sq = lambda do |x|"), "got:\n{out}");
    assert!(out.contains("(x * x)"), "got:\n{out}");
    assert!(out.contains("end"), "got:\n{out}");
}

#[test]
fn calls_use_dot_call() {
    // user functions are lambdas → called with .call
    assert!(rb("(fib 10)").contains("fib.call(10)"));
}

#[test]
fn float_division_coerces() {
    assert!(rb("(/ 10 4)").contains(".to_f"), "got:\n{}", rb("(/ 10 4)"));
}

#[test]
fn chained_comparison_expands() {
    assert!(rb("(< 1 2 3)").contains("(1 < 2 && 2 < 3)"));
}

#[test]
fn quoted_symbol_is_ruby_symbol() {
    assert!(rb("(quote (a b))").contains(":\"a\""));
}

#[test]
fn variadic_uses_splat() {
    assert!(rb("(def f (fn (& xs) (len xs)))").contains("lambda do |*xs|"));
}

#[test]
fn if_expression_is_ternary() {
    assert!(rb("(print (if (< n 2) n 0))").contains("((n < 2) ? n : 0)"));
}
