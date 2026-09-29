//! Tier 4 tables on the **VM** backend, which the card's PO decision names as
//! one of the three backends that must agree.
//!
//! # Why this file exists when the engine is shared
//!
//! Interpreter and VM really are the same Rust: the table builtins are
//! `Value::Builtin` values bound in the shared prelude, and `vm.rs:1385`
//! dispatches `Value::Builtin { f, .. }` by calling `f` directly. So the table
//! layer works on the VM **by construction**, and there is nothing to
//! hand-port — which is exactly the "for free" claim the card's body makes, and
//! which the PO's decision narrowed to two backends instead of four.
//!
//! "By construction" is still a claim, and this is the file that makes it a
//! checked one. The way it can rot is specific: the VM has its own prelude
//! installation path, and a builtin bound in `install_prelude` but missing from
//! the VM's own binding would be `unbound symbol` on one backend and work on the
//! other. Nothing else in the suite would notice, because the parity files for
//! the *compiled* binary go through `ainl`, which does not use the VM.
//!
//! The C port is the opposite case and has its own file, `dbtab_aot.rs`: that
//! one is a genuine second implementation, and there the agreement is evidence
//! rather than a tautology.

use std::path::PathBuf;

use ainl_core::eval::Env;
use ainl_core::value::Value;

/// Run `src` in the tree-walking interpreter, returning `(repr, error message)`.
fn run_interp(src: &str) -> (String, Option<String>) {
    let forms = ainl_core::parse(src).expect("the fixture itself must parse");
    let env = Env::with_prelude();
    match ainl_core::eval::run_forms(&forms, &env) {
        Ok(v) => (v.repr(), None),
        Err(e) => (String::new(), Some(e.to_string())),
    }
}

/// Run `src` in the VM, returning `(repr, error message)`.
///
/// The error is stringified *without* the position suffix, because the
/// interpreter's `or_at` adds one and the VM's does not — the same wrapper gap
/// `dbtab_aot.rs` normalizes for the AOT backend, and for the same reason.
fn run_vm(src: &str) -> (String, Option<String>) {
    let forms = ainl_core::parse(src).expect("the fixture itself must parse");
    let env = Env::with_prelude();
    match ainl_core::vm::run_forms(&forms, &env) {
        Ok(v) => (v.repr(), None),
        Err(e) => (String::new(), Some(strip_position(&e.to_string()))),
    }
}

fn strip_position(err: &str) -> String {
    let s = err.trim_end_matches('\n');
    let s = s.strip_prefix("runtime error: ").unwrap_or(s);
    match s.find(" at line ") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

fn strip_interp_position(err: &str) -> String {
    let s = err.trim_end_matches('\n');
    let s = s.strip_prefix("runtime error: ").unwrap_or(s);
    match s.find(" at line ") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// A scratch directory that removes itself, so a test that writes a database
/// file leaves nothing behind for the next one to trip over.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbtab-vm-{}-{tag}-{:?}",
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

/// The five table builtins are all bound on the VM.
///
/// The direct version of the "for free" claim: if any of the five were missing
/// from the VM's prelude, this is where it shows, and the failure is an
/// `unbound symbol` naming the missing one.
#[test]
fn every_table_builtin_is_bound_on_the_vm() {
    for name in ainl_core::dbtab::TABLE_BUILTINS {
        let env = Env::with_prelude();
        let v = env.get(name);
        assert!(
            matches!(v, Some(Value::Builtin { .. })),
            "{name} is not bound as a builtin on the VM's prelude: {v:?}"
        );
    }
}

/// Create, insert, select, enumerate, delete — the same values from both
/// evaluators.
#[test]
fn a_table_round_trip_agrees_between_the_interpreter_and_the_vm() {
    let s = Scratch::new("roundtrip");
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "grace" 45 "navy"))
      (db-insert h t (list "bob" 41 "navy"))
      (def got (list (db-select h t "grace")
                    (db-all-rows h t)
                    (db-delete-row h t "bob")
                    (db-all-rows h t)
                    (db-select h t "bob")))
      (db-close h)
      got)"#,
        path = s.db()
    );
    let (i_out, i_err) = run_interp(&src);
    assert!(i_err.is_none(), "the interpreter failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&src);
    assert!(v_err.is_none(), "the VM failed: {v_err:?}");
    assert_eq!(
        i_out, v_out,
        "the interpreter and the VM disagree on a table round trip"
    );
    // And the value itself is what the docs say it is, not merely equal on both
    // sides — two backends can agree on the wrong answer.
    assert!(
        i_out.contains("grace") && i_out.contains("nil"),
        "unexpected result: {i_out}"
    );
}

/// The order is the tree's walk, so it has to be the same on both evaluators.
/// 40 keys, so the tree is several levels deep and an insertion-order bug in
/// one evaluator cannot hide.
#[test]
fn rows_come_back_in_the_same_order_on_the_interpreter_and_the_vm() {
    let s = Scratch::new("order");
    let mut ins = String::new();
    for i in (0..40).rev() {
        ins.push_str(&format!("(db-insert h t (list \"k{i:03}\" {i}))\n"));
    }
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      {ins}
      (db-delete-row h t "k007")
      (db-delete-row h t "k023")
      (db-insert h t (list "k007" 7))
      (def got (db-all-rows h t))
      (db-close h)
      got)"#,
        path = s.db()
    );
    let (i_out, i_err) = run_interp(&src);
    assert!(i_err.is_none(), "the interpreter failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&src);
    assert!(v_err.is_none(), "the VM failed: {v_err:?}");
    assert_eq!(
        i_out, v_out,
        "the interpreter and the VM disagree on the walk order"
    );
    assert_eq!(
        i_out.matches("k007").count(),
        1,
        "a re-inserted key must appear exactly once: {i_out}"
    );
    assert!(
        !i_out.contains("k023"),
        "a deleted key must not come back: {i_out}"
    );
}

/// Persistence, on the VM: close, reopen, and the rows and the order are back.
///
/// The reopen is a *second* `run_*` call, not a second handle in one program,
/// so the file really is closed and re-read rather than served from a cached
/// index — an index that never reached the disk passes every same-process test.
#[test]
fn a_table_survives_a_reopen_on_the_vm() {
    let s = Scratch::new("reopen");
    let path = s.db();
    let write = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "c" 3))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" 2))
      (db-insert h t (list "d" 4))
      (db-delete-row h t "d")
      (db-close h))"#,
        path = path
    );
    let (_, i_err) = run_interp(&write);
    assert!(i_err.is_none(), "the interpreter write failed: {i_err:?}");

    let read = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def got (list (db-all-rows h "t") (db-select h "t" "a") (db-select h "t" "d")))
      (db-close h)
      got)"#,
        path = path
    );
    let (i_out, i_err) = run_interp(&read);
    assert!(i_err.is_none(), "the interpreter read failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&read);
    assert!(v_err.is_none(), "the VM read failed: {v_err:?}");
    assert_eq!(i_out, v_out, "the two evaluators disagree after a reopen");
    assert!(
        !i_out.contains("d"),
        "a deleted row must stay deleted across a reopen: {i_out}"
    );
    // The order survived too, not just the contents.
    let a = i_out.find("\"a\"").expect("row a");
    let b = i_out.find("\"b\"").expect("row b");
    let c = i_out.find("\"c\"").expect("row c");
    assert!(a < b && b < c, "the order was not preserved: {i_out}");
}

/// An **empty** table has to survive a reopen, which is the case that only
/// works because `db-create-table` writes a marker record: a table with no rows
/// would otherwise leave nothing in the log to rediscover it from.
#[test]
fn an_empty_table_survives_a_reopen_on_the_vm() {
    let s = Scratch::new("empty");
    let path = s.db();
    let create = format!(
        r#"(do (def h (db-open "{path}")) (db-create-table h "t") (db-close h))"#,
        path = path
    );
    let (_, i_err) = run_interp(&create);
    assert!(i_err.is_none(), "the interpreter create failed: {i_err:?}");

    let read = format!(
        r#"(do (def h (db-open "{path}"))
             (def got (db-all-rows h "t"))
             (db-close h)
             got)"#,
        path = path
    );
    let (i_out, i_err) = run_interp(&read);
    assert!(i_err.is_none(), "the interpreter read failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&read);
    assert!(v_err.is_none(), "the VM read failed: {v_err:?}");
    assert_eq!(
        i_out, v_out,
        "the two evaluators disagree on an empty table"
    );
    assert_eq!(i_out, "()", "the table must exist and be empty, not absent");
}

// ---- replace on an existing key --------------------------------------------

/// Re-inserting an existing primary key replaces the row, on both evaluators.
///
/// # Why asserting that the two evaluators agree was not enough
///
/// The bug this pins had its update gated behind a `debug_assert!` argument in
/// `BTree::insert`, so a replace ran in debug and was stripped in release: the
/// old row survived a re-insert in the mode that ships, and the AOT C port —
/// which used an unconditional `if` and was right in both — disagreed with
/// both. `cargo test` is debug, so the entire suite was green.
///
/// The version of this test that only compared the interpreter against the VM
/// would have stayed green too, and would have stayed green *under the bug*:
/// the two evaluators share the table layer as the same Rust, so they agree by
/// construction, and "both returned the stale row" is a perfectly consistent
/// pair of answers. A parity test that cannot fail is a test that does not
/// exist, so this one asserts the value — that `second` came back and `first`
/// did not — on each evaluator separately, and then that they agree.
#[test]
fn replacing_an_existing_key_replaces_the_row_on_both_evaluators() {
    let s = Scratch::new("replace");
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "k" "first"))
      (db-insert h t (list "k" "second"))
      (def got (list (db-select h t "k") (db-all-rows h t)))
      (db-close h)
      got)"#,
        path = s.db()
    );
    let (i_out, i_err) = run_interp(&src);
    assert!(i_err.is_none(), "the interpreter failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&src);
    assert!(v_err.is_none(), "the VM failed: {v_err:?}");

    // The value, on each evaluator. A dropped replace leaves the row present
    // and single-keyed, so only the value distinguishes the two.
    for (who, out) in [("interpreter", &i_out), ("VM", &v_out)] {
        assert!(
            out.contains("second"),
            "the {who} kept the OLD row after re-inserting an existing primary \
             key: {out}"
        );
        assert!(
            !out.contains("first"),
            "the {who} index still holds the replaced value: {out}"
        );
    }
    assert_eq!(
        i_out, v_out,
        "the interpreter and the VM disagree on a replace"
    );
}

/// The same replace in a tree deep enough that the replaced key is not in the
/// root, so the update walks more than one level.
///
/// The first version of this test deleted `k007` and re-inserted it, and it
/// **passed under the bug**. That was not luck and not a near miss: a
/// `db-delete-row` removes the key from the tree, so the re-insert that follows
/// is an insert of a *new* key and takes the code path below this `if`. The
/// test was named "replaced a row" and was structurally incapable of exercising
/// a replace. It is now a plain re-insert of a key that is still present,
/// which is the operation `db-insert` documents as last-write-wins.
#[test]
fn a_replaced_row_comes_back_new_from_a_deep_tree() {
    let s = Scratch::new("replace-deep");
    let mut ins = String::new();
    for i in (0..40).rev() {
        ins.push_str(&format!("(db-insert h t (list \"k{i:03}\" {i}))\n"));
    }
    let src = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      {ins}
      (db-insert h t (list "k007" "replaced"))
      (def got (list (db-select h t "k007") (db-all-rows h t)))
      (db-close h)
      got)"#,
        path = s.db()
    );
    let (i_out, i_err) = run_interp(&src);
    assert!(i_err.is_none(), "the interpreter failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&src);
    assert!(v_err.is_none(), "the VM failed: {v_err:?}");

    for (who, out) in [("interpreter", &i_out), ("VM", &v_out)] {
        assert!(
            out.contains("\"k007\" \"replaced\""),
            "the {who} did not return the re-inserted value: {out}"
        );
        assert!(
            !out.contains("\"k007\" 7"),
            "the {who} returned the pre-replace value for a re-inserted key: {out}"
        );
    }
    assert_eq!(
        i_out, v_out,
        "the interpreter and the VM disagree on a deep replace"
    );
}

/// The replaced value has to be on disk, not only in the index a process is
/// holding: close, reopen in a *separate* run, and the new value must be what
/// comes back.
///
/// # This test cannot catch a live-index replace bug, and that is not a defect
///
/// It is a persistence gate, and it is worth saying what it is blind to
/// because the blindness is structural rather than accidental.
///
/// `Db::put` maintains its own flat `HashMap<String, String>` index alongside
/// the log — see `db.rs:339`, `self.index.insert(key, value)` on every write.
/// That map is last-write-wins, so by the time a reopen calls
/// `TableSet::rebuild`, the map already holds only the *newest* value for the
/// key. `rebuild` therefore feeds the tree one insert per surviving key, and
/// every one of them is an insert into an empty tree: the replace branch in
/// `BTree::insert` is never reached, no matter what the bug in it is.
///
/// So a dropped replace is invisible to any reopen-based check, in every
/// backend. What this test can catch is a replace that is not *durable* — a
/// value that reads back correctly in-process and never reaches the file, which
/// is a real and separate defect, and the reason
/// `a_table_survives_a_reopen_on_the_vm` exists. The in-process tests above
/// are what hold the replace path, and each was checked by reverting the fix
/// and watching it fail.
#[test]
fn a_replaced_row_survives_a_reopen_on_the_vm() {
    let s = Scratch::new("replace-reopen");
    let path = s.db();
    let write = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "k" "first"))
      (db-insert h t (list "k" "second"))
      (db-close h))"#,
        path = path
    );
    let (_, i_err) = run_interp(&write);
    assert!(i_err.is_none(), "the interpreter write failed: {i_err:?}");

    let read = format!(
        r#"(do
      (def h (db-open "{path}"))
      (def got (list (db-select h "t" "k") (db-all-rows h "t")))
      (db-close h)
      got)"#,
        path = path
    );
    let (i_out, i_err) = run_interp(&read);
    assert!(i_err.is_none(), "the interpreter read failed: {i_err:?}");
    let (v_out, v_err) = run_vm(&read);
    assert!(v_err.is_none(), "the VM read failed: {v_err:?}");

    for (who, out) in [("interpreter", &i_out), ("VM", &v_out)] {
        assert!(
            out.contains("second") && !out.contains("first"),
            "the {who} did not read back the replaced value after a reopen: {out}"
        );
    }
    assert_eq!(
        i_out, v_out,
        "the two evaluators disagree about a replaced row after a reopen"
    );
}

/// Every table refusal, on both evaluators, with the same message.
///
/// The message is compared after the interpreter's `runtime error: ` / `at line
/// L, col C` wrapper is removed — the same normalization `dbtab_aot.rs` does for
/// the AOT runtime, and for the same reason: the wrapper is a property of the
/// tree-walk evaluator's error path and predates this card.
#[test]
fn every_table_refusal_agrees_between_the_interpreter_and_the_vm() {
    let cases: &[(&str, &str)] = &[
        (
            "a table that does not exist",
            r#"(do (def h (db-open "r.ainl-db")) (db-insert h "nope" (list "a")))"#,
        ),
        (
            "a composite primary key",
            r#"(do (def h (db-open "r.ainl-db")) (db-create-table h "t")
                 (db-insert h "t" (list (list 1 2) "x")))"#,
        ),
        (
            "an empty row",
            r#"(do (def h (db-open "r.ainl-db")) (db-create-table h "t")
                 (db-insert h "t" (list)))"#,
        ),
        (
            "a row that is not a list",
            r#"(do (def h (db-open "r.ainl-db")) (db-create-table h "t")
                 (db-insert h "t" "a"))"#,
        ),
        (
            "a handle that is not open",
            r#"(do (db-create-table 99 "t"))"#,
        ),
        (
            "a table name that is not a str",
            r#"(do (def h (db-open "r.ainl-db")) (db-create-table h 7))"#,
        ),
        (
            "selecting from a table that does not exist",
            r#"(do (def h (db-open "r.ainl-db")) (db-select h "nope" "a"))"#,
        ),
        (
            "deleting from a table that does not exist",
            r#"(do (def h (db-open "r.ainl-db")) (db-delete-row h "nope" "a"))"#,
        ),
    ];
    for (what, src) in cases {
        // Each case gets its own directory, because two cases writing the same
        // database file would let the first one's `db-create-table` make the
        // second one's "no such table" pass for the wrong reason.
        let s = Scratch::new(&format!("refusal-{}", what.len()));
        let src = src.replace("r.ainl-db", &s.db());
        let (_, i_err) = run_interp(&src);
        let (_, v_err) = run_vm(&src);
        let i_err = i_err.unwrap_or_else(|| panic!("{what}: the interpreter accepted it"));
        let v_err = v_err.unwrap_or_else(|| panic!("{what}: the VM accepted it"));
        assert_eq!(
            strip_interp_position(&i_err),
            strip_position(&v_err),
            "{what}: the two evaluators disagree\ninterp: {i_err}\nvm: {v_err}"
        );
    }
}
