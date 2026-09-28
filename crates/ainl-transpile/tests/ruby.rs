use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn function_becomes_lambda() {
    let out = rb("(def sq (fn (x) (* x x)))");
    assert!(out.contains("sq = lambda do |x|"), "got:\n{out}");
    // `*` goes through the checked `_mul` helper, not Ruby's bare `*`: a
    // `catch` binds the message it sees, and Ruby's `1 * "s"` raises an
    // `ArgumentError` with a message that is not AINL's.
    assert!(out.contains("_mul(x, x)"), "got:\n{out}");
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
    // In expression position (here, an argument) `if` lowers to a ternary, and
    // the condition goes through `_truthy` rather than the bare host value.
    // Ruby happens to agree with AINL that `0` and `""` are truthy, but by
    // coincidence rather than by contract — so the guard is emitted here too,
    // and this pins it. (It also silences Ruby's "string literal in
    // condition" warning.)
    assert!(
        rb("(print (if (< n 2) n 0))").contains("(_truthy((n < 2)) ? n : 0)"),
        "got:\n{}",
        rb("(print (if (< n 2) n 0))")
    );
}

#[test]
fn and_emits_the_conjunction_not_the_disjunction() {
    // Regression. `shared::logic` receives the HOST operator (`&&` / `||`) but
    // chose its final join by testing `op == "and"` — the AINL form name, which
    // never arrives. Both tests were therefore always false, so every `and`
    // took the `or` branch and was emitted as a disjunction. It passed on any
    // `and` whose operands were already boolean and blew up otherwise.
    let out = rb("(print (and a b))");
    assert!(
        out.contains("_print((_truthy(a) ? b : a))"),
        "an `and` must not be emitted as a disjunction, got:\n{out}"
    );
    // Scoped to the emitted expression: `_truthy`'s own body uses `nil?` and
    // `==`, but a whole-file check is still too broad to read as a statement
    // about this form.
    let expr = out
        .lines()
        .find(|l| l.contains("_print(") && l.contains("?"))
        .unwrap();
    assert!(!expr.contains("||"), "got:\n{expr}");
    assert!(!expr.contains("&&"), "got:\n{expr}");
}

#[test]
fn and_or_return_an_operand_not_a_boolean() {
    // AINL's `and` yields the first FALSEY OPERAND and `or` the first TRUTHY
    // OPERAND. A host `&&` / `||` yields a boolean, so the chain is a
    // conditional: `_truthy(a) ? b : a` for `and`, `_truthy(a) ? a : b` for
    // `or`. `(or 0 "")` is `0` in AINL because `0` is truthy.
    let out = rb(r#"(print (or 0 ""))"#);
    assert!(
        out.contains(r#"_print((_truthy(0) ? 0 : ""))"#),
        "got:\n{out}"
    );
    // `(and nil false 3)` is `nil`, not `false`: the first falsey operand wins.
    let out = rb("(print (and nil false 3))");
    assert!(
        out.contains("_print((_truthy(nil) ? (_truthy(false) ? 3 : false) : nil))"),
        "got:\n{out}"
    );
}

#[test]
fn logic_emits_the_truthy_helper_it_calls() {
    // `(and …)`/`(or …)` in a program with no `if` anywhere still has to ship
    // the `_truthy` definition it references, or the emitted program dies with
    // `undefined method '_truthy'` at run time.
    let out = rb("(print (and a b))");
    assert!(out.contains("def _truthy("), "got:\n{out}");
}

#[test]
fn hash_builtins_dispatch_to_runtime_helpers() {
    let out = rb(r#"(get (assoc (hash "a" 1) "b" 2) "a")"#);
    assert!(out.contains("_get(_assoc(_hash("), "got:\n{out}");
    assert!(out.contains("def _hash("), "got:\n{out}");
    assert!(out.contains("def _get("), "got:\n{out}");
    assert!(out.contains("def _assoc("), "got:\n{out}");
    assert!(out.contains("class AHash < Array"), "got:\n{out}");
}

#[test]
fn hash_runtime_omitted_when_unused() {
    // `_add` needs `_ainl_tname` to name a bad operand's type, and `_ainl_tname`
    // branches on `is_a?(AHash)` — so a program that only does arithmetic now
    // legitimately carries the class. What must still be omitted is the hash
    // *constructor* and the hash *builtins*.
    let out = rb("(+ 1 2)");
    assert!(!out.contains("def _hash("), "got:\n{out}");
    assert!(!out.contains("def _get("), "got:\n{out}");
    assert!(!out.contains("def _assoc("), "got:\n{out}");
}
