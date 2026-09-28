//! The `test` builtin and the `ainl test` runner behind it.
//!
//! AINL had no way to assert its own programs were correct. A model writing an
//! AINL program could not check its own work: every program that printed
//! something looked the same whether it was right or wrong. This module is the
//! minimum that closes that gap — a test *is* "expr == expected" — and
//! nothing more. No fixtures, no mocking, no async, no test objects.
//!
//! # Why the failure is an error, not a printed line
//!
//! The obvious design is `(test ...)` printing `ok`/`FAIL` and returning a
//! bool. That was rejected, and the reason is the project's hardest invariant:
//! the interpreter, the VM, the AOT C runtime and the three transpilers must
//! agree byte-for-byte on **stdout and stderr** (docs/SYNTAX.md §5a).
//!
//! A printed failure makes the *message* a stdout concern, and stdout parity
//! on a failure path would then depend on each backend's shim flushing,
//! buffering and repr'ing a value at exactly the same instant. Raising an
//! error instead reuses the mechanism `error` and `read-file` already use and
//! that all four backends already agree on: one shared message body, with the
//! position suffix (`at line N, col M`) present only where the source is in
//! hand. The AOT binary compiles spans away entirely, so it has no position to
//! print — a documented, test-enforced difference (see
//! `crates/ainl-cc/tests/aot_stdlib.rs`), not a new exception.
//!
//! That choice also gives `ainl test` something a boolean could not: the first
//! failing assertion in a file aborts it, and the runner can attribute the
//! failure to a file and keep going. A program that raised a runtime error
//! under `ainl run` already behaves this way, so a test file is an ordinary
//! AINL program and needs no special status.

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::value::Value;

/// The symbol the `test` builtin is bound to.
pub const TEST_SYM: &str = "test";

/// Bind `test` into `env`.
pub fn install(env: &Env) {
    env.define(
        TEST_SYM,
        Value::Builtin {
            name: TEST_SYM,
            f: builtin_test,
        },
    );
}

/// `(test name expr expected)` → `true` when `expr` equals `expected`.
///
/// Passes as `true` (so a passing test is a usable value, and a suite can
/// aggregate with `and`). Fails by raising: the message names the test, the
/// expected value and the actual value, which is the whole point — a failure
/// a model cannot act on is a failure that costs a round trip.
///
/// Both `name` and `expected` must be strings. That is a real restriction and
/// it is worth it: the message has to be a *string* to name the test, and
/// accepting a non-string name would mean either coercing it (silently, and
/// differently per backend) or printing it unquoted. Rejecting is the one
/// option that cannot diverge.
///
/// `expected` is written as a string because that is how a value is *spelled*
/// in a failure report, and it removes the last source of cross-backend
/// disagreement: a list, a map or a float compared structurally could in
/// principle print differently on one backend. Comparing the rendered form
/// means the assertion and the report cannot disagree with each other.
fn builtin_test(args: &[Value]) -> Result<Value> {
    let [name, actual, expected] = args else {
        return Err(Error::runtime("test expects (test name expr expected)"));
    };
    let Value::Str(name) = name else {
        return Err(Error::runtime(format!(
            "test expects a str name, got {}",
            name.type_name()
        )));
    };
    let Value::Str(expected) = expected else {
        return Err(Error::runtime(format!(
            "test expects a str expected value, got {}",
            expected.type_name()
        )));
    };
    let got = actual.to_string();
    if got == expected.as_str() {
        return Ok(Value::Bool(true));
    }
    Err(Error::runtime(failure_message(name, expected, &got)))
}

/// The one message a failing test produces, shared by every backend.
///
/// Built here rather than in each of the five code generators because the
/// 4-backend rule makes this string a parity contract: five hand-written copies
/// are five chances to disagree, and the AOT `aot_stdlib.rs` parity suite can
/// only check the ones someone remembered to write a case for.
pub fn failure_message(name: &str, expected: &str, actual: &str) -> String {
    format!("test failed: {name}: expected {expected}, got {actual}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::builtin_names;

    fn run(src: &str) -> Result<Value> {
        crate::run_str(src)
    }

    #[test]
    fn test_is_bound_in_the_prelude() {
        assert!(builtin_names().iter().any(|n| n == TEST_SYM));
    }

    #[test]
    fn a_matching_value_passes_and_yields_true() {
        // `expected` is always a *string* — the rendered form of the value. That
        // is what makes the assertion and the failure report the same text.
        assert_eq!(run(r#"(test "t" 1 "1")"#).unwrap(), Value::Bool(true));
        assert_eq!(run(r#"(test "t" "a" "a")"#).unwrap(), Value::Bool(true));
        assert_eq!(run(r#"(test "t" nil "nil")"#).unwrap(), Value::Bool(true));
        // A list's rendered form is what is compared, so the string names it.
        assert_eq!(
            run(r#"(test "t" (list 1 2) "(1 2)")"#).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn a_mismatch_names_the_test_expected_and_actual() {
        let err = run(r#"(test "adds" (+ 1 2) "4")"#).expect_err("should fail");
        assert_eq!(
            err.message(),
            "test failed: adds: expected 4, got 3",
            "the message must carry the name and both values"
        );
    }

    #[test]
    fn values_are_compared_as_rendered_not_as_typed() {
        // 1 and 1.0 are `=` in AINL, and both render "1.0"/"1" respectively —
        // so a whole float and the int 1 are *not* the same assertion. This is
        // asserted because it is the one place where "expr == expected" has a
        // sharper edge than `=`, and a test suite must not be surprised by it.
        assert!(run(r#"(test "t" 1 "1")"#).is_ok());
        assert!(run(r#"(test "t" 1.0 "1.0")"#).is_ok());
        assert!(run(r#"(test "t" 1.0 "1")"#).is_err());
    }

    #[test]
    fn arity_and_operand_errors_are_reported_precisely() {
        for (src, want) in [
            ("(test)", "test expects (test name expr expected)"),
            ("(test \"a\" 1)", "test expects (test name expr expected)"),
            (
                "(test \"a\" 1 \"1\" 4)",
                "test expects (test name expr expected)",
            ),
            ("(test 7 1 \"1\")", "test expects a str name, got int"),
            (
                "(test \"a\" 1 1)",
                "test expects a str expected value, got int",
            ),
            (
                "(test \"a\" 1 (list 1))",
                "test expects a str expected value, got list",
            ),
        ] {
            let err = run(src).expect_err(src);
            assert_eq!(err.message(), want, "for {src}");
        }
    }
}
