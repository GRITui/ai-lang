//! The Tier 4 storage engine, on the interpreter and the bytecode VM.
//!
//! The 4-backend rule makes this a *parity* test first and a *behaviour* test
//! second. `run_str` is the bytecode VM and `run_in_tree_walk` is the
//! tree-walking evaluator; both reach the same `BuiltinFn` through different
//! entry points, so a bug in either is invisible to a single-backend test. The
//! AOT half — including the headline crash test on a *compiled binary* — is in
//! the sibling crate (`aot_stdlib.rs`, `db_crash.rs`), and the transpiler half
//! is a refusal (`db_refusal.rs`).
//!
//! Every test gets its own directory, because these builtins *write files*.
//! A fixed name would make the tests interfere with each other and with the
//! developer's working tree; `Scratch` makes a unique path under the system
//! temp dir and removes it afterwards.
//!
//! What the rules are, and why, is in docs/SYNTAX.md §3j. What these tests
//! protect is that the interpreter actually implements them — in particular the
//! two that a natural implementation gets wrong:
//!
//! * **The last write wins, and the log is append-only.** Overwriting a key
//!   appends a second record rather than rewriting the first, so a program that
//!   crashes mid-overwrite keeps the old value rather than a half-written one.
//! * **A missing key is `nil`, not an error**, so `db-get` is a total function
//!   and a caller can probe without a `try`.

use ainl_core::{run_str, Env, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique scratch directory for one test, removed when it drops.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // pid + a counter + the thread id: `cargo test` runs these in parallel
        // and two tests sharing a path would see each other's writes.
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbt-{}-{tag}-{n}-{:?}",
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

    fn raw(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path.join(name)).expect("read the database")
    }

    fn put_raw(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.path.join(name), bytes).expect("write the database");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run `write` then `read` on **both** evaluators, each against its own
/// database, and return `(the value `read` produced, the two log files)`.
///
/// Per-evaluator files, not one shared file: the handle registry is a
/// thread-local, so running the same program twice on one thread opens the same
/// database twice and the second run *appends to the first's log*. A shared
/// fixture would measure that instead of the evaluator.
///
/// Returning the files rather than printing is deliberate. Two evaluators
/// whose logs differ byte-for-byte would give identical answers for a program
/// that only ever writes once, and would disagree on the *second* open — so the
/// log is the claim worth asserting, and a test that only checked `db-get` would
/// miss exactly the drift that matters.
///
/// `read` must **end with the value you want**, not with `(db-close h)`: closing
/// returns nil, so a read that closes last would always report nil. The
/// programs below bind the answer to a name and end with that name.
///
/// `{db}` in either program is replaced with that evaluator's own path.
fn both_evaluators_agree(write: &str, read: &str) -> (String, Vec<u8>, Vec<u8>) {
    let mut value = String::new();
    let mut files = Vec::new();
    for (label, vm) in [("vm", true), ("tree-walk", false)] {
        let s = Scratch::new(&format!("agree-{label}"));
        let p = s.db("a.ainl-db");
        let env = Env::with_prelude();
        let run = |src: &str| -> Result<Value, ainl_core::Error> {
            let src = src.replace("{db}", &p);
            if vm {
                ainl_core::run_in(&src, &env)
            } else {
                ainl_core::tree_walk_in(&src, &env)
            }
        };
        run(write)
            .unwrap_or_else(|e| panic!("the {label} evaluator failed: {e}\nprogram was:\n{write}"));
        files.push(s.raw("a.ainl-db"));
        let v =
            run(read).unwrap_or_else(|e| panic!("the {label} evaluator failed on the read: {e}"));
        value = v.to_string();
    }
    let second = files.pop().expect("two files");
    let first = files.pop().expect("two files");
    (value, first, second)
}

// ---- the basic contract ----------------------------------------------------

#[test]
fn write_read_close_reopen_keeps_the_data() {
    // The card's first acceptance bullet, in the shape a caller writes it.
    // The read runs in a *separate* evaluation of the same process, so the
    // handle is closed and the file reopened from disk — not a cached index.
    let (value, vm_file, tree_file) = both_evaluators_agree(
        r#"(do (def h (db-open "{db}"))
             (db-put h "alpha" "one")
             (db-put h "beta" "two")
             (db-close h))"#,
        r#"(do (def h (db-open "{db}"))
             (def r (list (db-get-raw h "alpha") (db-get-raw h "beta") (db-get-raw h "absent")))
             (db-close h)
             r)"#,
    );
    assert_eq!(value, r#"("one" "two" nil)"#);
    assert_eq!(
        vm_file, tree_file,
        "the two evaluators wrote different logs"
    );
}

#[test]
fn a_put_returns_nil_and_a_get_returns_the_value() {
    // The return values, read back as a list rather than printed, so the
    // assertion is on the language's values and not on this process's stdout.
    let (value, _, _) = both_evaluators_agree(
        r#"(do (def h (db-open "{db}"))
             (db-put h "k" "v")
             (db-flush h)
             (db-close h))"#,
        r#"(do (def h (db-open "{db}"))
             (def r (list (db-get-raw h "k") (db-get-raw h "absent")))
             (db-close h)
             r)"#,
    );
    assert_eq!(value, r#"("v" nil)"#, "an absent key is nil, not an error");
    // The `nil` returns of put/flush/close are pinned directly: they are what a
    // caller chains on, and a port that returned the value or true would break
    // every program that treats them as statements.
    let s = Scratch::new("nil-returns");
    let v = run_str(&format!(
        r#"(do (def h (db-open "{p}"))
             (list (db-put h "k" "v") (db-flush h) (db-close h)))"#,
        p = s.db("n.ainl-db")
    ))
    .expect("runs");
    assert_eq!(v.to_string(), "(nil nil nil)");
}

#[test]
fn db_open_returns_a_handle_and_reuses_the_number_after_a_close() {
    // The numbers themselves are asserted through the unit tests in db.rs; what
    // matters here is that the program completes, so a handle table that grew
    // without bound or handed back a stale entry would have failed.
    let s = Scratch::new("handle");
    let p = s.db("h.ainl-db");
    let q = s.db("h2.ainl-db");
    let v = run_str(&format!(
        r#"(do (def a (db-open "{p}"))
             (def b (db-open "{q}"))
             (db-close a)
             (def c (db-open "{q}"))
             (def r (list (= a 1) (= b 2) (= c a)))
             (db-close b) (db-close c)
             r)"#
    ))
    .expect("runs");
    assert_eq!(
        v.to_string(),
        "(true true true)",
        "the released number comes back"
    );
}

#[test]
fn the_last_write_wins_and_the_log_still_holds_both_records() {
    // The append-only property, made observable. A reader sees the newest
    // value; the file still contains the older record, because a log is never
    // rewritten in place.
    let (value, vm_file, tree_file) = both_evaluators_agree(
        r#"(do (def h (db-open "{db}"))
             (db-put h "k" "first")
             (db-put h "k" "second")
             (db-close h))"#,
        r#"(db-get-raw (db-open "{db}") "k")"#,
    );
    assert_eq!(value, "second");
    assert_eq!(
        vm_file, tree_file,
        "the two evaluators wrote different logs"
    );

    // Both records are on disk: the file holds the bytes "first" AND "second".
    let text = String::from_utf8_lossy(&vm_file).into_owned();
    assert!(
        text.contains("first"),
        "the old record was rewritten in place instead of appended"
    );
    assert!(text.contains("second"), "the new record is missing");
    // Two records, each 12 header bytes + 1 key + its value, after a 16-byte
    // header. An exact size, so a record carrying a redundant trailing copy
    // would fail here.
    let per_record = 12 + 1 + "first".len() + 12 + 1 + "second".len();
    assert_eq!(
        vm_file.len(),
        16 + per_record,
        "expected header + exactly two records"
    );
}

#[test]
fn a_key_with_awkward_bytes_round_trips() {
    // Values are text, but text can hold a newline, a space or a multi-byte
    // character. The framing is length-prefixed, so none of these is special —
    // and the C port holds them in a `char *`, which is why the engine's own
    // rule drops any record containing a NUL (no AINL string can have one).
    //
    // No Scratch here: `both_evaluators_agree` makes and cleans up its own, one
    // per evaluator.
    let pairs = [
        ("a b", "space in key"),
        ("a\nb", "newline in key"),
        ("日本", "multi-byte key"),
        ("k", "héllo 日本"),
        ("", "empty key"),
        ("v", ""),
    ];
    let mut puts = String::new();
    for (k, v) in &pairs {
        puts.push_str(&format!("(db-put h {:?} {:?})", k, v));
    }
    let mut gets = String::new();
    for (k, v) in &pairs {
        gets.push_str(&format!("(test {:?} (db-get-raw h {:?}) {:?})", k, k, v));
    }
    // `(test name expr expected)` fails the run on a mismatch, so a wrong read
    // back is a failure with the key and the two values named — better than an
    // equality assertion on a rendered string.
    both_evaluators_agree(
        &format!(r#"(do (def h (db-open "{{db}}")) {puts} (db-close h))"#),
        &format!(r#"(do (def h (db-open "{{db}}")) {gets} (db-close h))"#),
    );
}

// ---- crash recovery (the headline, at the engine level) --------------------

#[test]
fn a_torn_tail_is_dropped_and_the_file_is_repaired() {
    // The headline property, at the level the engine can be tested without
    // killing a process: a partial record at the end of the log is discarded,
    // every complete record before it survives, and the file is *truncated*
    // rather than merely ignored.
    //
    // The two records are written by a single evaluation (a program run twice
    // against one file would append twice, since the handle registry is
    // per-thread), then the file is damaged by hand and read back.
    let s = Scratch::new("torn");
    let p = s.db("t.ainl-db");
    run_str(&format!(
        r#"(do (def h (db-open "{p}"))
             (db-put h "a" "1")
             (db-put h "b" "2")
             (db-close h))"#
    ))
    .expect("writes");
    let good_len = s.raw("t.ainl-db").len();

    // A half-written record: the fixed part is there, the body is short.
    let mut raw = s.raw("t.ainl-db");
    raw.extend_from_slice(&5u32.to_le_bytes()); // key_len
    raw.extend_from_slice(&99u32.to_le_bytes()); // val_len: more than present
    raw.extend_from_slice(&0u32.to_le_bytes()); // crc
    raw.extend_from_slice(b"parti");
    s.put_raw("t.ainl-db", &raw);

    let v = run_str(&format!(
        r#"(do (def h (db-open "{p}")) (list (db-get-raw h "a") (db-get-raw h "b") (db-get-raw h "parti")))"#
    ))
    .expect("open must recover, not fail");
    assert_eq!(
        v,
        Value::List(ainl_core::ConsCell::from_values(vec![
            Value::str("1"),
            Value::str("2"),
            Value::Nil
        ])),
        "the complete records survive and the torn one is not served"
    );
    assert_eq!(
        s.raw("t.ainl-db").len(),
        good_len,
        "the torn tail must be truncated on disk, not just ignored"
    );

    // And the repair is *idempotent*: a second open replays the same records. A
    // recovery that only ignored the tail would pass the assertions above and
    // then lose the appended record on the next write.
    let v2 = run_str(&format!(
        r#"(do (def h (db-open "{p}")) (list (db-get-raw h "a") (db-get-raw h "b")))"#
    ))
    .expect("the second open must succeed");
    assert_eq!(v2.to_string(), "(\"1\" \"2\")");
}

#[test]
fn a_flipped_byte_in_a_record_is_caught_by_the_checksum() {
    // Every length still parses, so only the CRC can catch this — which is the
    // whole reason the format has one.
    let s = Scratch::new("crc");
    let p = s.db("c.ainl-db");
    run_str(&format!(
        r#"(do (def h (db-open "{p}"))
             (db-put h "a" "1")
             (db-put h "b" "2")
             (db-close h))"#
    ))
    .expect("writes");
    let mut raw = s.raw("c.ainl-db");
    let last = raw.len() - 1;
    raw[last] ^= 0xFF; // corrupt the final byte of "2"
    s.put_raw("c.ainl-db", &raw);

    let v = run_str(&format!(
        r#"(do (def h (db-open "{p}")) (list (db-get-raw h "a") (db-get-raw h "b")))"#
    ))
    .expect("open must recover");
    assert_eq!(
        v,
        Value::List(ainl_core::ConsCell::from_values(vec![
            Value::str("1"),
            Value::Nil
        ])),
        "the intact record survives; the corrupted one is not served"
    );
}

#[test]
fn a_corrupt_length_field_does_not_make_the_engine_trust_it() {
    // A 4 GiB length in a forty-byte file. A replay that trusted the field
    // would try to read — or allocate — that much.
    let s = Scratch::new("huge");
    let p = s.db("h.ainl-db");
    run_str(&format!(
        r#"(do (def h (db-open "{p}")) (db-put h "a" "1") (db-close h))"#
    ))
    .expect("writes");
    let mut raw = s.raw("h.ainl-db");
    let rec = raw.len() - (12 + 2);
    raw[rec..rec + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    s.put_raw("h.ainl-db", &raw);
    let v = run_str(&format!(r#"(db-get-raw (db-open "{p}") "a")"#)).expect("open must recover");
    assert_eq!(v, Value::Nil, "the record is rejected, not believed");
}

// ---- refusals --------------------------------------------------------------

#[test]
fn a_foreign_file_is_refused_with_the_shared_message() {
    let s = Scratch::new("foreign");
    let p = s.db("f.ainl-db");
    std::fs::write(&p, b"this is not a database, it is a text file").expect("write");
    let e = run_str(&format!(r#"(db-open "{p}")"#)).expect_err("must refuse");
    assert!(
        e.message().contains("is not an AINL database"),
        "got: {}",
        e.message()
    );
}

#[test]
fn a_future_version_is_refused_naming_the_version() {
    let s = Scratch::new("version");
    let p = s.db("v.ainl-db");
    let mut h = Vec::new();
    h.extend_from_slice(b"AINLDB");
    h.push(2);
    h.push(0);
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    s.put_raw("v.ainl-db", &h);
    let e = run_str(&format!(r#"(db-open "{p}")"#)).expect_err("must refuse");
    assert!(
        e.message().contains("is database version 2"),
        "the message must name the version, got: {}",
        e.message()
    );
    assert!(
        e.message().contains("reads version 1"),
        "and the version this AINL understands, got: {}",
        e.message()
    );
}

#[test]
fn an_arity_error_names_the_form_the_caller_should_have_written() {
    for (src, want) in [
        ("(db-open)", "db-open expects (db-open path)"),
        ("(db-open \"a\" \"b\")", "db-open expects (db-open path)"),
        (
            "(db-put 1 \"k\")",
            "db-put expects (db-put handle key value)",
        ),
        (
            "(db-put 1 \"k\" \"v\" \"x\")",
            "db-put expects (db-put handle key value)",
        ),
        (
            "(db-get-raw 1)",
            "db-get-raw expects (db-get-raw handle key)",
        ),
        ("(db-flush)", "db-flush expects (db-flush handle)"),
        ("(db-close)", "db-close expects (db-close handle)"),
    ] {
        let e = run_str(src).expect_err("must refuse");
        assert_eq!(e.message(), want, "for `{src}`");
    }
}

#[test]
fn a_type_error_names_the_operand_and_its_type() {
    for (src, want) in [
        ("(db-open 1)", "db-open expects a str path, got int"),
        ("(db-put 1 2 3)", "db-put expects a str key, got int"),
        ("(db-put 1 \"k\" 2)", "db-put expects a str value, got int"),
        ("(db-get-raw 1 2)", "db-get-raw expects a str key, got int"),
        (
            "(db-put \"x\" \"k\" \"v\")",
            "db-put expects a db handle, got str",
        ),
        (
            "(db-get-raw nil \"k\")",
            "db-get-raw expects a db handle, got nil",
        ),
    ] {
        let e = run_str(src).expect_err("must refuse");
        assert_eq!(e.message(), want, "for `{src}`");
    }
}

#[test]
fn a_handle_that_is_not_open_is_refused_by_number() {
    for (src, who) in [
        ("(db-put 7 \"k\" \"v\")", "db-put"),
        ("(db-get-raw 7 \"k\")", "db-get-raw"),
        ("(db-flush 7)", "db-flush"),
        ("(db-close 7)", "db-close"),
    ] {
        let e = run_str(src).expect_err("must refuse");
        assert_eq!(
            e.message(),
            format!("{who}: handle 7 is not open"),
            "for `{src}`"
        );
    }
    // Zero and a negative number are "not open" too, not a slot off the front
    // of the table.
    for n in [0, -1] {
        let e = run_str(&format!("(db-get-raw {n} \"k\")")).expect_err("must refuse");
        assert_eq!(
            e.message(),
            format!("db-get-raw: handle {n} is not open"),
            "handle {n} must be refused the same way, not read out of bounds"
        );
    }
}

#[test]
fn an_unwritable_path_is_refused_not_silently_ignored() {
    let s = Scratch::new("unwritable");
    let p = s.db("no-such-dir/deep/db.ainl-db");
    let e = run_str(&format!(r#"(db-open "{p}")"#)).expect_err("must refuse");
    assert_eq!(
        e.message(),
        format!("db-open: cannot open '{p}'"),
        "the message must name the path the caller wrote"
    );
}

// ---- the value model is unchanged -----------------------------------------

#[test]
fn a_handle_is_an_ordinary_int() {
    // The reason the handle is an int: no new `Value` variant, so `print`,
    // `=` and the JSON writer need no new arm on any backend, and `(= h 1)` is
    // the arithmetic a caller would expect.
    let s = Scratch::new("int");
    let p = s.db("i.ainl-db");
    let v = run_str(&format!(
        r#"(do (def h (db-open "{p}"))
             (def seen (list (= h 1) (< h 2) (+ h 10) (json-serialize h) h))
             (db-close h)
             seen)"#
    ))
    .expect("runs");
    // The fourth element is the *string* "1": json-serialize returns text, and
    // quoting it here is what proves the handle crossed it as the number 1 and
    // not as something json-serialize had to refuse.
    assert_eq!(
        v.to_string(),
        r#"(true true 11 "1" 1)"#,
        "a handle must behave like every other int"
    );
}
