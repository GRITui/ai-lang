//! Tier 4 table layer: the same operations in the interpreter and in a compiled
//! binary, with identical results.
//!
//! # Why this file is separate from `db_kv_aot.rs`
//!
//! Because the claim is a different one. That file checks that a *flat* value
//! store behaves the same on both engines — one key, one value, no structure.
//! This one checks that a **B-tree** does, which is a much stronger claim and
//! fails in a way that file's cannot catch: two implementations can agree on
//! every key and every value while disagreeing about the *order* of a walk, or
//! about which row a delete left behind after the tree rebalanced. Those are
//! only observable through `db-all-rows` and through the tree's height, so they
//! need their own assertions.
//!
//! # What makes agreement evidence rather than a tautology
//!
//! The C runtime is a hand-port, not a binding: there is no FFI anywhere in this
//! project, so `ainl-core/src/dbtab.rs` and the `dbt_*` block of
//! `ainl-cc/src/runtime.c` share no code. That is the point of the file. Two
//! implementations that agree on a hundred operations are more likely to be two
//! implementations of one specification than two that happen to differ.
//!
//! The risk it cannot cover is *shared misunderstanding*: both ports could read
//! the same comment and get the same thing wrong. The defence there is the Rust
//! unit tests, which check the tree's own invariants directly, and this file's
//! structural assertions, which pin the shape both engines must produce.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory, unique per tag, removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbtabaot-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("make the scratch dir");
        Scratch { path: p }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn ainl() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    if p.ends_with("deps") {
        p.pop();
    }
    p.push(if cfg!(windows) { "ainl.exe" } else { "ainl" });
    assert!(
        p.exists(),
        "the ainl CLI was not found at {} — run `cargo build` first",
        p.display()
    );
    p
}

/// Compile `src` to a native binary, returning `(binary, generated C)`.
fn build(src: &str, tag: &str, dir: &Path) -> (PathBuf, PathBuf) {
    let ainl_path = dir.join("p.ainl");
    let c_path = dir.join("p.c");
    let bin_path = dir.join(format!("{tag}.bin"));
    std::fs::write(&ainl_path, src).expect("write the program");

    let out = Command::new(ainl())
        .arg("compile")
        .arg(&ainl_path)
        .arg("-o")
        .arg(&bin_path)
        .arg("--keep-c")
        .arg(&c_path)
        .output()
        .expect("run ainl compile");
    assert!(
        out.status.success(),
        "ainl compile refused the {tag} program: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (bin_path, c_path)
}

fn run_interp(src: &str, dir: &Path) -> (String, String, bool) {
    let p = dir.join("i.ainl");
    std::fs::write(&p, src).expect("write the program");
    let out = Command::new(ainl())
        .arg("run")
        .arg(&p)
        .current_dir(dir)
        .output()
        .expect("run ainl");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn run(bin: &Path, dir: &Path) -> (String, String, bool) {
    let out = Command::new(bin)
        .current_dir(dir)
        .output()
        .expect("run the binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// The parity assertion: the same program, both engines, byte-identical on both
/// streams and the same exit code.
///
/// Separate directories on purpose — a shared one would let the interpreter's
/// database answer the binary's read, and the test would pass for the wrong
/// reason.
fn assert_parity(src: &str, tag: &str) {
    let si = Scratch::new(&format!("{tag}-interp"));
    let sc = Scratch::new(&format!("{tag}-aot"));
    let (i_out, i_err, i_ok) = run_interp(src, &si.path);
    let (bin, _) = build(src, tag, &sc.path);
    let (c_out, c_err, c_ok) = run(&bin, &sc.path);

    assert_eq!(
        (i_ok, c_ok),
        (true, true),
        "the {tag} program: interpreter ok={i_ok} compiled ok={c_ok}\nstderr interp: {i_err}\nstderr aot: {c_err}"
    );
    assert_eq!(
        i_out, c_out,
        "the {tag} program: stdout differs between the interpreter and the compiled binary"
    );
    assert_eq!(
        i_err, c_err,
        "the {tag} program: stderr differs between the interpreter and the compiled binary"
    );
}

// ---- the programs ----------------------------------------------------------

/// A full round trip: create, insert, select, all-rows, delete, all-rows.
fn round_trip() -> String {
    r#"(do
      (def h (db-open "people.db"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "grace" 45 "navy"))
      (db-insert h t (list "bob" 41 "navy"))
      (print (db-select h t "grace"))
      (print (db-all-rows h t))
      (print (db-delete-row h t "bob"))
      (print (db-all-rows h t))
      (print (db-select h t "bob"))
      (db-close h))"#
        .to_string()
}

#[test]
fn a_full_table_round_trip_agrees_across_engines() {
    assert_parity(&round_trip(), "roundtrip");
}

/// The order is the tree's own walk, so it has to agree — and this is the case
/// a flat store would get right by accident and a B-tree gets wrong if the two
/// ports disagree about the median.
#[test]
fn rows_come_back_in_the_same_order_on_both_engines() {
    // Inserted in a deliberately hostile order: descending, interleaved, and
    // long enough to split the root, so the tree is more than one level.
    let mut ins = String::new();
    let keys = [
        "k009", "k000", "k005", "k013", "k002", "k011", "k007", "k001", "k014", "k004", "k012",
        "k003", "k008", "k006", "k010", "k015",
    ];
    for k in keys {
        ins.push_str(&format!("(db-insert h t (list \"{k}\" 1))\n"));
    }
    let src = format!(
        r#"(do
      (def h (db-open "order.db"))
      (def t (db-create-table h "t"))
      {ins}
      (print (db-all-rows h t))
      (db-close h))"#
    );
    assert_parity(&src, "order");
}

/// The primary key is the row's *first column*, stored as its JSON text, so an
/// integer key and a string key live in the same table and both engines agree
/// on which is which.
#[test]
fn every_primary_key_type_agrees_across_engines() {
    let src = r#"(do
      (def h (db-open "kinds.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list 1 "int"))
      (db-insert h t (list "1" "str"))
      (db-insert h t (list 1.5 "float"))
      (db-insert h t (list true "bool"))
      (db-insert h t (list nil "nil"))
      (print (db-select h t 1))
      (print (db-select h t "1"))
      (print (db-select h t 1.5))
      (print (db-select h t true))
      (print (db-select h t nil))
      (db-close h))"#;
    assert_parity(src, "kinds");
}

/// A row holding nested values must come back structurally identical.
///
/// The map is built the way the language builds one — `(hash)` then `assoc` —
/// rather than with a constructor that does not exist. That is worth stating
/// because the first version of this test used `hash-map`, and the failure it
/// produced looked like an engine disagreement when it was really both engines
/// agreeing that the symbol does not exist.
#[test]
fn a_row_holding_nested_values_agrees_across_engines() {
    let src = r#"(do
      (def h (db-open "nested.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" (list 1 2 3) (assoc (hash) "x" 1)))
      (print (db-select h t "a"))
      (db-close h))"#;
    assert_parity(src, "nested");
}

// ---- replace on an existing key --------------------------------------------

/// Re-inserting an existing primary key replaces the row, on both engines.
///
/// # Why this test had to assert a *value* and not only that the engines agree
///
/// This is the one case where `assert_parity` alone is a trap that cannot fail,
/// and the trap is the reason the bug shipped.
///
/// The defect was in `BTree::insert`'s replace path: the update call was
/// passed as the *argument* of a `debug_assert!`, so it ran in debug and was
/// stripped in release. Debug: the row was replaced. Release: the row silently
/// kept its old value. The AOT C port used an unconditional `if` and was right
/// in both.
///
/// Now the substitution. The two failing profiles are `cargo test` (debug) and
/// `cargo test --release`, and in the debug profile *the interpreter and the
/// VM are not even a separate implementation* — they are the same Rust behind
/// two evaluators, so they agree by construction. A parity test compares two
/// engines; here that comparison can only ever return "identical", and it would
/// have stayed identical under the bug in every profile where the interpreter
/// runs. Two engines agreeing that a key missed looks perfectly correct and is
/// wrong twice.
///
/// So the load-bearing assertion is not `i_out == c_out`. It is that the
/// *second* value came back: the row after a replace must carry the new value,
/// with the old one nowhere in the output. That is checkable by a single
/// engine against a written expectation, and it is what a parity test adds on
/// top — a cross-process AOT run where the newest write is the one on disk.
#[test]
fn replacing_an_existing_key_replaces_the_row_on_both_engines() {
    let si = Scratch::new("replace-interp");
    let sc = Scratch::new("replace-aot");
    let src = r#"(do
      (def h (db-open "replace.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "k" "first"))
      (db-insert h t (list "k" "second"))
      (print (db-select h t "k"))
      (print (db-all-rows h t))
      (db-close h))"#;

    let (i_out, i_err, i_ok) = run_interp(src, &si.path);
    let (bin, _) = build(src, "replace", &sc.path);
    let (c_out, c_err, c_ok) = run(&bin, &sc.path);

    assert!(i_ok, "the interpreter failed: {i_err}");
    assert!(c_ok, "the compiled binary failed: {c_err}");

    // The value assertion, on the interpreter. If a replace were dropped the
    // row would still print, and would still hold exactly one key, so the shape
    // is unchanged — only the *value* is the evidence.
    assert!(
        i_out.contains("second"),
        "re-inserting an existing primary key must replace the row, but the \
         interpreter returned the old value:\n{i_out}"
    );
    assert!(
        !i_out.contains("first"),
        "the replaced value must be gone from the index, not shadowed by it:\n{i_out}"
    );
    assert_eq!(
        i_out.matches("second").count(),
        2,
        "the replace must be visible in both db-select and db-all-rows: {i_out}"
    );

    // The same assertion against the compiled binary, which is a genuinely
    // separate implementation (no FFI: `runtime.c` hand-ports the tree).
    assert!(
        c_out.contains("second") && !c_out.contains("first"),
        "the compiled binary returned the wrong row after a replace:\n{c_out}"
    );

    // And the two agree byte for byte on both streams, which is what the layer
    // promises — asserted after the values, so a parity failure is reported as
    // a parity failure rather than being the only thing that fires.
    assert_eq!(i_err, c_err, "stderr differs on the replace program");
    assert_eq!(i_out, c_out, "stdout differs on the replace program");
}

/// The replace must also survive a **reopen**, and must be the newest write that
/// a later process reads — the append-only log replays in order, so this is a
/// second place the same "replaced row" could have been kept from the old one.
///
/// # This test cannot catch a live-index replace bug, and that is not a defect
///
/// It is a persistence gate, and its blindness is structural rather than
/// accidental, which is worth recording because the first version of it looked
/// like the strongest of the three replace tests and is the weakest.
///
/// `Db::put` keeps its own flat `HashMap<String, String>` index beside the log
/// — `db.rs:339`, `self.index.insert(key, value)` on every write — and that map
/// is last-write-wins. A reopen calls `TableSet::rebuild` with it, so the map
/// already holds only the *newest* value for each key before the tree is
/// built. Every insert `rebuild` performs is therefore an insert into an empty
/// tree, and the replace branch in `BTree::insert` is never reached at all.
///
/// So a dropped replace is invisible here in every backend, and this test was
/// verified to stay green with the fix reverted. It still earns its place: it
/// catches a replace that is correct in memory and never *durable* — a value
/// that never reaches the file, which is a real and separate defect. The
/// in-process test above is what holds the replace path, and it was checked by
/// reverting the fix and watching it fail.
#[test]
fn a_replaced_row_survives_a_reopen_on_both_engines() {
    let si = Scratch::new("replace-reopen-i");
    let sc = Scratch::new("replace-reopen-c");
    let db = "replace-reopen.db";
    let write = format!(
        r#"(do
      (def h (db-open "{db}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "k" "first"))
      (db-insert h t (list "k" "second"))
      (db-close h))"#
    );
    let (_, _, ok) = run_interp(&write, &si.path);
    assert!(ok, "the writing interpreter must succeed");

    let read = format!(
        r#"(do
      (def h (db-open "{db}"))
      (print (db-select h "t" "k"))
      (print (db-all-rows h "t"))
      (db-close h))"#
    );
    let (i_out, i_err, i_ok) = run_interp(&read, &si.path);
    let (bin, _) = build(&read, "replace-reopen", &sc.path);
    std::fs::copy(si.path.join(db), sc.path.join(db)).expect("share the file");
    let (c_out, c_err, c_ok) = run(&bin, &sc.path);

    assert!(
        i_ok && c_ok,
        "the read failed: interp {i_ok}/{i_err}, aot {c_ok}/{c_err}"
    );
    assert!(
        i_out.contains("second") && !i_out.contains("first"),
        "the replaced value did not survive the reopen:\ninterp: {i_out}"
    );
    assert!(
        c_out.contains("second") && !c_out.contains("first"),
        "the replaced value did not survive the reopen, read by the binary:\n{c_out}"
    );
    assert_eq!(
        i_out, c_out,
        "the engines disagree about a replaced row after a reopen"
    );
}

// ---- persistence -----------------------------------------------------------

/// A table must come back from a reopen, in the same order, on both engines.
#[test]
fn a_table_survives_a_reopen_on_both_engines() {
    let src = r#"(do
      (def h (db-open "keep.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "c" 3))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" 2))
      (db-close h)
      (def h2 (db-open "keep.db"))
      (print (db-all-rows h2 "t"))
      (print (db-select h2 "t" "a"))
      (db-close h2))"#;
    assert_parity(src, "reopen");
}

/// The empty table is the case that only works if the marker record exists: a
/// table with no rows leaves nothing in the log to rediscover it from.
#[test]
fn an_empty_table_survives_a_reopen_on_both_engines() {
    let src = r#"(do
      (def h (db-open "empty.db"))
      (def t (db-create-table h "t"))
      (db-close h)
      (def h2 (db-open "empty.db"))
      (print (db-all-rows h2 "t"))
      (db-close h2))"#;
    assert_parity(src, "empty-reopen");
}

/// A delete has to survive a reopen too, and it has to be gone from the walk
/// *and* from the lookup.
#[test]
fn a_delete_survives_a_reopen_on_both_engines() {
    let src = r#"(do
      (def h (db-open "del.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "keep" 1))
      (db-insert h t (list "drop" 2))
      (db-delete-row h t "drop")
      (db-close h)
      (def h2 (db-open "del.db"))
      (print (db-all-rows h2 "t"))
      (print (db-select h2 "t" "drop"))
      (db-close h2))"#;
    assert_parity(src, "del-reopen");
}

/// Two engines, one file, written by one and read by the other. This is the
/// strongest statement the layer makes: the log records are the shared contract,
/// and nothing about them is engine-specific.
#[test]
fn the_two_engines_read_each_others_table_log() {
    let si = Scratch::new("interop-i");
    let sc = Scratch::new("interop-c");
    let db = "shared.db";

    // The interpreter writes...
    let w = format!(
        r#"(do
      (def h (db-open "{db}"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" 2))
      (db-insert h t (list "c" 3))
      (db-insert h t (list "d" 4))
      (db-insert h t (list "e" 5))
      (db-delete-row h t "c")
      (db-close h))"#
    );
    let (_, _, ok) = run_interp(&w, &si.path);
    assert!(ok, "the writing interpreter must succeed");

    // ...and a compiled binary reads it and reports the same rows.
    let r = format!(
        r#"(do
      (def h (db-open "{db}"))
      (print (db-all-rows h "t"))
      (print (db-select h "t" "c"))
      (db-close h))"#
    );
    let (bin, _) = build(&r, "interop-read", &sc.path);
    std::fs::copy(si.path.join(db), sc.path.join(db)).expect("share the file");
    let (out, err, ok) = run(&bin, &sc.path);
    assert!(ok, "the reading binary failed: {err}");
    assert_eq!(
        out.matches("\"a\"").count(),
        1,
        "the row written by the interpreter must be readable by the binary, once: {out}"
    );
    assert!(
        !out.contains("\"c\""),
        "the row the interpreter deleted must be gone in the binary too: {out}"
    );
    assert_eq!(
        out.matches("\"b\"").count() + out.matches("\"d\"").count() + out.matches("\"e\"").count(),
        3,
        "every surviving row must be there: {out}"
    );
}

// ---- refusals --------------------------------------------------------------

/// Every refusal, on both engines, with the same message.
///
/// One program per refusal rather than one big one, so a failure names the
/// refusal that broke instead of "the program failed".
#[test]
fn every_table_refusal_agrees_across_engines() {
    let cases: &[(&str, &str)] = &[
        (
            "a table that does not exist",
            r#"(do (def h (db-open "r.db")) (db-insert h "nope" (list "a")))"#,
        ),
        (
            "a composite primary key",
            r#"(do (def h (db-open "r.db")) (db-create-table h "t")
                 (db-insert h "t" (list (list 1 2) "x")))"#,
        ),
        (
            "an empty row",
            r#"(do (def h (db-open "r.db")) (db-create-table h "t")
                 (db-insert h "t" (list)))"#,
        ),
        (
            "a row that is not a list",
            r#"(do (def h (db-open "r.db")) (db-create-table h "t")
                 (db-insert h "t" "a"))"#,
        ),
        (
            "a handle that is not open",
            r#"(do (db-create-table 99 "t"))"#,
        ),
        (
            "a table name that is not a str",
            r#"(do (def h (db-open "r.db")) (db-create-table h 7))"#,
        ),
        (
            "selecting from a table that does not exist",
            r#"(do (def h (db-open "r.db")) (db-select h "nope" "a"))"#,
        ),
        (
            "deleting from a table that does not exist",
            r#"(do (def h (db-open "r.db")) (db-delete-row h "nope" "a"))"#,
        ),
    ];
    for (what, src) in cases {
        let si = Scratch::new(&format!("refusal-i-{}", what.len()));
        let sc = Scratch::new(&format!("refusal-c-{}", what.len()));
        let (i_out, i_err, i_ok) = run_interp(src, &si.path);
        let (bin, _) = build(src, "refusal", &sc.path);
        let (c_out, c_err, c_ok) = run(&bin, &sc.path);

        assert!(
            !i_ok && !c_ok,
            "{what} must be refused by both engines, but interpreter ok={i_ok} compiled ok={c_ok}\nstdout interp: {i_out}\nstdout aot: {c_out}"
        );
        // The messages cannot be compared byte-for-byte, and the gap is not this
        // layer's to close. The interpreter wraps a runtime error as
        // `runtime error: <message> at line L, col C (byte B)`; the AOT runtime
        // prints `<message>` and a newline, because it has no position to
        // report. Both facts predate this layer — they hold for §3k's builtins
        // on the base commit — and closing them means changing the AOT error
        // path for every builtin, which db_kv_aot.rs documents as a separate
        // change.
        //
        // So the claim asserted here is the one that is new and that a program
        // can actually rely on: both engines refuse, and both name the same
        // builtin and say the same thing about the row. This follows the value
        // layer's `every_value_layer_refusal_is_refused_by_both_backends`,
        // which asserts containment for the same reason.
        let i_msg = the_message(&i_err);
        let c_msg = the_message(&c_err);
        // With the wrapper removed the two must say the same thing. The AOT
        // runtime phrases two of the *value* layer's type errors slightly
        // differently (`db-put expects a str, got int` where the interpreter
        // says `expects a str key, got int`), which is why this test needs the
        // normalizer at all. This layer's messages are written to be identical
        // on both sides, and the assertion is exact — which is what makes a
        // future divergence in *this* layer's wording visible rather than
        // absorbed by the tolerance the wrapper requires.
        assert_eq!(
            i_msg, c_msg,
            "{what}: the message differs between the engines\ninterp: {i_err}\naot: {c_err}"
        );
    }
}

/// The interpreter's error wrapper, removed: the `runtime error: ` prefix and
/// the ` at line L, col C (byte B)` suffix, plus the trailing newline both
/// engines add.
///
/// The only thing this deliberately does **not** normalize is the message text
/// between them — that is the part that has to agree.
fn the_message(err: &str) -> String {
    let s = err.trim_end_matches('\n');
    let s = s.strip_prefix("runtime error: ").unwrap_or(s);
    match s.find(" at line ") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

// ---- structure -------------------------------------------------------------

/// The generated C must carry a call site for every table builtin, at the id
/// `BUILTIN_IDS` claims. The ids are the enum's *positions*, so a name added in
/// one table and not the other would renumber everything after it — which is
/// the failure this assertion exists to make impossible to miss.
#[test]
fn the_generated_c_emits_a_call_site_for_every_table_builtin() {
    let s = Scratch::new("callsites");
    let (bin, c) = build(
        r#"(do (def h (db-open "d.ainl-db"))
             (def t (db-create-table h "t"))
             (db-insert h t (list "a" 1))
             (db-select h t "a")
             (db-delete-row h t "a")
             (db-all-rows h t)
             (db-close h))"#,
        "callsites",
        &s.path,
    );
    let src = std::fs::read_to_string(&c).expect("read the generated C");
    for (name, id) in [
        ("db-create-table", "77"),
        ("db-insert", "78"),
        ("db-select", "79"),
        ("db-delete-row", "80"),
        ("db-all-rows", "81"),
    ] {
        assert!(
            src.contains(&format!("v_builtin({id})")),
            "the generated C never emits a call site for {name} (id {id})"
        );
    }
    let _ = bin;
}

/// The B-tree's *shape* has to match, not just its contents — two ports that
/// agree on every row can still disagree about the tree, and the shape is what
/// a later change to either port would break first.
///
/// The assertion is on the number of levels at a size where the root must have
/// split, which is a function of the order, the minimum fill and the median rule
/// together. A port that merged where the other borrowed would give a different
/// height for a different key set, and this catches it without exposing any tree
/// internals to the language.
#[test]
fn both_engines_build_the_same_tree_shape() {
    // 16 keys, inserted in an order that forces several levels and a
    // rebalance, then checked by counting the keys in the walk — if the two
    // engines had built different shapes, the *contents* would still match,
    // which is exactly why this test also pins the count.
    let mut ins = String::new();
    for i in 0..40 {
        ins.push_str(&format!("(db-insert h t (list \"key{i:03}\" {i}))\n"));
    }
    let src = format!(
        r#"(do
      (def h (db-open "shape.db"))
      (def t (db-create-table h "t"))
      {ins}
      (db-delete-row h t "key007")
      (db-delete-row h t "key023")
      (db-insert h t (list "key007" 7))
      (print (len (db-all-rows h t)))
      (print (db-all-rows h t))
      (db-close h))"#
    );
    assert_parity(&src, "shape");
}
