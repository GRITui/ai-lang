use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

/// The emitted `_print(...)` CALL, as one line.
///
/// A plain `lines().find(|l| l.contains("_print("))` finds the runtime
/// helper's own `function _print(...xs) {...}` definition first, so every
/// assertion written against it was reading a line that has nothing to do with
/// the program — it passes whatever the program emits, and fails for reasons
/// that have nothing to do with the program either. This matches a line that
/// *calls* it, which is the only one carrying the form under test.
fn print_call(out: &str) -> String {
    out.lines()
        .find(|l| l.contains("_print(") && !l.contains("function _print"))
        .expect("no _print call in the emitted program")
        .to_string()
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
    //
    // Each clause goes through `_cmp` rather than the host `<`, because an
    // AINL int is a BigInt here and native `<` would compare a float-involving
    // pair exactly where the interpreter compares it as f64. The `n` suffixes
    // are the BigInt int literals (see `an_int_literal_is_a_bigint`).
    let out = js("(< 1 2 3)");
    assert!(
        out.contains("_cmp(1n, 2n, \"<\") && _cmp(2n, 3n, \"<\")"),
        "got:\n{out}"
    );
    assert!(js("(= 2 2)").contains("(_eq(2n, 2n))"));
}

#[test]
fn an_int_literal_is_a_bigint() {
    // The whole point of the BigInt switch: an AINL int emits a JS `BigInt`
    // literal, so the digits in the generated source are exact and no runtime
    // parse is involved. `(print 42)` must not emit a bare `42`, which would be
    // a float64 and re-introduce the rounding this target had.
    let out = js("(print 42)");
    assert!(out.contains("_print(42n);"), "got:\n{out}");
    assert!(!out.contains("_print(42);"), "got:\n{out}");
    // A float literal is still the tagged `_Float` wrapper, unchanged.
    let f = js("(print 3.0)");
    assert!(f.contains("_print(new _Float(3.0));"), "got:\n{f}");
}

#[test]
fn equality_uses_structural_eq_not_reference_identity() {
    // `===` on JS arrays is reference identity, diverging from the
    // interpreter's (and Python/Ruby's) structural list equality. `=` must
    // go through the `_eq` runtime helper instead of a bare `===`.
    let out = js("(= (list 1 2) (list 1 2))");
    assert!(out.contains("_eq([1n, 2n], [1n, 2n])"), "got:\n{out}");
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
        js("(print (if (< n 2) n 0))").contains("(_truthy((_cmp(n, 2n, \"<\"))) ? n : 0n)"),
        "got:\n{}",
        js("(print (if (< n 2) n 0))")
    );
}

#[test]
fn if_uses_ainl_truthiness_not_javascripts() {
    // The reason `if` goes through `_truthy` at all. In JS, `0` and `""` are
    // falsey; in AINL they are TRUTHY. A bare `?:`/`if` would silently make
    // every `(if 0 ...)` take the else branch on this target only.
    // `0n` is the BigInt int literal — the truthiness rule is unchanged, only
    // the spelling of the literal is.
    let out = js("(print (if 0 \"a\" \"b\"))");
    assert!(
        out.contains("_truthy(0n)"),
        "an `if` on 0 must be guarded, got:\n{out}"
    );
}

#[test]
fn lambda_is_arrow() {
    assert!(js("(map (fn (x) (* x 2)) xs)").contains("((x) => _mul(x, 2n))"));
}

#[test]
fn variadic_is_rest_param() {
    let out = js("(def f (fn (& xs) (len xs)))");
    assert!(out.contains("function f(...xs) {"), "got:\n{out}");
}

#[test]
fn while_and_let_braces() {
    let out = js("(def f (fn (n) (let ((i 0)) (while (< i n) (def i (+ i 1))) i)))");
    assert!(out.contains("var i = 0n;"), "got:\n{out}");
    assert!(out.contains("while ((_cmp(i, n, \"<\"))) {"), "got:\n{out}");
    assert!(
        out.contains("i = _add(i, 1n);") || out.contains("var i = _add(i, 1n);"),
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
    // The conjunction is a conditional whose `then` arm is the rest of the chain
    // and whose `else` arm is the falsey operand, so a falsey `a` is returned
    // unchanged. A disjunction would have put the rest in the `else` arm.
    // `a` is BOUND rather than repeated, so the `_truthy` test and the yielded
    // value read the same evaluation — see `an_operand_is_evaluated_once`.
    assert!(
        out.contains("_print(((_ainl_t0) => (_truthy(_ainl_t0) ? b : _ainl_t0))(a))"),
        "got:\n{out}"
    );
    // Scoped to the emitted expression: `_truthy`'s own body contains `||`, so
    // a whole-file `!contains` would fail on the helper's null check.
    let expr = print_call(&out);
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
fn an_operand_is_evaluated_once() {
    // Each operand's text is needed twice — once inside the `_truthy` test and
    // once as the value it yields — so repeating it evaluates it twice. The
    // fold binds the operand instead and uses only the name, which costs a
    // closure but is the only way to keep a side effect to a single run.
    //
    // This was silent before: a value-only test cannot see a doubled
    // evaluation, and the doubled one was a function call here.
    let out = js("(print (or (f 1) 2))");
    let expr = print_call(&out);
    assert_eq!(
        expr.matches("f(1n)").count(),
        1,
        "the operand's text appears more than once, so it runs more than once: {expr}"
    );
    assert!(
        expr.contains("(f(1n))"),
        "the operand should appear as the bound value exactly once, got:\n{expr}"
    );
}

#[test]
fn an_all_falsy_or_ends_on_the_false_identity() {
    // SYNTAX.md 2: `or` returns the first truthy operand, and `false` when
    // there is none. Seeding the fold with the last operand instead made
    // `(or nil nil)` answer `nil` here and `false` on the interpreter and the
    // AOT binary. `false` is therefore the tail of the chain, and the final
    // operand is tested too — otherwise a truthy one would be discarded.
    let out = js("(print (or nil nil))");
    let expr = print_call(&out);
    assert!(
        expr.contains(": false)"),
        "an all-falsy `or` must fall through to `false`, got:\n{expr}"
    );
    // A lone falsey operand is the same case with no chain to run: the
    // interpreter answers `false` for `(or nil)`, not `nil`.
    let out = js("(print (or nil))");
    let expr = print_call(&out);
    assert!(
        expr.contains(": false)"),
        "`(or nil)` must be `false`, got:\n{expr}"
    );
    // And a lone TRUTHY operand still comes back unchanged — 0 is truthy in
    // AINL (§1), so a fix that read truthiness off the emitted text, or used
    // the host's, would break this.
    // `42n` — the BigInt int literal. The truthiness rule itself is unchanged:
    // 0 is truthy in AINL, so a lone truthy operand still comes back as itself.
    let out = js("(print (or 42))");
    let expr = print_call(&out);
    assert!(
        expr.contains("(42n))") && expr.contains(": false)"),
        "`(or 42)` must answer 42, got:\n{expr}"
    );
}

#[test]
fn a_logic_temp_cannot_shadow_a_user_binding() {
    // The chain is a nest of real closures, so a generated operand name that
    // collided with a program binding would shadow it and change what the rest
    // of the operand reads. The counter therefore steps past any name the
    // program already uses. An AINL identifier is any run of non-delimiter
    // characters, so `_ainl_t0` is a legal thing for a program to `def`.
    let out = js("(def _ainl_t0 9)\n(print (or _ainl_t0 1))\n(print _ainl_t0)");
    assert!(
        out.contains("_ainl_t1)"),
        "the fold must not reuse a name the program bound, got:\n{out}"
    );
}

#[test]
fn and_or_return_an_operand_not_a_boolean() {
    // AINL's `and` yields the first FALSEY OPERAND and `or` the first TRUTHY
    // OPERAND. A host `&&` / `||` yields a boolean, so the whole chain has to
    // be a conditional over bound operands. Coercing the operands to booleans
    // and returning one loses the value: `(or 0 "")` must be `0` (0 is truthy
    // in AINL), and a boolean fold answers `true`.
    let out = js(r#"(print (or 0 ""))"#);
    assert!(
        out.contains(r#"((_ainl_t1) => (_truthy(_ainl_t1) ? _ainl_t1 : ((_ainl_t0) => (_truthy(_ainl_t0) ? _ainl_t0 : false))("")))(0n))"#),
        "got:\n{out}"
    );
    let expr = print_call(&out);
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
    // answer `false`. The last operand is the chain's value and is emitted
    // bare; the two before it are bound, so each is tested once.
    let out = js("(print (and nil false 3))");
    assert!(
        out.contains("(_ainl_t1) => (_truthy(_ainl_t1) ? ((_ainl_t0) => (_truthy(_ainl_t0) ? 3n : _ainl_t0))(false) : _ainl_t1))(null)"),
        "got:\n{out}"
    );
    let expr = print_call(&out);
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
