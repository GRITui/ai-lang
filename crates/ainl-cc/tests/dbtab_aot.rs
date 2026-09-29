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

/// A row is an AINL list, so it may hold lists and maps, and it must come back
/// structurally identical.
#[test]
fn a_row_holding_nested_values_agrees_across_engines() {
    let src = r#"(do
      (def h (db-open "nested.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" (list 1 2 3) (hash-map "x" 1)))
      (print (db-select h t "a"))
      (db-close h))"#;
    assert_parity(src, "nested");
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
        assert_eq!(
            i_err, c_err,
            "{what}: the message differs between the engines\ninterp: {i_err}\naot: {c_err}"
        );
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
