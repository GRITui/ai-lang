//! Tier 4 query layer: the same SQL subset in the interpreter and in a compiled
//! binary, byte for byte.
//!
//! # Why this file is separate from `dbtab_aot.rs`
//!
//! Because the claim is different. That file checks that a *B-tree* agrees on
//! which rows exist and in what order. This one checks that a **query parser**
//! agrees — on the rows it selects, the order it puts them in, and the exact
//! text of the error it prints when it refuses. That third part is the one worth
//! a file of its own, because it is the part that can drift without anybody
//! noticing: two parsers that agree on every valid query can still disagree on
//! every invalid one, and the only symptom is a stderr that depends on which
//! backend compiled the program.
//!
//! # Why parity is checked on refusals at all
//!
//! Because a refusal is the message a model reads. `SELECT * FROM people GROUP
//! BY 1` has to be refused on both engines with the same words, the same
//! position, and the same suggestion — otherwise the same program teaches two
//! different things depending on how it was built. The subset sentence and the
//! "did you mean" are part of that contract, not decoration, so they are
//! compared as bytes and not as substrings.
//!
//! # The one documented difference
//!
//! The interpreter appends ` at line N, col M (byte B)` naming the *call site*
//! in the AINL source; the compiled binary has no source and so has no position
//! to give. That suffix is the backend difference `docs/SYNTAX.md` §5a already
//! documents, and it is why `assert_parity` compares the message with that
//! suffix stripped rather than skipping the error case.

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
            "ainl-dbqaot-{}-{tag}-{:?}",
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

fn compile(src: &str, tag: &str, dir: &Path) -> PathBuf {
    let ainl_path = dir.join("p.ainl");
    let bin_path = dir.join(format!("{tag}.bin"));
    std::fs::write(&ainl_path, src).expect("write the program");
    let out = Command::new(ainl())
        .arg("compile")
        .arg(&ainl_path)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .expect("run ainl compile");
    assert!(
        out.status.success(),
        "ainl compile refused the {tag} program: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin_path
}

fn interpret(src: &str, dir: &Path) -> (String, String, bool) {
    let p = dir.join("p.ainl");
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

fn run_bin(bin: &Path, dir: &Path) -> (String, String, bool) {
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

/// Strip the interpreter's call-site suffix, leaving the message the two
/// backends must agree on.
///
/// The suffix is ` at line N, col M (byte B)` and is appended *after* the
/// message, so it is the tail from the last ` at line ` onward. Stripping from
/// the **last** occurrence rather than the first matters: a query text can
/// itself contain ` at line ` (it is echoed back in the message), and cutting at
/// the first one would compare two truncated prefixes and call it agreement.
fn without_call_site(err: &str) -> String {
    let head = err
        .strip_prefix("runtime error: ")
        .unwrap_or(err)
        .trim_end()
        .to_string();
    match head.rfind(" at line ") {
        Some(at) => {
            // Only cut if what follows really is the position shape — a message
            // that merely happens to contain the words is left whole.
            let tail = &head[at..];
            let looks_like_a_position = tail
                .rsplit_once("(byte ")
                .map(|(_, rest)| rest.ends_with(')'))
                .unwrap_or(false);
            if looks_like_a_position {
                head[..at].to_string()
            } else {
                head
            }
        }
        None => head,
    }
}

/// The three-row fixture every test here uses.
///
/// Ada/bob/grace is chosen because it makes the interesting orderings reachable
/// in three rows: ages 36/41/45 sort differently from primary keys
/// ada/bob/grace, and two of the three rows share the department "navy" so a
/// stable sort on a duplicated value is observable without a fourth row.
fn fixture(query: &str) -> String {
    format!(
        r#"(do
      (def h (db-open "people.db"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "bob" 41 "navy"))
      (db-insert h t (list "grace" 45 "navy"))
      (print (db-query h "{query}"))
      (db-close h))"#
    )
}

/// A `db-query-count` program over the same fixture.
fn fixture_count(query: &str) -> String {
    format!(
        r#"(do
      (def h (db-open "people.db"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "bob" 41 "navy"))
      (db-insert h t (list "grace" 45 "navy"))
      (print (db-query-count h "{query}"))
      (db-close h))"#
    )
}

/// The parity assertion: same program, both engines, same stdout, same exit
/// code, and — for a refusal — the same message once the interpreter's
/// call-site suffix is removed.
///
/// Separate directories on purpose: a shared one would let the interpreter's
/// database answer the binary's read, and the test would pass for the wrong
/// reason.
fn assert_parity(src: &str, tag: &str) {
    let si = Scratch::new(&format!("{tag}-interp"));
    let sc = Scratch::new(&format!("{tag}-aot"));
    let (i_out, i_err, i_ok) = interpret(src, &si.path);
    let bin = compile(src, tag, &sc.path);
    let (c_out, c_err, c_ok) = run_bin(&bin, &sc.path);

    assert_eq!(
        (i_ok, c_ok),
        (true, true),
        "the {tag} program: interpreter ok={i_ok} compiled ok={c_ok}\n\
         stderr interp: {i_err}\nstderr aot: {c_err}"
    );
    assert_eq!(
        i_out, c_out,
        "the {tag} program: stdout differs between the interpreter and the compiled binary"
    );
    assert_eq!(
        without_call_site(&i_err),
        c_err.trim_end(),
        "the {tag} program: the error message differs between the engines"
    );
}

/// The refusal assertion: both engines must fail, and say the same thing.
fn assert_refusal_parity(src: &str, tag: &str, must_contain: &[&str]) {
    let si = Scratch::new(&format!("{tag}-interp"));
    let sc = Scratch::new(&format!("{tag}-aot"));
    let (i_out, i_err, i_ok) = interpret(src, &si.path);
    let bin = compile(src, tag, &sc.path);
    let (c_out, c_err, c_ok) = run_bin(&bin, &sc.path);

    assert!(
        !i_ok && !c_ok,
        "the {tag} program was expected to fail on both engines: interp ok={i_ok} \
         compiled ok={c_ok}\nstdout interp: {i_out}\nstdout aot: {c_out}"
    );
    assert_eq!(
        i_out, c_out,
        "the {tag} program: a refusal printed to stdout on one engine and not the other"
    );
    let shared = without_call_site(&i_err);
    assert_eq!(
        shared,
        c_err.trim_end(),
        "the {tag} program: the refusal differs between the engines"
    );
    for want in must_contain {
        assert!(
            shared.contains(want),
            "the {tag} program: the refusal does not say {want:?}\ngot: {shared}"
        );
    }
}

// ---- the accepted queries -------------------------------------------------

#[test]
fn the_whole_subset_agrees_across_engines() {
    // One program that touches every clause, so the test says which part broke
    // rather than just "a query differed".
    let src = r#"(do
      (def h (db-open "people.db"))
      (def t (db-create-table h "people"))
      (db-insert h t (list "ada" 36 "math"))
      (db-insert h t (list "bob" 41 "navy"))
      (db-insert h t (list "grace" 45 "navy"))

      (print "star:      " (db-query h "SELECT * FROM people"))
      (print "project:   " (db-query h "SELECT 1, 2 FROM people"))
      (print "where:     " (db-query h "SELECT 1 FROM people WHERE 3 = 'navy'"))
      (print "and:       " (db-query h "SELECT 1 FROM people WHERE 2 > 40 AND 3 = 'navy'"))
      (print "or:        " (db-query h "SELECT 1 FROM people WHERE 1 = 'ada' OR 2 = 45"))
      (print "order asc: " (db-query h "SELECT 1 FROM people ORDER BY 2"))
      (print "order des: " (db-query h "SELECT 1 FROM people ORDER BY 2 DESC"))
      (print "order exp: " (db-query h "SELECT 1, 2 FROM people ORDER BY 2 LIMIT 2"))
      (print "count:     " (db-query-count h "SELECT 1 FROM people"))
      (print "count w:   " (db-query-count h "SELECT 1 FROM people WHERE 2 > 40"))
      (print "indexed:   " (db-query h "SELECT 2 FROM people WHERE 1 = 'grace'"))
      (print "ties:      " (db-query h "SELECT 1 FROM people ORDER BY 3"))
      (print "case:      " (db-query h "select 1 from people where 3 = 'NAVY' order by 2 desc"))
      (print "neg:       " (db-query h "SELECT 1 FROM people WHERE 2 > -1"))
      (print "float:     " (db-query h "SELECT 1 FROM people WHERE 2 < 41.5"))
      (print "bool:      " (db-query h "SELECT 1 FROM people WHERE 4 = true"))
      (print "limit 0:   " (db-query h "SELECT 1 FROM people LIMIT 0"))
      (print "limit big: " (db-query-count h "SELECT 1 FROM people LIMIT 99"))
      (print "nil row:   " (db-query h "SELECT 1, 2 FROM people WHERE 3 != 'math'"))
      (print "comma:     " (db-query h "SELECT 3, 1 FROM people WHERE 1 = 'bob'"))
      (db-close h))"#;
    assert_parity(src, "subset");
}

/// ORDER BY runs on the **full** row, so it can name a column the projection
/// does not have. A C port that projected first would sort on a nil and answer
/// with the wrong order while every other test still passed.
#[test]
fn ordering_by_an_unprojected_column_agrees() {
    assert_parity(&fixture("SELECT 1 FROM people ORDER BY 2"), "unprojected");
}

/// DESC reverses the comparison, not the input. Two rows share "navy", so a
/// sort that reversed ties would put grace before bob — the opposite order from
/// the ASC result on the same column.
#[test]
fn a_descending_sort_keeps_ties_in_key_order() {
    assert_parity(
        &fixture("SELECT 1, 2 FROM people ORDER BY 3 DESC"),
        "stability",
    );
}

/// The point-lookup path: `WHERE 1 = <scalar>` is the one shape that reads a
/// single key rather than walking, and both engines must choose it and agree on
/// what it finds. A hit, a miss, and a key that is not a string.
#[test]
fn the_indexed_path_agrees_on_hits_and_misses() {
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list 7 "seven"))
      (db-insert h t (list "k" "by-name"))
      (db-insert h t (list 3.5 "fractional"))
      (print "int:  " (db-query h "SELECT 2 FROM t WHERE 1 = 7"))
      (print "miss: " (db-query h "SELECT 2 FROM t WHERE 1 = 99"))
      (print "str:  " (db-query h "SELECT 2 FROM t WHERE 1 = 'k'"))
      (print "flt:  " (db-query h "SELECT 2 FROM t WHERE 1 = 3.5"))
      (print "nope: " (db-query h "SELECT 2 FROM t WHERE 1 = nil"))
      (print "cnt:  " (db-query-count h "SELECT 1 FROM t WHERE 1 = 7"))
      (db-close h))"#;
    assert_parity(src, "indexed");
}

/// Rows of different lengths, and a projection past the end of the short ones.
/// The convention is nil, and both engines have to agree on where it applies.
#[test]
fn ragged_rows_and_past_the_end_project_to_nil() {
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a"))
      (db-insert h t (list "b" 2))
      (db-insert h t (list "c" 3 "three"))
      (print "star: " (db-query h "SELECT * FROM t"))
      (print "two:  " (db-query h "SELECT 1, 2 FROM t"))
      (print "past: " (db-query h "SELECT 5 FROM t"))
      (print "cnt:  " (db-query-count h "SELECT 1 FROM t WHERE 2 = 2"))
      (db-close h))"#;
    assert_parity(src, "ragged");
}

/// A multi-byte string, in a value and in a table name. The column counter is in
/// characters, so a refusal that follows one has to count the same way on both
/// engines.
#[test]
fn multi_byte_text_agrees() {
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "日本"))
      (db-insert h t (list "日本" "語"))
      (print "hit:  " (db-query h "SELECT 2 FROM 日本 WHERE 1 = '日本'"))
      (print "name: " (db-query h "SELECT 1 FROM 日本"))
      (db-close h))"#;
    assert_parity(src, "unicode");
}

// ---- the refusals ---------------------------------------------------------

/// Every construct v1 knows about and does not support, in one program.
///
/// The point is not that each is refused — the Rust unit tests cover the
/// messages — but that **both** engines refuse each one. A keyword in only one
/// engine's table is a query that one backend answers and the other rejects,
/// which is the worst outcome the 4-backend rule exists to prevent.
#[test]
fn every_out_of_scope_keyword_is_refused_by_both_engines() {
    let cases: &[(&str, &str)] = &[
        (
            "SELECT * FROM people GROUP BY 1",
            "'GROUP' is not supported in v1",
        ),
        (
            "SELECT * FROM people HAVING 1 = 1",
            "'HAVING' is not supported in v1",
        ),
        (
            "SELECT * FROM people JOIN people p ON 1 = 1",
            "'JOIN' is not supported",
        ),
        (
            "SELECT * FROM people LEFT JOIN people p ON 1 = 1",
            "'LEFT' is not supported",
        ),
        (
            "SELECT * FROM people INNER JOIN people p ON 1 = 1",
            "'INNER' is not supported",
        ),
        (
            "SELECT DISTINCT 1 FROM people",
            "'DISTINCT' is not supported",
        ),
        (
            "SELECT 1 FROM people UNION SELECT 1 FROM people",
            "'UNION' is not supported",
        ),
        (
            "SELECT 1 FROM people WHERE 1 IN (1)",
            "'IN' is not supported",
        ),
        (
            "SELECT 1 FROM people WHERE 1 LIKE 'a'",
            "'LIKE' is not supported",
        ),
        (
            "SELECT 1 FROM people WHERE 2 BETWEEN 1 AND 3",
            "'BETWEEN' is not supported",
        ),
        (
            "SELECT 1 FROM people WHERE 2 IS NULL",
            "'IS' is not supported",
        ),
        (
            "SELECT 1 FROM people WHERE NOT 1 = 1",
            "'NOT' is not supported",
        ),
        ("SELECT 1 FROM people OFFSET 1", "'OFFSET' is not supported"),
        (
            "SELECT 1 FROM people ORDER BY 1 NULLS FIRST",
            "'NULLS' is not supported",
        ),
        (
            "SELECT 1 FROM people ORDER BY 1 LIMIT 1 OFFSET 1",
            "'OFFSET' is not supported",
        ),
        ("SELECT SUM(1) FROM people", "SUM is not supported in v1"),
        ("SELECT AVG(1) FROM people", "AVG is not supported in v1"),
        ("SELECT MIN(1) FROM people", "MIN is not supported in v1"),
        ("SELECT MAX(1) FROM people", "MAX is not supported in v1"),
        (
            "SELECT * FROM (SELECT 1 FROM people)",
            "a subquery or a parenthesised table",
        ),
        (
            "SELECT 1 FROM people WHERE 1 = (SELECT 1)",
            "a subquery as a value",
        ),
    ];
    for (i, (query, want)) in cases.iter().enumerate() {
        assert_refusal_parity(&fixture(query), &format!("kw{i}"), &[want]);
    }
}

/// COUNT is refused by name **and** with the builtin that does the job, because
/// refusing the commonest aggregate without saying what to use instead is a
/// dead end for the reader.
#[test]
fn count_names_the_builtin_that_does_the_job() {
    assert_refusal_parity(
        &fixture("SELECT COUNT(1) FROM people"),
        "count",
        &["COUNT is not supported in v1", "db-query-count"],
    );
}

/// A near miss gets a suggestion; a word that matches nothing legal does not.
/// Offering a fix the reader cannot type is worse than offering none.
#[test]
fn suggestions_are_offered_only_where_they_can_be_typed() {
    // FORM for FROM: a transposition, which plain edit distance scores as two
    // changes and so misses at the one-edit cap. Both engines must still see it.
    assert_refusal_parity(
        &fixture("SELECT 1 FORM people"),
        "transpose",
        &["did you mean 'FROM'?"],
    );
    // WHER is a near miss of an out-of-scope keyword, and the message has to say
    // so rather than imply WHERE would have worked.
    assert_refusal_parity(
        &fixture("SELECT * FROM people WHER 1 = 1"),
        "typo-outofscope",
        &["'WHER' is not supported in v1"],
    );
    // A word nobody recognises gets a plain refusal and no suggestion.
    assert_refusal_parity(
        &fixture("SELECT * FROM people WHERE 1 = zzz"),
        "typo-value",
        &["is not a value"],
    );
}

/// The clause order is enforced, not documented. `LIMIT 1 ORDER BY 2` is the
/// case that matters: accepted-and-reordered is the failure this whole layer
/// exists to prevent, and a C port with a permissive clause loop would accept
/// it silently while the Rust side refused.
#[test]
fn a_clause_in_the_wrong_order_is_refused_not_reordered() {
    for (i, (query, want)) in [
        (
            "SELECT * FROM people LIMIT 1 ORDER BY 2",
            "ORDER BY comes before LIMIT",
        ),
        (
            "SELECT * FROM people ORDER BY 2 WHERE 1 = 1",
            "WHERE comes before ORDER BY",
        ),
        (
            "SELECT * FROM people WHERE 1 = 1 WHERE 2 = 2",
            "only one WHERE",
        ),
        ("SELECT * FROM people LIMIT 1 LIMIT 2", "only one LIMIT"),
        (
            "SELECT * FROM people ORDER BY 1 ORDER BY 2",
            "only one ORDER BY",
        ),
    ]
    .iter()
    .enumerate()
    {
        assert_refusal_parity(&fixture(query), &format!("order{i}"), &[want]);
    }
}

/// A query that stops early is told which word is missing, rather than being
/// reported as a bad token at the end — the end of the query is not where the
/// reader made the mistake.
#[test]
fn a_truncated_query_names_the_missing_word() {
    for (i, (query, want)) in [
        ("", "the query ended, but SELECT is required"),
        ("SELECT", "the query ended, but FROM is required"),
        ("SELECT 1", "the query ended, but FROM is required"),
        ("SELECT 1 FROM", "expected a table name after FROM"),
        ("SELECT 1 FROM people WHERE", "expected a column number"),
        (
            "SELECT 1 FROM people WHERE 1 = 1 AND",
            "expected a column number",
        ),
    ]
    .iter()
    .enumerate()
    {
        assert_refusal_parity(&fixture(query), &format!("trunc{i}"), &[want]);
    }
}

/// An ORDER BY column no row has is a named error, not a sort that quietly
/// returns primary-key order. The one-row case is included separately because a
/// merge of a single element never calls a comparator, so the shape check has
/// to be a pass of its own.
#[test]
fn an_order_by_column_no_row_has_is_refused() {
    assert_refusal_parity(
        &fixture("SELECT 1 FROM people ORDER BY 9"),
        "order-nil",
        &["ORDER BY column 9 is nil in every row"],
    );
    assert_refusal_parity(
        &fixture("SELECT 1 FROM people WHERE 1 = 'ada' ORDER BY 9"),
        "order-nil-one",
        &["ORDER BY column 9 is nil in every row"],
    );
    // The same column in a *projection* is nil, not an error. The asymmetry is
    // deliberate and both engines have to keep it.
    assert_parity(&fixture("SELECT 9 FROM people"), "project-nil");
}

/// An ordered comparison against a value that cannot be ordered is an error
/// naming both types. `=` and `!=` on the same column are a plain answer,
/// because that is what the language's own `=` already does.
#[test]
fn an_unorderable_comparison_names_both_types() {
    assert_refusal_parity(
        &fixture("SELECT 1 FROM people WHERE 9 > 1"),
        "where-nil",
        &["WHERE column 9 is a nil", "a int"],
    );
    assert_parity(
        &fixture("SELECT 1 FROM people WHERE 9 = nil"),
        "where-nil-eq",
    );
    assert_parity(
        &fixture("SELECT 1 FROM people WHERE 9 != nil"),
        "where-nil-ne",
    );
}

/// A column that holds different types in different rows is unorderable, and
/// only the comparator can see it — no per-row shape check can.
#[test]
fn an_order_by_over_mixed_types_is_refused() {
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" "two"))
      (print (db-query h "SELECT 1 FROM t ORDER BY 2"))
      (db-close h))"#;
    assert_refusal_parity(
        src,
        "order-mixed",
        &["ORDER BY column 2", "cannot be ordered"],
    );
    // `=` on the same column is fine — a false answer, not a type error.
    let ok = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" "two"))
      (print (db-query h "SELECT 1 FROM t WHERE 2 = 1"))
      (db-close h))"#;
    assert_parity(ok, "order-mixed-eq");
}

/// A table that is not there, and a message that says which one was asked for.
#[test]
fn a_missing_table_names_itself() {
    assert_refusal_parity(
        &fixture("SELECT 1 FROM nope"),
        "no-table",
        &["no table named 'nope' in this database"],
    );
}

/// The lexer refuses what it cannot read rather than guessing: a stray quote, a
/// number glued to a word, and a character with no place in the grammar.
#[test]
fn the_lexer_refuses_what_it_cannot_read() {
    for (i, (query, want)) in [
        (
            "SELECT 1 FROM people WHERE 1 = 'unclosed",
            "was never closed",
        ),
        (
            "SELECT 1 FROM people WHERE 1 = 2 1abc",
            "a number cannot be followed by",
        ),
        (
            "SELECT 1 FROM people WHERE 1 = 2 $",
            "is not part of the query language",
        ),
    ]
    .iter()
    .enumerate()
    {
        assert_refusal_parity(&fixture(query), &format!("lex{i}"), &[want]);
    }
}

/// A name where a number belongs is told why a name cannot be a column, because
/// "expected a column number, got 'name'" invites a model to look for a syntax
/// problem where the real answer is that rows have no column names at all.
#[test]
fn a_column_name_is_explained_not_just_rejected() {
    assert_refusal_parity(
        &fixture("SELECT 1 FROM people WHERE name = 1"),
        "colname",
        &["is not a column", "columns are numbered from 1"],
    );
    assert_refusal_parity(
        &fixture("SELECT 0 FROM people"),
        "col0",
        &["column 0 does not exist", "numbered from 1"],
    );
}

/// `db-query-count` refuses exactly what `db-query` does. A count that accepted
/// more than the query would let a program verify with one call and then run the
/// looser form in production.
#[test]
fn the_count_builtin_refuses_what_the_query_builtin_refuses() {
    let si = Scratch::new("cnt-interp");
    let sc = Scratch::new("cnt-aot");
    let src = &fixture_count("SELECT * FROM people GROUP BY 1");
    let (i_out, i_err, i_ok) = interpret(src, &si.path);
    let bin = compile(src, "cnt", &sc.path);
    let (c_out, c_err, c_ok) = run_bin(&bin, &sc.path);
    assert!(
        !i_ok && !c_ok,
        "db-query-count accepted a query db-query refuses: interp ok={i_ok} compiled ok={c_ok}"
    );
    assert_eq!(i_out, c_out);
    let shared = without_call_site(&i_err);
    assert_eq!(shared, c_err.trim_end());
    assert!(
        shared.contains("db-query-count") && shared.contains("'GROUP' is not supported"),
        "the refusal does not name db-query-count: {shared}"
    );
}

/// Both builtins are read-only. A query that could write would be a second
/// mutation path over the same log, and the table layer's invariants — one
/// encoding, one tombstone, one key format — are only true if there is one
/// writer. There is no way to express a write in the grammar, and this asserts
/// that the refusal for the words that would try is a *refusal*.
#[test]
fn the_query_layer_cannot_write() {
    for (i, query) in [
        "INSERT INTO people VALUES (1)",
        "DELETE FROM people",
        "UPDATE people SET 2 = 3",
        "DROP TABLE people",
        "CREATE TABLE x (1)",
    ]
    .iter()
    .enumerate()
    {
        assert_refusal_parity(
            &fixture(query),
            &format!("write{i}"),
            &["not supported in v1"],
        );
    }
}

/// A query is a string, so a query built at runtime behaves like one written out
/// — including being refused. The parser is not given a shortcut for a query it
/// could not have lexed.
#[test]
fn a_query_built_at_runtime_is_parsed_the_same_way() {
    // `(join list)` and not `+`: `+` is arithmetic and refuses a str, so a
    // program that assembles a query has to do it with the string builtins. The
    // point is that the text reaches the parser assembled rather than literal,
    // and a parser that only worked on literals would be a parser that trusted
    // its caller.
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (db-insert h t (list "b" 2))
      (print (db-query h (join (list "SELECT 1 FROM t" " WHERE 2 = 1") "")))
      (print (db-query h (join (list "SELECT 1, 2 FROM t" " ORDER BY 2 DESC") "")))
      (print (db-query-count h (join (list "SELECT 1 FROM t" " WHERE 2 = 1") "")))
      (db-close h))"#;
    assert_parity(src, "runtime-query");
}

/// The same, for a query that is assembled *wrong*. A query built at runtime has
/// to meet the same refusals a literal one does — otherwise the grammar is only
/// enforced for the queries a human typed, which is exactly the set that is
/// already right.
#[test]
fn a_runtime_query_is_refused_like_a_literal_one() {
    let src = r#"(do
      (def h (db-open "t.db"))
      (def t (db-create-table h "t"))
      (db-insert h t (list "a" 1))
      (print (db-query h (join (list "SELECT 1 FROM t" " GROUP BY 1") "")))
      (db-close h))"#;
    assert_refusal_parity(src, "runtime-refusal", &["'GROUP' is not supported in v1"]);
}
