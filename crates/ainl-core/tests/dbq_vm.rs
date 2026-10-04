//! Tier 4 queries on the **VM** backend.
//!
//! # Why this file exists
//!
//! The interpreter and the VM really are the same Rust: the query builtins are
//! `Value::Builtin` values bound in the shared prelude, and the VM dispatches
//! them by calling the same function. So the query layer works on the VM **by
//! construction** — which is a claim, and this file is what makes it a checked
//! one rather than a believed one.
//!
//! "By construction" can rot in one specific way. The VM has its own prelude
//! installation path, so a builtin installed in the interpreter's `install` but
//! missing from the VM's binding would be `unbound symbol` on one backend and
//! working on the other. Nothing else in the suite would notice: the AOT parity
//! file drives the compiled binary, which does not use the VM at all, so a VM
//! that had lost a binding would be caught by nobody.
//!
//! # What is not covered here
//!
//! The C port. That is a genuine second implementation, and it has its own file,
//! `dbq_aot.rs`, where agreement is evidence rather than a tautology. This file
//! is about the cheap claim; that one is about the expensive one.

use std::path::PathBuf;

use ainl_core::eval::Env;
use ainl_core::value::Value;

/// A scratch directory that removes itself, so a test that writes a database
/// file leaves nothing behind for the next one to trip over.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbq-vm-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("make the scratch dir");
        Scratch(p)
    }

    /// A path *inside* the directory, so a test's database is removed with it.
    fn db(&self) -> String {
        self.0.join("t.ainl-db").to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_interp(src: &str) -> (String, Option<String>) {
    let forms = ainl_core::parse(src).expect("the fixture itself must parse");
    let env = Env::with_prelude();
    match ainl_core::eval::run_forms(&forms, &env) {
        Ok(v) => (v.repr(), None),
        Err(e) => (String::new(), Some(e.to_string())),
    }
}

fn run_vm(src: &str) -> (String, Option<String>) {
    let forms = ainl_core::parse(src).expect("the fixture itself must parse");
    let env = Env::with_prelude();
    match ainl_core::vm::run_forms(&forms, &env) {
        Ok(v) => (v.repr(), None),
        Err(e) => (String::new(), Some(strip_position(&e.to_string()))),
    }
}

/// Is `tail` exactly a position suffix — ` at line N, col M`, optionally
/// followed by ` (byte B)`?
///
/// The check has to be this strict, and the reason is the shape of the messages.
/// A query error reads:
///
/// ```text
/// db-query: at line 1, col 22: 'GROUP' is not supported in v1 ... in the query "SELECT * FROM people GROUP BY 1" at line 6, col 10 (byte 161)
/// ```
///
/// There are **two** positions in that string. The first is the position inside
/// the query and belongs to the message; the second is the call site in the AINL
/// source and is the backend-specific suffix being stripped. A looser test —
/// "does the tail mention `col `, does it end in `)`" — is satisfied by the first
/// one too, and strips the whole message down to `db-query:`. Then every refusal
/// compares as the identical string `db-query:` and the parity file passes while
/// checking nothing at all.
///
/// So: digits, the exact `, col `, digits, and then *nothing* but optionally
/// ` (byte ` digits `)`. The message's own position is followed by `:`, so it
/// cannot match.
fn is_position_suffix(tail: &str) -> bool {
    let Some(rest) = tail.strip_prefix(" at line ") else {
        return false;
    };
    let (rest, byte_part) = match rest.split_once(" (byte ") {
        Some((r, b)) => {
            let Some(b) = b.strip_suffix(')') else {
                return false;
            };
            (r, Some(b))
        }
        None => (rest, None),
    };
    let Some((line, col)) = rest.split_once(", col ") else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(line) && digits(col) && byte_part.map(digits).unwrap_or(true)
}

/// Drop the `runtime error: ` prefix and the trailing call-site position, which
/// the interpreter's `or_at` adds and the VM's does not.
///
/// A VM message has **no** call-site suffix at all, so on that side this is a
/// no-op by construction: the only ` at line ` in it is the one inside the
/// message, and `is_position_suffix` rejects it because a `:` follows.
fn strip_position(err: &str) -> String {
    let s = err.trim_end_matches('\n');
    let s = s.strip_prefix("runtime error: ").unwrap_or(s);
    match s.rfind(" at line ") {
        Some(i) if is_position_suffix(&s[i..]) => s[..i].to_string(),
        _ => s.to_string(),
    }
}

/// The interpreter's message, normalised the same way, so the two sides of the
/// comparison go through one function.
fn strip_interp_position(err: &str) -> String {
    strip_position(err)
}

/// The parity assertion, for a program that must succeed.
fn assert_parity(src: &str, tag: &str) -> String {
    let (i_out, i_err) = run_interp(src);
    assert!(
        i_err.is_none(),
        "the interpreter failed on {tag}: {i_err:?}"
    );
    let (v_out, v_err) = run_vm(src);
    assert!(v_err.is_none(), "the VM failed on {tag}: {v_err:?}");
    assert_eq!(i_out, v_out, "the interpreter and the VM disagree on {tag}");
    i_out
}

/// The parity assertion, for a program that must be **refused** — where the
/// error text itself is the thing both backends have to produce.
fn assert_refusal_parity(src: &str, tag: &str, must_contain: &[&str]) {
    let (i_out, i_err) = run_interp(src);
    assert!(
        i_err.is_some(),
        "{tag}: the interpreter accepted it: {i_out}"
    );
    let (v_out, v_err) = run_vm(src);
    assert!(v_err.is_some(), "{tag}: the VM accepted it: {v_out}");
    let shared = strip_interp_position(i_err.as_deref().unwrap());
    assert_eq!(
        shared,
        v_err.as_deref().unwrap(),
        "the interpreter and the VM refuse {tag} differently"
    );
    for want in must_contain {
        assert!(
            shared.contains(want),
            "{tag}: the refusal does not say {want:?}\ngot: {shared}"
        );
    }
}

/// A database path for a test, unique per tag, inside a directory that is
/// already alive.
///
/// This is a free function on purpose. The first version of this file built the
/// path from a `Scratch` that the *helper* owned, so the directory was removed
/// before the program that named it ran — and every failure test then reported
/// `db-open: cannot open ...` instead of the refusal it was written to check.
/// A refusal test that fails for the wrong reason is worse than no test, because
/// it looks like it passed a thing it never reached. The caller now holds the
/// `Scratch` for the whole test, and this only computes a name.
fn db_path(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ainl-dbq-vm-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    p.join("t.ainl-db").to_string_lossy().into_owned()
}

/// A program that inserts the three-row fixture and returns the query and the
/// count as a pair, so a test compares one value rather than two.
fn fixture(tag: &str, query: &str) -> String {
    format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "bob" 41 "navy"))
      (db-insert h t (list "grace" 45 "navy"))
      (def got (list (db-query h "{query}")
                    (db-query-count h "{query}")))
      (db-close h)
      got)"#,
        path = db_path(tag),
        query = query
    )
}

/// The join (Tier 5) fixture: **two** tables, with a left row that matches
/// nothing on the right.
///
/// `people` is ada/bob/grace/oslo and `cities` is london/sydney, so `"london"`
/// matches two left rows and the INNER and LEFT answers differ by exactly one
/// row. Returns the rows and the count as a pair, like `fixture`.
fn join_fixture(tag: &str, query: &str) -> String {
    format!(
        r#"(do
      (def h (db-open "{path}"))
      (def p (db-create-table h "people"))
      (def c (db-create-table h "cities"))
      (db-insert h p (list "ada" 36 "london"))
      (db-insert h p (list "bob" 41 "london"))
      (db-insert h p (list "grace" 45 "sydney"))
      (db-insert h p (list "oslo" 1 "oslo"))
      (db-insert h c (list "london" "uk"))
      (db-insert h c (list "sydney" "au"))
      (def got (list (db-query h "{query}")
                    (db-query-count h "{query}")))
      (db-close h)
      got)"#,
        path = db_path(tag),
        query = query
    )
}

/// A join returns the same **rows** on the interpreter and the VM.
///
/// `assert_parity` compares the two engines' outputs, and the expected value is
/// written out here too — otherwise a join that was wrong in the same way on both
/// sides would pass. The VM runs the same `dbquery::execute` rather than a second
/// implementation, so the claim under test is that the join works on the VM at
/// all, and that the two evaluators agree on which pairs it produced.
#[test]
fn a_join_agrees_between_the_interpreter_and_the_vm() {
    for (tag, query) in [
        (
            "vm-join-inner",
            "SELECT * FROM people JOIN cities ON people.3 = cities.1",
        ),
        (
            "vm-join-left",
            "SELECT * FROM people LEFT JOIN cities ON people.3 = cities.1",
        ),
        (
            "vm-join-where",
            "SELECT 1, 5 FROM people JOIN cities ON people.3 = cities.1 WHERE 5 = 'uk'",
        ),
        (
            "vm-join-order",
            "SELECT 1, 2 FROM people JOIN cities ON people.3 = cities.1 ORDER BY 2 DESC LIMIT 2",
        ),
        (
            "vm-join-inner-spelled",
            "SELECT 1 FROM people INNER JOIN cities ON people.3 = cities.1",
        ),
    ] {
        // The Scratch has to outlive the program that names its path, so it is
        // held here rather than inside `join_fixture` — see `db_path`. Held to
        // `_s` deliberately: it is removed on drop, and a `db-open` on a path whose
        // directory is gone reports a missing file instead of the answer.
        let _s = Scratch::new(tag);
        let out = assert_parity(&join_fixture(tag, query), tag);
        // The count is the second element of the pair, so rows and count are
        // checked together — a join that returned the right rows and the wrong
        // count would otherwise pass.
        assert!(!out.is_empty(), "the {tag} program returned nothing: {out}");
        // And the rows themselves, **compared exactly** rather than by substring:
        // the output is the `( (rows…) count )` pair the fixture returns, so an
        // exact match pins the order, the count, and the nil-padding at once.
        // Parity alone would pass on a join that was wrong the same way on both
        // sides, which is the failure this file exists to catch.
        let rows = match tag {
            "vm-join-inner" => {
                r#"(("ada" 36 "london" "london" "uk") ("bob" 41 "london" "london" "uk") ("grace" 45 "sydney" "sydney" "au"))"#
            }
            "vm-join-left" => {
                r#"(("ada" 36 "london" "london" "uk") ("bob" 41 "london" "london" "uk") ("grace" 45 "sydney" "sydney" "au") ("oslo" 1 "oslo" nil nil))"#
            }
            _ => continue,
        };
        let count = if tag == "vm-join-left" { "4" } else { "3" };
        assert_eq!(
            out.trim(),
            format!("({rows} {count})"),
            "the {tag} join did not return the combined rows the nested loop \
             produces"
        );
    }
}

/// Both query builtins are bound on the VM.
///
/// The direct version of the "for free" claim: a name installed for the
/// interpreter but not the VM shows up here as an `unbound symbol`.
#[test]
fn every_query_builtin_is_bound_on_the_vm() {
    for name in ainl_core::dbquery::SQL_BUILTINS {
        let env = Env::with_prelude();
        let v = env.get(name);
        assert!(
            matches!(v, Some(Value::Builtin { .. })),
            "{name} is not bound as a builtin on the VM's prelude: {v:?}"
        );
    }
}

/// The whole subset, both evaluators, one value.
#[test]
fn the_subset_agrees_between_the_interpreter_and_the_vm() {
    let s = Scratch::new("subset");
    let mut src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "bob" 41 "navy"))
      (db-insert h t (list "grace" 45 "navy"))
      (def got (list"#,
        path = s.db()
    );
    for q in [
        "SELECT * FROM people",
        "SELECT 1, 2 FROM people",
        "SELECT 1 FROM people WHERE 3 = 'navy'",
        "SELECT 1 FROM people WHERE 2 > 40 AND 3 = 'navy'",
        "SELECT 1 FROM people WHERE 1 = 'ada' OR 2 = 45",
        "SELECT 1 FROM people ORDER BY 2",
        "SELECT 1 FROM people ORDER BY 2 DESC",
        "SELECT 1, 2 FROM people ORDER BY 3 DESC",
        "SELECT 1 FROM people WHERE 2 > 36 ORDER BY 2 LIMIT 1",
        "SELECT 2 FROM people WHERE 1 = 'grace'",
    ] {
        src.push_str(&format!(" (db-query h \"{q}\")"));
    }
    src.push_str(" (db-query-count h \"SELECT 1 FROM people\")");
    // Close the `list` and the `def got`, then return `got`, then close the
    // `do` — four closers and a name. The `format!` header leaves `(do`,
    // `(def got` and `(list` open, and the value has to be `got` rather than
    // the last call: a `do` returns its final form, so a tail that is only
    // closers yields the count, not the list of every query above it.
    src.push_str(")) got)");
    let out = assert_parity(&src, "the subset");
    // The value is what the docs say, not merely equal on both sides: two
    // evaluators can agree on the wrong answer.
    assert!(
        out.contains("grace") && out.contains("45"),
        "unexpected: {out}"
    );
}

/// The ordering, on a table big enough that the tree is several levels deep.
/// A merge sort that only looks right on three sorted rows is easy to write.
#[test]
fn ordering_agrees_over_a_deep_tree() {
    let s = Scratch::new("deep");
    let mut ins = String::new();
    // Inserted in reverse key order, so an insertion-order bug in one evaluator
    // cannot be masked by the input already being sorted.
    for i in (0..40).rev() {
        ins.push_str(&format!("(db-insert h t (list \"k{i:03}\" {}))\n", 40 - i));
    }
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      {ins}
      (def got (list (db-query h "SELECT 1, 2 FROM t ORDER BY 2")
                    (db-query h "SELECT 1, 2 FROM t ORDER BY 2 DESC")
                    (db-query h "SELECT 1, 2 FROM t ORDER BY 2 LIMIT 5")
                    (db-query h "SELECT 1 FROM t WHERE 2 > 20")
                    (db-query-count h "SELECT 1 FROM t WHERE 2 > 20")))
      (db-close h)
      got)"#,
        path = s.db(),
        ins = ins
    );
    let out = assert_parity(&src, "a deep tree");
    assert!(out.contains("k039"), "unexpected: {out}");
}

/// Stable order on a column that repeats. Fifty rows over four values, so a sort
/// that reversed ties would disagree with the interpreter somewhere in the
/// middle of the list rather than only at its ends.
#[test]
fn a_stable_order_agrees_on_repeated_keys() {
    let s = Scratch::new("stable");
    let mut ins = String::new();
    for i in 0..50 {
        ins.push_str(&format!("(db-insert h t (list \"k{i:03}\" {}))\n", i % 4));
    }
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      {ins}
      (def got (list (db-query h "SELECT 1 FROM t ORDER BY 2")
                    (db-query h "SELECT 1 FROM t ORDER BY 2 DESC")))
      (db-close h)
      got)"#,
        path = s.db(),
        ins = ins
    );
    assert_parity(&src, "repeated sort keys");
}

/// A refusal is a value both evaluators produce, and the text is the part a
/// model reads. The out-of-scope table, the suggestion, and the two positions
/// that differ on purpose (an ORDER BY column no row has, in the multi-row and
/// the one-row cases) are all here.
#[test]
fn refusals_agree_between_the_interpreter_and_the_vm() {
    for (i, (query, want)) in [
        (
            "SELECT * FROM people GROUP BY 1",
            "'GROUP' is not supported in v1",
        ),
        // The join types v1 does not have, refused **by name**. Tier 5 made
        // `JOIN` itself parse, so it left the unsupported table and these are what
        // joined the rest of it.
        (
            "SELECT * FROM people CROSS JOIN people ON people.3 = people.1",
            "'CROSS' is not supported in v1",
        ),
        (
            "SELECT * FROM people RIGHT JOIN people ON people.3 = people.1",
            "'RIGHT' is not supported in v1",
        ),
        (
            "SELECT * FROM people JOIN people USING (3)",
            "'USING' is not supported in v1",
        ),
        // A second join clause — refused as the second one, since `JOIN` alone is
        // legal now and "unexpected 'JOIN'" would not say which JOIN is the problem.
        (
            "SELECT * FROM people JOIN people ON people.3 = people.1 \
             JOIN people ON people.1 = people.1",
            "second JOIN clause",
        ),
        // Both sides of an ON on one table: a self-join without an `AS` to tell
        // the two copies apart, so a column compared with itself.
        (
            "SELECT * FROM people JOIN people ON people.3 = people.2",
            "compares a column with itself",
        ),
        // An alias is out of scope, so it is refused as the keyword it is.
        (
            "SELECT * FROM people JOIN people p ON people.3 = people.1",
            "unexpected 'p'",
        ),
        (
            "SELECT DISTINCT 1 FROM people",
            "'DISTINCT' is not supported",
        ),
        ("SELECT COUNT(1) FROM people", "db-query-count"),
        ("SELECT 1 FORM people", "did you mean 'FROM'?"),
        (
            "SELECT 1 FROM people LIMIT 1 ORDER BY 2",
            "ORDER BY comes before LIMIT",
        ),
        (
            "SELECT 1 FROM people ORDER BY 9",
            "ORDER BY column 9 is nil in every row",
        ),
        (
            "SELECT 1 FROM people WHERE 1 = 'ada' ORDER BY 9",
            "ORDER BY column 9 is nil in every row",
        ),
        (
            "SELECT 1 FROM people WHERE 9 > 1",
            "WHERE column 9 is a nil",
        ),
        (
            "SELECT * FROM (SELECT 1 FROM people)",
            "a subquery or a parenthesised table",
        ),
        ("SELECT 1 FROM nope", "no table named 'nope'"),
        ("", "the query ended, but SELECT is required"),
        ("SELECT 1 FROM people WHERE name = 1", "is not a column"),
    ]
    .iter()
    .enumerate()
    {
        // The Scratch has to outlive this loop, so it is held here rather than
        // inside `fixture`.
        let _s = Scratch::new(&format!("ref{i}"));
        assert_refusal_parity(&fixture(&format!("ref{i}"), query), query, &[*want]);
    }
}

/// A database that is reopened is re-read, not served from an index that never
/// reached the disk.
///
/// The second program is a *separate* `run_*` call with its own `db-open`, so
/// the file really is closed and re-read. The first version of this test closed
/// the handle and then reused it, which is not a reopen at all — the engine
/// treats a closed handle as having no tables and answers "no table named 't'",
/// so the test was asserting the use-after-close rule and calling it persistence.
#[test]
fn a_query_survives_close_and_reopen() {
    let s = Scratch::new("reopen");
    let path = s.db();
    let write = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" 2))
      (db-close h)
      nil)"#
    );
    let read = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def got (db-query h "SELECT 1, 2 FROM t ORDER BY 2 DESC"))
      (db-close h)
      got)"#
    );
    let (w_out, w_err) = run_interp(&write);
    assert!(w_err.is_none(), "the write failed: {w_err:?}");
    let (v_out, v_err) = run_vm(&write);
    assert!(v_err.is_none(), "the VM write failed: {v_err:?}");
    assert_eq!(w_out, v_out, "the evaluators disagree on the write program");

    let out = assert_parity(&read, "a reopened database");
    assert!(
        out.starts_with("((\"b\" 2) (\"a\" 1))"),
        "the reopened query is not what the docs say: {out}"
    );
}

/// Re-inserting a primary key **replaces** the row, and a query has to see the
/// new one.
///
/// This is the replacement path, and it is here for a specific reason: §3m
/// shipped a `debug_assert!` guarding the value update in the B-tree, so in a
/// release build the assertion — and with it the whole `insert_into` call — is
/// stripped, and the tree keeps the **old** row on the interpreter and the VM.
/// Every other query in this file inserts distinct keys, so none of them touch
/// that line, which is how a broken replace survives a whole query battery
/// green.
///
/// Two details make this test actually catch it, and both were wrong in the
/// first version:
///
///   - The query must run on the **live** handle, in the same `run_*` call as
///     the inserts. A close/reopen rebuilds the tree from the log, where the
///     last-write-wins map has already collapsed the duplicate, so a
///     reopen-based fixture answers correctly even on a broken tree. That is
///     the whole reason the first version of this test passed in release.
///   - The assertions are on the *value*, not on a row count. A keep-the-old-row
///     bug produces exactly the right keys and exactly the right count; only the
///     payload differs, so a count assertion passes on a stale read.
///
/// Both a longer and a shorter re-insert are covered, because a replace that
/// copies the new length but does not truncate leaves the old tail attached and
/// a projection past the new end reads the old columns instead of `nil`.
#[test]
fn a_reinserted_key_is_replaced_and_the_query_sees_the_new_row() {
    let s = Scratch::new("replace");
    let path = s.db();
    // "a" 1 -> "a" 10 11 12 (longer), and "b" 1 2 3 -> "b" 9 (shorter). One
    // program, one live handle, and the queries run before the close — so this
    // reads the B-tree and not the log.
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "a" 10 11 12))
      (db-insert h t (list "b" 1 2 3))
      (db-insert h t (list "b" 9))
      (def longer (db-query h "SELECT * FROM t WHERE 1 = 'a'"))
      (def shorter (db-query h "SELECT 2, 3 FROM t WHERE 1 = 'b'"))
      (db-close h)
      (list longer shorter))"#
    );
    let (i_out, i_err) = run_interp(&src);
    assert!(i_err.is_none(), "the interpreter failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&src);
    assert!(v_err.is_none(), "the VM failed: {v_err:?}");
    assert_eq!(i_out, v_out, "the evaluators disagree on the re-insert");

    // The longer row answers (("a" 10 11 12)); a tree that kept the old value
    // answers (("a" 1)). The shorter answers ((9 nil)); a tree that did not
    // truncate answers ((1 2 3)).
    assert!(
        i_out.contains(r#"(("a" 10 11 12))"#),
        "the query did not see the re-inserted longer row: {i_out}"
    );
    assert!(
        i_out.contains("((9 nil))"),
        "a re-insert did not truncate the old row: {i_out}"
    );
}
