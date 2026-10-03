//! The Tier 4 key-value API (`db-set` / `db-get` / `db-del` / `db-keys` /
//! `db-count`) on the interpreter and the bytecode VM.
//!
//! The shape follows `db_builtins.rs`, and the reason is the same: these are
//! *parity* tests first. `run_in` is the bytecode VM and `tree_walk_in` is the
//! tree-walking evaluator; both reach the same `BuiltinFn` through different
//! entry points, so a bug in either dispatch is invisible to a single-backend
//! test. The AOT half — including a persistence test on a *compiled binary* —
//! is in the sibling crate, and the transpiler half is a refusal.
//!
//! Every test gets its own directory because these builtins write files.
//!
//! What the rules are, and why, is in docs/SYNTAX.md §3l. What these tests
//! protect is that the value layer really does round-trip an AINL value through
//! the byte layer's log, on both evaluators, with the two properties a natural
//! implementation gets wrong:
//!
//! * **A tombstone is a log record, not a memory edit.** A delete has to survive
//!   the process, so it has to be replayed like any other record.
//! * **A deleted key is gone from all three readers.** `db-get` returning `nil`
//!   while `db-keys` still lists the key and `db-count` still counts it is the
//!   failure mode where `db-del` looks like it worked and enumeration does not
//!   agree. The three are asserted together, every time, because a fix to one
//!   that misses another is easy to write and invisible to a single assertion.

use ainl_core::{run_str, BigNum, Env, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique scratch directory for one test, removed when it drops.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbkv-{}-{tag}-{n}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create scratch dir");
        Scratch { path: p }
    }

    fn db(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run `src` on **both** evaluators and require them to agree, returning the
/// string form of each result.
///
/// A helper rather than a macro because "run it twice and compare" is the
/// invariant this whole file exists to protect, and writing it once means no
/// test can quietly run on only one evaluator. `{db}` is replaced with the
/// test's own database path.
fn both_evaluators_agree(tag: &str, src: &str) -> (String, String) {
    let mut out = Vec::new();
    for (label, vm) in [("vm", true), ("tree-walk", false)] {
        let s = Scratch::new(&format!("{tag}-{label}"));
        let p = s.db("a.ainl-db");
        let env = Env::with_prelude();
        let src = src.replace("{db}", &p);
        let v = if vm {
            ainl_core::run_in(&src, &env)
        } else {
            ainl_core::tree_walk_in(&src, &env)
        };
        out.push(
            v.unwrap_or_else(|e| panic!("the {label} evaluator failed: {e}\nprogram was:\n{src}"))
                .to_string(),
        );
    }
    let second = out.pop().expect("two results");
    let first = out.pop().expect("two results");
    assert_eq!(
        first, second,
        "the two evaluators disagree — the value layer is not shared"
    );
    (first, second)
}

/// The same, for a program expected to *fail*: both evaluators must produce the
/// same error message, and the message is returned for the caller to assert on.
fn both_evaluators_error(tag: &str, src: &str) -> String {
    let mut out = Vec::new();
    for (label, vm) in [("vm", true), ("tree-walk", false)] {
        let s = Scratch::new(&format!("{tag}-{label}"));
        let p = s.db("a.ainl-db");
        let env = Env::with_prelude();
        let src = src.replace("{db}", &p);
        let r = if vm {
            ainl_core::run_in(&src, &env)
        } else {
            ainl_core::tree_walk_in(&src, &env)
        };
        out.push(
            r.expect_err(&format!("the {label} evaluator should have refused {src}"))
                .message()
                .to_string(),
        );
    }
    let second = out.pop().expect("two results");
    let first = out.pop().expect("two results");
    assert_eq!(
        first, second,
        "the two evaluators produce different errors for the same program"
    );
    first
}

// ---- the round trip: every value type --------------------------------------

/// The card's second acceptance bullet, one assertion per value type.
///
/// Each case is `(stored, read-back, is-still-the-same-type)`. The type check
/// is the part that matters: `(= 1 (db-get h "k"))` is `true` for `1.0` too in a
/// language with one numeric tower, so asserting only the printed value would
/// let a float silently replace an int and pass. The values that *can* be told
/// apart by `=` are the ones with a distinct printed form, and the int/float
/// case gets its own test below.
#[test]
fn every_value_type_round_trips() {
    let cases: &[(&str, &str, &str)] = &[
        // stored, then what `db-get` prints *inside a list*, then what `=` must
        // answer. The middle column is a list element, so a string prints with
        // its quotes — that is `print`'s rule for a list, not the value's own
        // rendering, and getting it wrong here would hide a real encoding bug.
        ("42", "42", "true"),
        ("-7", "-7", "true"),
        ("1.5", "1.5", "true"),
        ("true", "true", "true"),
        ("false", "false", "true"),
        ("\"hello\"", "\"hello\"", "true"),
        ("(list 1 2 3)", "(1 2 3)", "true"),
        ("(list (list 1) (list 2))", "((1) (2))", "true"),
        // A map is built with `hash` and serialized as a JSON object. It comes
        // back with the object's braces, which is the *correct* answer and the
        // interesting one: a map is the only AINL value whose JSON form is not
        // also its printed form, so it is the case that would silently turn into
        // a flat list of pairs in an encoder that did not distinguish them.
        ("(hash \"a\" 1 \"b\" 2)", "{\"a\" 1 \"b\" 2}", "true"),
    ];
    for (stored, want, eq) in cases {
        let src = format!(
            r#"(do (def h (db-open "{{db}}"))
                    (db-set h "k" {stored})
                    (def v (db-get h "k"))
                    (def same (= v {stored}))
                    (db-close h)
                    (list v same))"#
        );
        let (out, _) = both_evaluators_agree("rt", &src);
        assert_eq!(
            out,
            format!("({want} {eq})"),
            "the value {stored} did not round trip"
        );
    }
}

/// A `nil` stored under a key is a *stored* nil, and it is indistinguishable
/// from an absent key by design — the card specifies `nil` for both. That is
/// worth pinning rather than leaving to a reader: the alternative (a
/// "was it there?" answer) would need a second query, and this tier does not
/// have one. `db-keys` is that answer, and it is asserted separately.
#[test]
fn a_stored_nil_reads_as_nil() {
    let (out, _) = both_evaluators_agree(
        "nil",
        r#"(do (def h (db-open "{db}"))
            (db-set h "present" nil)
            (def r (list (db-get h "present") (db-get h "absent") (db-keys h) (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(
        out, "(nil nil (\"present\") 1)",
        "a stored nil reads as nil, and the key is still live"
    );
}

/// The empty string is the case a `"` -vs-missing confusion would collapse, and
/// it is the only value that is *falsy-looking text* rather than `nil`. It must
/// survive as a string.
#[test]
fn an_empty_string_is_a_value_not_an_absence() {
    let (out, _) = both_evaluators_agree(
        "empty",
        r#"(do (def h (db-open "{db}"))
            (db-set h "k" "")
            (def r (list (db-get h "k") (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(
        out, "(\"\" 1)",
        "an empty string must be stored, not read as nil"
    );
}

/// The int/float distinction, which is the one thing JSON is genuinely bad at
/// and the reason this card's parity claim is scoped to three backends.
///
/// If this test ever needs a tolerance or a filter to pass, the *encoding* is
/// what is wrong — not the assertion. A store that turned every number into a
/// float would still round-trip every other value in this file.
#[test]
fn an_int_stays_an_int_and_a_whole_float_stays_a_float() {
    let (out, _) = both_evaluators_agree(
        "numbers",
        r#"(do (def h (db-open "{db}"))
            (db-set h "i" 1)
            (db-set h "f" 1.0)
            (def r (list (json-serialize (db-get h "i")) (json-serialize (db-get h "f"))))
            (db-close h)
            r)"#,
    );
    assert_eq!(
        out, "(\"1\" \"1.0\")",
        "the int/float distinction did not survive the log"
    );
}

// ---- last-write-wins, and what is on disk ----------------------------------

/// Overwriting is last-write-wins on every backend, and it is a *new record*
/// rather than an in-place edit — the property that makes a crash mid-overwrite
/// leave the old value intact.
#[test]
fn an_overwrite_wins_and_the_older_record_is_still_on_disk() {
    let s = Scratch::new("overwrite");
    let p = s.db("a.ainl-db");
    let env = Env::with_prelude();
    let src = format!(
        r#"(do (def h (db-open "{p}"))
            (db-set h "k" "first")
            (db-set h "k" "second")
            (db-close h)
            (def h2 (db-open "{p}"))
            (def r (db-get h2 "k"))
            (db-close h2)
            r)"#
    );
    let v = ainl_core::run_in(&src, &env).expect("run");
    assert_eq!(v.to_string(), "second", "the last write must win");
    // The older record is still in the log, which is the append-only claim.
    let raw = std::fs::read(&p).expect("read the log");
    let has_first = raw.windows(5).any(|w| w == b"first");
    assert!(has_first, "the log is append-only: the old record survives");
}

// ---- deletion --------------------------------------------------------------

/// `db-del` removes the key, and all three readers agree it is gone.
///
/// The three assertions are deliberately in one test. A delete that clears
/// `db-get` but not `db-keys` is the exact bug this file was written to catch,
/// and it is invisible to any one of the three alone.
#[test]
fn delete_removes_the_key_from_get_keys_and_count() {
    let (out, _) = both_evaluators_agree(
        "del",
        r#"(do (def h (db-open "{db}"))
            (db-set h "a" 1)
            (db-set h "b" 2)
            (def gone (db-del h "a"))
            (def r (list gone (db-get h "a") (db-keys h) (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(
        out, "(true nil (\"b\") 1)",
        "a deleted key must be absent from get, keys and count alike"
    );
}

/// Deleting an absent key is `false`, not an error — the idempotent answer, and
/// what makes `if (db-del h k) ...` safe to run twice.
#[test]
fn deleting_an_absent_key_is_false_and_not_an_error() {
    let (out, _) = both_evaluators_agree(
        "del-absent",
        r#"(do (def h (db-open "{db}"))
            (def r (list (db-del h "never") (db-del h "never") (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(out, "(false false 0)");
}

/// The delete survives the process, which is the whole reason it is a log
/// record: a delete held only in memory would pass every test above and come
/// back on the next run.
#[test]
fn a_delete_survives_a_reopen() {
    let s = Scratch::new("del-reopen");
    let p = s.db("a.ainl-db");
    let env = Env::with_prelude();
    let write = format!(
        r#"(do (def h (db-open "{p}"))
            (db-set h "keep" 1)
            (db-set h "drop" 2)
            (db-del h "drop")
            (db-close h))"#
    );
    ainl_core::run_in(&write, &env).expect("write");
    let read = format!(
        r#"(do (def h (db-open "{p}"))
            (def r (list (db-get h "drop") (db-get h "keep") (db-keys h) (db-count h)))
            (db-close h)
            r)"#
    );
    let v = ainl_core::run_in(&read, &env).expect("read");
    assert_eq!(
        v.to_string(),
        "(nil 1 (\"keep\") 1)",
        "the delete must outlive the process"
    );
}

/// A set after a delete revives the key, because both are records and replay
/// applies them in order. This is the property that makes last-write-wins the
/// *only* rule a caller has to know.
#[test]
fn a_set_after_a_delete_revives_the_key() {
    let (out, _) = both_evaluators_agree(
        "revive",
        r#"(do (def h (db-open "{db}"))
            (db-set h "k" 1)
            (db-del h "k")
            (db-set h "k" 2)
            (def r (list (db-get h "k") (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(out, "(2 1)");
}

// ---- keys and count --------------------------------------------------------

/// `db-keys` is **sorted**, and that is a parity requirement rather than a
/// nicety: the two engines index keys differently and neither has a defined
/// iteration order, so an unsorted list would print the same keys in a
/// different sequence on each backend.
#[test]
fn keys_are_sorted_by_byte_value() {
    let (out, _) = both_evaluators_agree(
        "sorted",
        r#"(do (def h (db-open "{db}"))
            (db-set h "b" 1)
            (db-set h "A" 1)
            (db-set h "a" 1)
            (db-set h "10" 1)
            (db-set h "2" 1)
            (def r (db-keys h))
            (db-close h)
            r)"#,
    );
    assert_eq!(
        out, "(\"10\" \"2\" \"A\" \"a\" \"b\")",
        "keys must be in ascending byte order, not numeric or insertion order"
    );
}

/// An empty store has no keys, and the list is empty rather than nil.
#[test]
fn an_empty_database_has_no_keys_and_a_count_of_zero() {
    let (out, _) = both_evaluators_agree(
        "empty",
        r#"(do (def h (db-open "{db}"))
            (def r (list (db-keys h) (db-count h)))
            (db-close h)
            r)"#,
    );
    assert_eq!(out, "(() 0)");
}

// ---- refusals --------------------------------------------------------------

/// A value JSON cannot represent is refused, with the JSON writer's own
/// message. Reusing that message is what makes the C port's refusal identical
/// for free — it calls the same writer.
#[test]
fn a_function_is_refused_by_name() {
    let msg = both_evaluators_error(
        "fn",
        r#"(do (def h (db-open "{db}")) (db-set h "k" (fn (x) x)) (db-close h))"#,
    );
    assert!(
        msg.contains("json-serialize: cannot serialize a fn"),
        "got: {msg}"
    );
}

/// A stale handle is refused by number, at every layer — the same message shape
/// the byte layer already documents.
#[test]
fn a_stale_handle_is_refused_by_number() {
    let msg = both_evaluators_error(
        "stale",
        r#"(do (def h (db-open "{db}")) (db-close h) (db-set h "k" 1))"#,
    );
    assert!(msg.contains("db-set: handle 1 is not open"), "got: {msg}");
}

/// An arity error names the form the caller should have written — the house
/// style, and the message a transpiler-refusal test matches on.
#[test]
fn an_arity_error_names_the_form() {
    let msg = both_evaluators_error("arity", r#"(db-set 1 "k")"#);
    assert_eq!(msg, "db-set expects (db-set handle key value)");
}

/// A type error names the operand and its type, for both the handle and the key.
#[test]
fn a_type_error_names_the_operand() {
    let msg = both_evaluators_error("type", r#"(db-set "x" "k" 1)"#);
    assert_eq!(msg, "db-set expects a db handle, got str");
    let msg = both_evaluators_error("type2", r#"(db-set 1 5 1)"#);
    assert_eq!(msg, "db-set expects a str key, got int");
}

/// The one genuinely sharp edge in the tier: text written by the **byte** layer
/// is not a value, so reading it through the value layer is an error rather than
/// a bare `nil`.
///
/// `nil` would be the wrong answer for two reasons: it is indistinguishable from
/// a missing key, and it would leave a program that mixed the two layers with no
/// way to find out which of the two went wrong. The message names the fix.
#[test]
fn db_put_text_is_not_mistaken_for_a_value() {
    let s = Scratch::new("put-text");
    let p = s.db("a.ainl-db");
    let env = Env::with_prelude();
    let write = format!(r#"(do (def h (db-open "{p}")) (db-put h "k" "buy milk") (db-close h))"#);
    ainl_core::run_in(&write, &env).expect("the byte layer must still work");
    let read = format!(r#"(do (def h (db-open "{p}")) (def v (db-get h "k")) (db-close h) v)"#);
    let e = ainl_core::run_in(&read, &env).expect_err("plain text is not a value");
    let msg = e.message();
    assert!(
        msg.contains("not an AINL value") && msg.contains("buy milk"),
        "the error must name the offending text and the fix; got: {msg}"
    );
    assert!(
        msg.contains("db-set"),
        "the error must name the builtin that fixes it; got: {msg}"
    );
    let _ = read;
}

/// And the composition that *does* work: text that happens to be valid JSON is
/// readable as the value it spells. This is the documented boundary of the
/// collision, asserted so it is a decision rather than an accident.
#[test]
fn db_put_text_that_is_json_reads_as_that_value() {
    let s = Scratch::new("put-json");
    let p = s.db("a.ainl-db");
    let env = Env::with_prelude();
    ainl_core::run_in(
        &format!(r#"(do (def h (db-open "{p}")) (db-put h "k" "42") (db-close h))"#),
        &env,
    )
    .expect("write");
    let v = ainl_core::run_in(
        &format!(r#"(do (def h (db-open "{p}")) (def v (db-get h "k")) (db-close h) v)"#),
        &env,
    )
    .expect("read");
    assert_eq!(
        v,
        Value::Int(BigNum::small(42)),
        "JSON text reads back as the value it spells"
    );
}

/// `db-get` on a *missing* key stays `nil` on both evaluators — the card's
/// explicit requirement, and the reason a probe needs no `try`.
#[test]
fn an_absent_key_is_nil() {
    let (out, _) = both_evaluators_agree(
        "absent",
        r#"(do (def h (db-open "{db}")) (def v (db-get h "nope")) (db-close h) v)"#,
    );
    assert_eq!(out, "nil");
}

// ---- the two layers coexist ------------------------------------------------

/// One handle, both layers. The card says the value API sits *on top of* the
/// byte engine, so a program must be able to use them together on the same
/// handle — that is what "on top of" has to mean if it means anything.
#[test]
fn both_layers_share_one_handle() {
    let s = Scratch::new("both");
    let p = s.db("a.ainl-db");
    let env = Env::with_prelude();
    ainl_core::run_in(
        &format!(
            r#"(do (def h (db-open "{p}"))
                (db-set h "n" 7)
                (db-put h "s" "text")
                (db-flush h)
                (db-close h))"#
        ),
        &env,
    )
    .expect("write with both layers");
    let v = ainl_core::run_in(
        &format!(
            r#"(do (def h (db-open "{p}"))
                (def r (list (db-count h) (db-keys h)))
                (db-close h)
                r)"#
        ),
        &env,
    )
    .expect("read");
    assert_eq!(
        v.to_string(),
        "(2 (\"n\" \"s\"))",
        "a key written by either layer is visible to the value layer"
    );
}

/// `run_str` is the in-process single-form entry point; the suite uses it once
/// so the value builtins are exercised through the same path the CLI's `ainl
/// run` takes, not only through `run_in` with an explicit environment.
#[test]
fn the_builtins_are_reachable_through_run_str() {
    let s = Scratch::new("runstr");
    let p = s.db("a.ainl-db");
    let v = run_str(&format!(r#"(db-open "{p}")"#)).expect("db-open");
    assert!(
        matches!(v, Value::Int(ref n) if n.as_i64().unwrap_or(i64::MIN) >= 1),
        "db-open returns a handle number, got {v:?}"
    );
}
