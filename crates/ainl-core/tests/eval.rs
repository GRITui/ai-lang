use ainl_core::{run_str, Value};

fn eval(src: &str) -> Value {
    run_str(src).unwrap_or_else(|e| panic!("eval failed for `{src}`: {e}"))
}

#[test]
fn arithmetic_stays_integer() {
    assert_eq!(eval("(+ 1 2 3)"), Value::Int(6));
    assert_eq!(eval("(- 10 3 2)"), Value::Int(5));
    assert_eq!(eval("(* 2 3 4)"), Value::Int(24));
}

#[test]
fn division_and_floats_promote() {
    assert_eq!(eval("(/ 10 4)"), Value::Float(2.5));
    assert_eq!(eval("(+ 1 2.5)"), Value::Float(3.5));
}

#[test]
fn comparisons_are_chained() {
    assert_eq!(eval("(< 1 2 3)"), Value::Bool(true));
    assert_eq!(eval("(< 1 3 2)"), Value::Bool(false));
    assert_eq!(eval("(= 2 2 2)"), Value::Bool(true));
}

#[test]
fn if_and_truthiness() {
    assert_eq!(eval("(if true 1 2)"), Value::Int(1));
    assert_eq!(eval("(if nil 1 2)"), Value::Int(2));
    assert_eq!(eval("(if false 1 2)"), Value::Int(2));
    assert_eq!(eval("(if 0 1 2)"), Value::Int(1)); // 0 is truthy
}

#[test]
fn closures_and_recursion() {
    let src = "(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))) (fib 20)";
    assert_eq!(eval(src), Value::Int(6765));
}

#[test]
fn let_scoping() {
    assert_eq!(eval("(let ((a 2) (b 3)) (+ a b))"), Value::Int(5));
}

#[test]
fn variadic_and_lists() {
    assert_eq!(eval("(len (list 1 2 3))"), Value::Int(3));
    let src = "(def f (fn (& xs) (len xs))) (f 1 2 3 4)";
    assert_eq!(eval(src), Value::Int(4));
}

#[test]
fn while_loop_mutation() {
    let src = "(let ((i 0)) (while (< i 5) (def i (+ i 1))) i)";
    assert_eq!(eval(src), Value::Int(5));
}

#[test]
fn quote_makes_data() {
    // a quoted list of symbols is data, not a function call
    assert_eq!(eval("(len (quote (a b c)))"), Value::Int(3));
}

#[test]
fn errors_surface() {
    assert!(run_str("(+ 1 nope)").is_err()); // unbound symbol
    assert!(run_str("(/ 1 0)").is_err()); // division by zero
    assert!(run_str("(1 2 3)").is_err()); // calling a non-fn
}
