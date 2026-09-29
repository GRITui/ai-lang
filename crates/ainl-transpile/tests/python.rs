use ainl_transpile::transpile_python_src;

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

/// The emitted `_print(...)` CALL, as one line.
///
/// A plain `lines().find(|l| l.contains("_print("))` finds the runtime helper's
/// own `def _print(...)` definition first, so an assertion written against it
/// reads a line that has nothing to do with the program. This matches a line
/// that *calls* it.
fn print_call(out: &str) -> String {
    out.lines()
        .find(|l| l.contains("_print(") && !l.contains("def _print"))
        .expect("no _print call in the emitted program")
        .to_string()
}

#[test]
fn function_becomes_def() {
    let out = py("(def sq (fn (x) (* x x)))");
    assert!(out.contains("def sq(x):"), "got:\n{out}");
    // `*` goes through the checked `_mul` helper, not Python's bare `*`: a
    // `catch` binds the message it sees, and Python's `1 * "s"` is a repeated
    // string with no error at all while AINL rejects the operand.
    assert!(out.contains("return _mul(x, x)"), "got:\n{out}");
}

#[test]
fn recursive_if_tail_returns_conditional() {
    let out = py("(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))");
    assert!(out.contains("def fib(n):"), "got:\n{out}");
    // the tail `if` lowers to an if/else with returns in both branches.
    // The condition is wrapped in `_truthy` (not the bare host value): AINL
    // treats `0` and `""` as TRUTHY, Python treats both as falsey, so an
    // unguarded `if` would take the wrong branch on this target alone.
    assert!(out.contains("if _truthy((n < 2)):"), "got:\n{out}");
    assert!(out.contains("return n"), "got:\n{out}");
}

#[test]
fn if_uses_ainl_truthiness_not_pythons() {
    // In Python both `0` and `""` are falsey; in AINL they are TRUTHY. A bare
    // host `if` would make every `(if 0 ...)` take the else branch here only.
    let out = py("(print (if 0 \"a\" \"b\"))");
    assert!(out.contains("_truthy(0)"), "got:\n{out}");
}

#[test]
fn while_and_let_are_statements() {
    let out = py("(def f (fn (n) (let ((i 0)) (while (< i n) (def i (+ i 1))) i)))");
    assert!(out.contains("i = 0"), "got:\n{out}");
    assert!(out.contains("while (i < n):"), "got:\n{out}");
    assert!(out.contains("i = _add(i, 1)"), "got:\n{out}");
    assert!(out.contains("return i"), "got:\n{out}");
}

#[test]
fn lambda_in_expression_position() {
    let out = py("(map (fn (x) (* x 2)) xs)");
    assert!(out.contains("lambda x: _mul(x, 2)"), "got:\n{out}");
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
    assert!(
        !out.contains("def _print"),
        "runtime should be omitted:\n{out}"
    );
    let out2 = py("(print 1)");
    assert!(out2.contains("def _print"), "got:\n{out2}");
    assert!(out2.contains("def _disp"), "display dep pulled in:\n{out2}");
}

#[test]
fn while_in_expression_position_errors() {
    // `while` has no Python expression form; using it as a value must error.
    assert!(transpile_python_src("(+ 1 (while true 1))").is_err());
}

#[test]
fn equality_uses_eq_helper_not_bare_python_equals() {
    // Bare `==` is wrong for `=`: `_Sym` is a `str` subclass (so
    // `_sym("a") == "a"` is wrongly `True`) and Python `bool` is an `int`
    // subclass (so `True == 1` is wrongly `True`). `_eq` must be used instead.
    let out = py("(= (quote a) 1)");
    assert!(out.contains("_eq(_sym(\"a\"), 1)"), "got:\n{out}");
    assert!(out.contains("def _eq("), "got:\n{out}");
}

#[test]
fn eq_helper_omitted_when_equality_unused() {
    let out = py("(+ 1 2)");
    assert!(!out.contains("def _eq("), "got:\n{out}");
}

#[test]
fn hash_builtins_dispatch_to_runtime_helpers() {
    let out = py(r#"(get (assoc (hash "a" 1) "b" 2) "a")"#);
    assert!(out.contains("_get(_assoc(_hash("), "got:\n{out}");
    assert!(out.contains("def _hash("), "got:\n{out}");
    assert!(out.contains("def _get("), "got:\n{out}");
    assert!(out.contains("def _assoc("), "got:\n{out}");
    assert!(out.contains("class _Hash(list)"), "got:\n{out}");
}

#[test]
fn hash_runtime_omitted_when_unused() {
    // `_add` needs `_ainl_tname` to name a bad operand's type, and `_ainl_tname`
    // branches on `isinstance(x, _Hash)` — so a program that only does
    // arithmetic now legitimately carries the class. What must still be omitted
    // is the hash *constructor* and the hash *builtins*.
    let out = py("(+ 1 2)");
    assert!(!out.contains("def _hash("), "got:\n{out}");
    assert!(!out.contains("def _get("), "got:\n{out}");
    assert!(!out.contains("def _assoc("), "got:\n{out}");
}

#[test]
fn and_or_return_an_operand_not_a_boolean() {
    // AINL's `and` yields the first FALSEY OPERAND and `or` the first TRUTHY
    // OPERAND. A host `and` / `or` yields an operand too, but only for the
    // *last* one it evaluates and never with AINL's truthiness: `(or 0 "")` is
    // `0` in AINL (`0` is truthy) and `False` in Python (`0` is falsey). So the
    // chain is a host conditional — `b if c else a` — with every operand but
    // the last guarded by `_truthy` and bound so it is evaluated once.
    let out = py(r#"(print (or 0 ""))"#);
    assert!(
        out.contains(r#"(lambda _ainl_t1: (_ainl_t1 if _truthy(_ainl_t1) else (lambda _ainl_t0: (_ainl_t0 if _truthy(_ainl_t0) else False))("")))(0))"#),
        "got:\n{out}"
    );
    // `(and nil false 3)` is `nil`, not `False`: the first falsey operand wins.
    let out = py("(print (and nil false 3))");
    assert!(
        out.contains("(lambda _ainl_t1: ((lambda _ainl_t0: (3 if _truthy(_ainl_t0) else _ainl_t0))(False) if _truthy(_ainl_t1) else _ainl_t1))(None))"),
        "got:\n{out}"
    );
}

#[test]
fn an_all_falsy_or_ends_on_the_false_identity() {
    // SYNTAX.md 2: `or` returns the first truthy operand, and `false` when
    // there is none. The tail of the chain is therefore `False` — in PYTHON's
    // spelling, since `false` would be a `NameError` at run time on precisely
    // the case the identity exists to answer.
    let out = py("(print (or nil nil))");
    let expr = print_call(&out);
    assert!(
        expr.contains("else False)"),
        "an all-falsy `or` must fall through to `False`, got:\n{expr}"
    );
    // A lone falsey operand is the same case with no chain: `(or nil)` is
    // `false` on the interpreter, not `nil`.
    let out = py("(print (or nil))");
    let expr = print_call(&out);
    assert!(
        expr.contains("else False)"),
        "`(or nil)` must be `false`, got:\n{expr}"
    );
    // A lone TRUTHY operand still comes back unchanged — 0 is truthy in AINL.
    let out = py("(print (or 0))");
    let expr = print_call(&out);
    assert!(
        expr.contains("(0))") && expr.contains("else False)"),
        "`(or 0)` must answer 0, got:\n{expr}"
    );
}

#[test]
fn an_operand_is_evaluated_once() {
    // Each operand's text is needed twice — once in the `_truthy` test and once
    // as the value it yields — so repeating it evaluates it twice. The fold
    // binds the operand and uses only the name. A value-only assertion cannot
    // see a doubled evaluation; this counts the occurrences instead.
    let out = py("(print (or (f 1) 2))");
    let expr = print_call(&out);
    assert_eq!(
        expr.matches("f(1)").count(),
        1,
        "the operand's text appears more than once, so it runs more than once: {expr}"
    );
}

#[test]
fn a_logic_temp_cannot_shadow_a_user_binding() {
    // The chain is a nest of real closures, so a generated name that collided
    // with a program binding would shadow it. `_ainl_t0` is a legal AINL
    // identifier, so the counter has to step past one a program chose.
    let out = py("(def _ainl_t0 9)\n(print (or _ainl_t0 1))");
    assert!(
        out.contains("_ainl_t1:"),
        "the fold must not reuse a name the program bound, got:\n{out}"
    );
}

#[test]
fn logic_emits_the_truthy_helper_it_calls() {
    // `(and …)`/`(or …)` in a program with no `if` anywhere still has to ship
    // the `_truthy` definition it references, or the emitted program dies with
    // `NameError: name '_truthy' is not defined` at run time.
    let out = py("(print (and a b))");
    assert!(out.contains("def _truthy("), "got:\n{out}");
}
