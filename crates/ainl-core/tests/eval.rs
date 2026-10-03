use ainl_core::{parse_to_json, run_str, BigNum, ConsCell, LineIndex, Value};

fn eval(src: &str) -> Value {
    run_str(src).unwrap_or_else(|e| panic!("eval failed for `{src}`: {e}"))
}

#[test]
fn arithmetic_stays_integer() {
    assert_eq!(eval("(+ 1 2 3)"), Value::Int(BigNum::small(6)));
    assert_eq!(eval("(- 10 3 2)"), Value::Int(BigNum::small(5)));
    assert_eq!(eval("(* 2 3 4)"), Value::Int(BigNum::small(24)));
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
    assert_eq!(eval("(if true 1 2)"), Value::Int(BigNum::small(1)));
    assert_eq!(eval("(if nil 1 2)"), Value::Int(BigNum::small(2)));
    assert_eq!(eval("(if false 1 2)"), Value::Int(BigNum::small(2)));
    assert_eq!(eval("(if 0 1 2)"), Value::Int(BigNum::small(1))); // 0 is truthy
}

#[test]
fn closures_and_recursion() {
    let src = "(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))) (fib 20)";
    assert_eq!(eval(src), Value::Int(BigNum::small(6765)));
}

#[test]
fn let_scoping() {
    assert_eq!(
        eval("(let ((a 2) (b 3)) (+ a b))"),
        Value::Int(BigNum::small(5))
    );
}

#[test]
fn variadic_and_lists() {
    assert_eq!(eval("(len (list 1 2 3))"), Value::Int(BigNum::small(3)));
    let src = "(def f (fn (& xs) (len xs))) (f 1 2 3 4)";
    assert_eq!(eval(src), Value::Int(BigNum::small(4)));
}

#[test]
fn hash_basics() {
    assert_eq!(
        eval(r#"(get (hash "a" 1 "b" 2) "a")"#),
        Value::Int(BigNum::small(1))
    );
    assert_eq!(eval(r#"(get (hash "a" 1) "missing")"#), Value::Nil);
    assert_eq!(eval(r#"(has (hash "a" 1) "a")"#), Value::Bool(true));
    assert_eq!(eval(r#"(has (hash "a" 1) "z")"#), Value::Bool(false));
    assert_eq!(
        eval(r#"(len (hash "a" 1 "b" 2))"#),
        Value::Int(BigNum::small(2))
    );
}

#[test]
fn hash_assoc_does_not_mutate_the_original() {
    let src = r#"
        (def h (hash "a" 1))
        (def h2 (assoc h "a" 99))
        (list (get h "a") (get h2 "a"))
    "#;
    assert_eq!(
        eval(src),
        Value::List(ConsCell::from_values([
            Value::Int(BigNum::small(1)),
            Value::Int(BigNum::small(99))
        ]))
    );
}

#[test]
fn hash_construction_lets_a_repeated_key_keep_its_last_value() {
    // Last value wins, but only one entry — not two.
    assert_eq!(
        eval(r#"(len (hash "a" 1 "a" 2))"#),
        Value::Int(BigNum::small(1))
    );
    assert_eq!(
        eval(r#"(get (hash "a" 1 "a" 2) "a")"#),
        Value::Int(BigNum::small(2))
    );
}

#[test]
fn hash_keys_and_vals_preserve_insertion_order() {
    let src = r#"(keys (hash "a" 1 "b" 2 "c" 3))"#;
    assert_eq!(
        eval(src),
        Value::List(ConsCell::from_values([
            Value::str("a"),
            Value::str("b"),
            Value::str("c"),
        ]))
    );
}

#[test]
fn hash_equality_is_order_sensitive_like_list() {
    // A deliberate simplification (docs/SYNTAX.md §3 "Maps") that keeps
    // equality identical to List's across all four runtimes.
    assert_eq!(
        eval(r#"(= (hash "a" 1 "b" 2) (hash "a" 1 "b" 2))"#),
        Value::Bool(true)
    );
    assert_eq!(
        eval(r#"(= (hash "a" 1 "b" 2) (hash "b" 2 "a" 1))"#),
        Value::Bool(false)
    );
}

#[test]
fn hash_key_equality_distinguishes_symbol_from_string() {
    // A quoted symbol and an equal-content string are different keys, same
    // as they're different values everywhere else in the language — this
    // works "for free" because `get`/`has` reuse Value's own PartialEq.
    assert_eq!(
        eval(r#"(= (hash (quote a) 1) (hash "a" 1))"#),
        Value::Bool(false)
    );
    assert_eq!(eval(r#"(get (hash (quote a) 1) "a")"#), Value::Nil);
}

#[test]
fn while_loop_mutation() {
    let src = "(let ((i 0)) (while (< i 5) (def i (+ i 1))) i)";
    assert_eq!(eval(src), Value::Int(BigNum::small(5)));
}

// Scoping (docs/SYNTAX.md §2a): `let` and `fn` each open a fresh scope; `def`
// inside them can only shadow, never mutate an outer binding. `if`/`do`/
// `while`/`and`/`or` share the caller's scope, so `def` inside them mutates.

#[test]
fn nested_let_shadows_rather_than_mutates() {
    let src = "(let ((i 0)) (let () (def i 99)) i)";
    assert_eq!(eval(src), Value::Int(BigNum::small(0)));
}

#[test]
fn fn_body_shadows_rather_than_mutates_the_defining_scope() {
    let src = "(def counter 0) \
               (def bump (fn () (def counter (+ counter 1)) counter)) \
               (bump) (bump) counter";
    assert_eq!(eval(src), Value::Int(BigNum::small(0)));
}

#[test]
fn if_does_not_open_a_new_scope_so_def_mutates() {
    let src = "(let ((x 1)) (if true (def x 2) nil) x)";
    assert_eq!(eval(src), Value::Int(BigNum::small(2)));
}

#[test]
fn quote_makes_data() {
    // a quoted list of symbols is data, not a function call
    assert_eq!(eval("(len (quote (a b c)))"), Value::Int(BigNum::small(3)));
}

#[test]
fn errors_surface() {
    assert!(run_str("(+ 1 nope)").is_err()); // unbound symbol
    assert!(run_str("(/ 1 0)").is_err()); // division by zero
    assert!(run_str("(1 2 3)").is_err()); // calling a non-fn
}

#[test]
fn unbounded_recursion_errors_cleanly_instead_of_overflowing_the_stack() {
    // No base case: this must hit the depth guard and return `Err`, not
    // crash the process with a native stack overflow.
    let src = "(def loop (fn (n) (+ 1 (loop n)))) (loop 0)";
    let err = run_str(src).unwrap_err().to_string();
    assert!(err.contains("recursion limit"), "got: {err}");
}

#[test]
fn infinite_loop_errors_cleanly_instead_of_hanging() {
    let err = run_str("(while true 0)").unwrap_err().to_string();
    assert!(err.contains("step limit"), "got: {err}");
}

#[test]
fn each_run_in_call_gets_a_fresh_step_budget() {
    // A prior run that burns its whole step budget must not starve the next
    // call sharing the same `Env` (as the REPL does line-by-line).
    let env = ainl_core::Env::with_prelude();
    assert!(ainl_core::run_in("(while true 0)", &env).is_err());
    assert_eq!(
        ainl_core::run_in("(+ 1 2)", &env).unwrap(),
        Value::Int(BigNum::small(3))
    );
}

#[test]
fn mod_min_by_neg_one_does_not_panic() {
    // i64::MIN.rem_euclid(-1) panics in std (the quotient overflows even
    // though the true remainder is 0); AINL's `mod` must not crash.
    assert_eq!(
        eval("(mod -9223372036854775808 -1)"),
        Value::Int(BigNum::small(0))
    );
}

#[test]
fn overflow_widens_to_bignum_matching_documented_numeric_model() {
    // Pins the interpreter's half of the numeric model documented in
    // docs/NUMERIC_MODEL.md: integer overflow now WIDENS to an exact
    // arbitrary-precision int (bignum) rather than promoting to f64, matching
    // the Python/Ruby targets. A change to this value should come with an
    // update to that doc.
    assert_eq!(
        eval("(* 9223372036854775807 2)"),
        Value::Int(BigNum::from_str("18446744073709551614").unwrap())
    );
}

#[test]
fn unary_negate_widens_to_bignum_on_i64_min_overflow() {
    // -i64::MIN has no i64 representation; it widens to an exact bignum (not a
    // float, not a wrap, not a panic), like every other arithmetic op's
    // overflow path.
    assert_eq!(
        eval("(- -9223372036854775808)"),
        Value::Int(BigNum::from_str("9223372036854775808").unwrap())
    );
}

#[test]
fn line_index_locates_positions() {
    let src = "abc\n(de\nfg)";
    let idx = LineIndex::new(src);
    assert_eq!(idx.locate(0), (1, 1)); // 'a'
    assert_eq!(idx.locate(4), (2, 1)); // '(' after first newline
    assert_eq!(idx.locate(6), (2, 3)); // 'e'
    assert_eq!(idx.locate(8), (3, 1)); // 'f'
}

#[test]
fn json_carries_spans_and_locs() {
    let json = parse_to_json("(+ 1 2)", Some("t.ainl")).unwrap();
    // structural fields present
    assert!(json.contains("\"version\": \"0.1\""));
    assert!(json.contains("\"source\": \"t.ainl\""));
    assert!(json.contains("\"t\": \"list\""));
    assert!(json.contains("\"t\": \"sym\", \"v\": \"+\""));
    assert!(json.contains("\"t\": \"int\", \"v\": 1"));
    // every node carries span + loc
    assert!(json.contains("\"span\": [0, 7]")); // the whole (+ 1 2)
    assert!(json.contains("\"loc\": [1, 1]"));
}

#[test]
fn json_escapes_strings() {
    let json = parse_to_json("(print \"a\\\"b\\nc\")", None).unwrap();
    assert!(json.contains("\\\"")); // escaped quote survives
    assert!(json.contains("\\n")); // escaped newline survives
}

#[test]
fn json_is_valid_for_all_examples() {
    // A structural sanity check: balanced braces/brackets in generated JSON.
    for src in [
        "(def f (fn (x) (* x x))) (f 3)",
        "(let ((a 1) (b 2.5)) (+ a b))",
        "()",
        "(quote (a \"str\" 3 4.0))",
    ] {
        let json = parse_to_json(src, None).unwrap();
        let braces = json.matches('{').count() as i64 - json.matches('}').count() as i64;
        let brackets = json.matches('[').count() as i64 - json.matches(']').count() as i64;
        assert_eq!(braces, 0, "unbalanced braces for `{src}`");
        assert_eq!(brackets, 0, "unbalanced brackets for `{src}`");
    }
}
