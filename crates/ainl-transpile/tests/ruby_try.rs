//! Ruby-side lowering for `try`/`catch`, plus the parity rules the 4-backend
//! rule depends on.
//!
//! These are *shape* tests (what the generated Ruby looks like). The
//! behavioural cross-check — same bytes out of Ruby, Python, JS, the AOT binary
//! and the interpreter — lives in `crates/ainl-core/tests/try_catch.rs` and in
//! `scripts/check-transpile.sh`, because asserting it here would only ever
//! prove one host agrees with itself.

use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn try_lowers_to_stabby_lambdas_not_do_end() {
    // The whole reason the sides are `-> () { }`: a brace block is sugar for
    // `do...end`, and `do...end` binds to the nearest keyword. Emitting
    // `lambda { }` inside a `do` block is a parse error on the host
    // ("tried to create Proc object without a block"), which is exactly the
    // failure this shape exists to prevent.
    let out = rb("(try (/ 1 0) (catch (e) 1))");
    assert!(out.contains("_ainl_try("), "got:\n{out}");
    assert!(out.contains("->() {"), "got:\n{out}");
    assert!(out.contains("->(e) {"), "got:\n{out}");
    assert!(
        !out.contains("lambda do |"),
        "a `do...end` side can be captured by an enclosing block:\n{out}"
    );
}

#[test]
fn the_stabby_form_is_never_written_without_a_parameter_list() {
    // A bare `-> body` reads as the unary minus operator plus its operand and
    // does not parse. Both sides must carry `()` or `(name)`.
    for src in [
        "(try (/ 1 0) (catch (e) 1))",
        "(def s (try (error \"x\") (catch (e) (get e \"message\"))))",
    ] {
        let out = rb(src);
        assert!(!out.contains("-> {"), "bare `-> {{` in:\n{out}");
        assert!(
            !out.contains("->(") || out.contains("->() {"),
            "got:\n{out}"
        );
    }
}

#[test]
fn try_in_expression_position_uses_an_iife() {
    // A Ruby expression cannot contain statements, so this needs an IIFE —
    // same problem `let` already solves, resolved the same way.
    let out = rb(r#"(def s (try (error "x") (catch (e) (get e "message"))))"#);
    assert!(out.contains("s = ("), "got:\n{out}");
    assert!(out.contains("_ainl_try("), "got:\n{out}");
    assert!(out.contains(").call"), "the IIFE must be invoked:\n{out}");
}

#[test]
fn catch_binds_the_documented_hash_shape() {
    // `{"message" <str>, "kind" "runtime"}` — message FIRST, because every
    // backend prints a map in insertion order and the key order is what makes
    // the caught value byte-identical across all five.
    let out = rb("(try (error \"x\") (catch (e) e))");
    assert!(out.contains("def _caught("), "got:\n{out}");
    assert!(out.contains("'message'"), "got:\n{out}");
    assert!(out.contains("'kind', 'runtime'"), "got:\n{out}");
    let msg = out.find("'message'").unwrap();
    let kind = out.find("'kind'").unwrap();
    assert!(msg < kind, "message must come before kind:\n{out}");
}

#[test]
fn ainl_errors_raise_the_catchable_type() {
    // A bare `raise("msg")` produces a `RuntimeError`, which `rescue
    // AinlError` would NOT catch — so the AINL error would fly past every
    // `catch` in the program. The class must be raised explicitly.
    let out = rb("(try (error \"x\") (catch (e) 1))");
    assert!(
        out.contains("class AinlError < StandardError"),
        "got:\n{out}"
    );
    assert!(out.contains("raise(AinlError,"), "got:\n{out}");
    assert!(out.contains("rescue AinlError => e"), "got:\n{out}");
}

#[test]
fn the_error_class_is_declared_before_the_raise_that_names_it() {
    // The RUNTIME table is emitted in declaration order, so a class declared
    // after its first use is a load-order bug that only shows up at run time.
    let out = rb("(try (error \"x\") (catch (e) 1))");
    let class = out.find("class AinlError").unwrap();
    let raise_ = out.find("raise(AinlError,").unwrap();
    let rescue_ = out.find("rescue AinlError").unwrap();
    assert!(class < raise_, "class must precede _error:\n{out}");
    assert!(class < rescue_, "class must precede _ainl_try:\n{out}");
}

#[test]
fn non_ainl_exceptions_are_not_swallowed() {
    // A bare `rescue` would also catch a bug in the generated Ruby and present
    // it to the AINL program as if it had handled its own error.
    let out = rb("(try (error \"x\") (catch (e) 1))");
    assert!(
        out.contains("rescue AinlError => e"),
        "expected a typed rescue clause:\n{out}"
    );
    assert!(
        !out.contains("rescue => e") && !out.contains("rescue StandardError =>"),
        "an untyped rescue would swallow a bug in the generated code:\n{out}"
    );
}

#[test]
fn arithmetic_routes_through_the_checked_helpers() {
    // Ruby's own operators are not AINL's: `1 + "s"` is `"1s"` and
    // `1.0 / 0.0` is `Infinity`, so a `catch` would never see the error the
    // interpreter reports.
    assert!(rb("(+ 1 2)").contains("_add(1, 2)"));
    assert!(rb("(- 1 2)").contains("_sub(1, 2)"));
    assert!(rb("(* 1 2)").contains("_mul(1, 2)"));
    assert!(rb("(/ 1 2)").contains("_div(1, 2)"));
    assert!(rb("(mod 1 2)").contains("_mod(1, 2)"));
}

#[test]
fn div_rejects_zero_before_dividing() {
    // `1.0 / 0.0` does not raise on the host — it is `Infinity`. The zero test
    // has to come first or the check is dead code.
    let out = rb("(/ 1 2)");
    let zero = out.find("_error('division by zero')").unwrap();
    let div = out.find("to_f /").unwrap();
    assert!(
        zero < div,
        "the zero test must precede the division:\n{out}"
    );
}

#[test]
fn list_and_hash_builtins_guard_through_the_shared_predicates() {
    // Without the guards a wrong-typed operand raises a HOST NoMethodError,
    // which is not an AinlError and so escapes the rescue clause entirely.
    let out = rb("(first xs)");
    assert!(out.contains("def _alist("), "got:\n{out}");
    assert!(out.contains("_alist('first', x)"), "got:\n{out}");
    let out = rb(r#"(get h "k")"#);
    assert!(out.contains("def _ahash("), "got:\n{out}");
    assert!(out.contains("_ahash('get', h)"), "got:\n{out}");
}

#[test]
fn file_builtins_raise_the_ainl_message_not_the_host_exception() {
    // Ruby's would otherwise bind `Errno::ENOENT`, Python's a
    // `FileNotFoundError` and JS's a raw `ENOENT` Error — three different
    // types and message bodies for the same missing file.
    let out = rb(r#"(read-file "x.txt")"#);
    assert!(out.contains("read-file: cannot read"), "got:\n{out}");
    assert!(out.contains("rescue SystemCallError"), "got:\n{out}");
    assert!(out.contains("rescue Errno::EISDIR"), "got:\n{out}");
}

#[test]
fn the_try_helpers_pull_in_what_they_name() {
    // A generated file that references a class defined after it is a
    // load-order bug, and it only shows up when someone runs the program.
    // `_print` is deliberately NOT in this list: the program has no `print`,
    // so omitting it is the correct behaviour, and naming it would pin the
    // very thing the `need` graph exists to avoid.
    let out = rb("(try (error \"x\") (catch (e) 1))");
    for helper in ["AinlError", "AHash", "_caught", "_ainl_try", "_error"] {
        assert!(out.contains(helper), "{helper} missing:\n{out}");
    }
    assert!(!out.contains("def _print("), "unused _print:\n{out}");
}

#[test]
fn a_program_without_try_carries_no_try_runtime() {
    let out = rb(r#"(print "hi")"#);
    for helper in ["_ainl_try", "AinlError", "_caught"] {
        assert!(!out.contains(helper), "unused {helper} was emitted:\n{out}");
    }
}

#[test]
fn a_multi_form_side_is_refused_in_expression_position() {
    // Same limit `let` has: a side that is more than one form is a sequence of
    // statements and cannot live in an expression. Saying so beats emitting
    // something that silently drops a form.
    let err = transpile_ruby_src("(def s (try (print 1) (print 2) (catch (e) 1)))")
        .expect_err("multi-statement body should be refused in expression position");
    let msg = err.to_string();
    assert!(msg.contains("multi-statement try body"), "got: {msg}");
}
