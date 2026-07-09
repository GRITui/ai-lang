use ainl_transpile::transpile_python_src;

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn function_becomes_def() {
    let out = py("(def sq (fn (x) (* x x)))");
    assert!(out.contains("def sq(x):"), "got:\n{out}");
    assert!(out.contains("return (x * x)"), "got:\n{out}");
}

#[test]
fn recursive_if_tail_returns_conditional() {
    let out = py("(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))");
    assert!(out.contains("def fib(n):"), "got:\n{out}");
    // the tail `if` lowers to an if/else with returns in both branches
    assert!(out.contains("if (n < 2):"), "got:\n{out}");
    assert!(out.contains("return n"), "got:\n{out}");
}

#[test]
fn while_and_let_are_statements() {
    let out = py("(def f (fn (n) (let ((i 0)) (while (< i n) (def i (+ i 1))) i)))");
    assert!(out.contains("i = 0"), "got:\n{out}");
    assert!(out.contains("while (i < n):"), "got:\n{out}");
    assert!(out.contains("i = (i + 1)"), "got:\n{out}");
    assert!(out.contains("return i"), "got:\n{out}");
}

#[test]
fn lambda_in_expression_position() {
    let out = py("(map (fn (x) (* x 2)) xs)");
    assert!(out.contains("(lambda x: (x * 2))"), "got:\n{out}");
}

#[test]
fn chained_comparison_and_names_sanitized() {
    assert!(py("(< 1 2 3)").contains("(1 < 2 < 3)"));
    let out = py("(def fib-iter (fn (n) n))");
    assert!(out.contains("def fib_iter(n):"), "got:\n{out}");
}

#[test]
fn only_used_runtime_is_emitted() {
    let out = py("(+ 1 2)");
    assert!(!out.contains("def _print"), "runtime should be omitted:\n{out}");
    let out2 = py("(print 1)");
    assert!(out2.contains("def _print"), "got:\n{out2}");
    assert!(out2.contains("def _disp"), "display dep pulled in:\n{out2}");
}

#[test]
fn while_in_expression_position_errors() {
    // `while` has no Python expression form; using it as a value must error.
    assert!(transpile_python_src("(+ 1 (while true 1))").is_err());
}
