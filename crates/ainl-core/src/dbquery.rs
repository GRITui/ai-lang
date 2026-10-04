//! `db-query` / `db-query-count` — a small SQL subset over §3m's tables.
//!
//! # The grammar, and the decision to keep it small
//!
//! ```text
//! SELECT <* | col, ...> FROM <table> [WHERE <cond>] [ORDER BY <col> [ASC|DESC]] [LIMIT <n>]
//! cond := <col> <op> <value> (AND|OR <col> <op> <value>)*
//! op   := = | != | < | <= | > | >=
//! ```
//!
//! That is the whole language, and the discipline around it is the point of this
//! module. A v1 that quietly accepted a clause it did not implement is a v1 that
//! returns **the wrong rows** with a `true` beside it: `SELECT name, COUNT(*) FROM
//! t GROUP BY dept` parsed as "select two columns from t" would look like it
//! worked. So [`UNSUPPORTED`] is a first-class table — `JOIN`, `GROUP BY`,
//! `HAVING`, `DISTINCT`, `IN`, `LIKE`, `BETWEEN`, `IS NULL`, aggregates,
//! subqueries — and every one of them is **recognised and refused by name**,
//! with the supported subset printed in the message. An unknown word is a
//! different failure from a known-but-unsupported one, and both are errors; what
//! never happens is silence.
//!
//! # Columns are positions, because the table layer has no schema
//!
//! §3m's `db-create-table` takes a *name* and nothing else, and `db-insert` takes
//! a bare list, so a row has no column names to name. Inventing a schema here
//! would mean changing a builtin the previous card shipped and re-deriving it in
//! the C port — a second feature wearing a query engine's clothes. So a column
//! reference is a **1-based position** (`1` is the primary key, which is exactly
//! what the B-tree indexes) and a projection is `SELECT 2, 3`.
//!
//! The sharp edge that leaves is the *ragged* row: `db-insert` accepts a list of
//! any length, so column 5 of a 3-column row is `nil`, by the same
//! out-of-range-is-nil convention `nth` already uses. `nil` compares equal to
//! `nil` and refuses to be ordered, so the failure is a named error rather than a
//! surprising sort.
//!
//! # Index use is exactly one shape, and it is reported
//!
//! A `WHERE` uses the B-tree when it is a **single `=` on column 1 against a
//! scalar literal** — one point lookup, O(log n). Every other shape is a full
//! scan plus a filter, which is what §3m's `db-all-rows` already is, so the
//! fallback costs no new code and no new order. `Exec::used_index` carries the
//! decision out of the evaluator, which is how the Rust unit tests assert the
//! rule directly; `dbtab_perf`-style timing is what shows the C port obeying it,
//! because a scan and a lookup return the same rows and only a clock can tell
//! them apart.
//!
//! # Two engines, no FFI
//!
//! As with every other `db-*` layer, the C runtime in `ainl-cc/src/runtime.c`
//! (`dbq_*`) is a hand-port, read side-by-side with this file. That makes the
//! error messages part of the contract: they are spelled once here and once
//! there, and `crates/ainl-cc/tests/dbq_aot.rs` asserts them equal.

use std::cmp::Ordering;

use crate::bignum::BigNum;
use crate::db::Db;
use crate::dbtab;
use crate::error::{Error, Result};
use crate::eval::Env;
use crate::suggest::close_match;
use crate::value::{ConsCell, Value};

pub const DB_QUERY: &str = "db-query";
pub const DB_QUERY_COUNT: &str = "db-query-count";

/// Every name this layer adds, in the order the refusal scanner reports them.
pub const SQL_BUILTINS: &[&str] = &[DB_QUERY, DB_QUERY_COUNT];

/// The grammar, quoted in every "not supported in v1" message.
///
/// One constant, so the message cannot disagree with the parser about what is
/// supported — the failure mode this module exists to prevent, which is exactly
/// as likely to appear in a message as in a code path.
pub const SUBSET: &str = "SELECT <* | col, ...> FROM <table> \
[INNER|LEFT JOIN <table> ON <alias>.<col> <op> <alias>.<col>] \
[WHERE <col> <op> <value> [AND|OR <cond>]] [ORDER BY <col> [ASC|DESC]] [LIMIT <n>]";

/// Keywords v1 recognises and refuses, each with the words that could legally
/// follow it here.
///
/// The second half is what makes the refusal *useful* rather than merely
/// correct: `SELECT * FROM t JOIN u ON …` is refused with "`ON` is not supported
/// in v1" and no hint, whereas it can be refused with "did you mean `WHERE`?"
/// when `ON` is a typo and "`JOIN` is not supported in v1" when it is not.
/// Deciding that from the *next* token is what tells the two apart, and it is
/// why this is a table rather than a list of words.
///
/// `JOIN`, `INNER`, `LEFT` and `ON` are **not** here: Tier 5 made them parse, so
/// they are part of the subset and belong in [`SUBSET`]. The join types that stay
/// out of scope remain, each with the words that could follow it, so
/// `CROSS JOIN` is still named rather than ignored.
const UNSUPPORTED: &[(&str, &[&str])] = &[
    // Join types outside v1. `RIGHT`/`FULL`/`OUTER`/`CROSS` and `USING` remain
    // refused by name: the nested loop matches on one qualified comparison, and
    // none of these can be expressed by it.
    ("RIGHT", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("FULL", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("OUTER", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("CROSS", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("USING", &["WHERE", "ORDER", "GROUP", "LIMIT"]),
    // Grouping and aggregation.
    ("GROUP", &["BY", "WHERE", "ORDER", "LIMIT"]),
    ("HAVING", &["WHERE", "ORDER", "LIMIT"]),
    ("DISTINCT", &["FROM", "WHERE", "ORDER", "LIMIT"]),
    ("UNION", &["SELECT", "WHERE", "ORDER", "LIMIT"]),
    ("INTERSECT", &["SELECT", "WHERE", "ORDER", "LIMIT"]),
    ("EXCEPT", &["SELECT", "WHERE", "ORDER", "LIMIT"]),
    (
        "CASE",
        &["WHEN", "THEN", "ELSE", "END", "FROM", "WHERE", "LIMIT"],
    ),
    ("WHEN", &["THEN", "WHERE", "ORDER", "LIMIT"]),
    ("THEN", &["WHEN", "ELSE", "END", "WHERE", "LIMIT"]),
    ("ELSE", &["END", "WHERE", "ORDER", "LIMIT"]),
    // Predicates outside the six operators.
    ("IN", &["WHERE", "ORDER", "LIMIT"]),
    ("LIKE", &["WHERE", "ORDER", "LIMIT"]),
    ("BETWEEN", &["WHERE", "ORDER", "LIMIT"]),
    ("IS", &["NULL", "NOT", "WHERE", "ORDER", "LIMIT"]),
    (
        "NOT",
        &["WHERE", "IN", "LIKE", "BETWEEN", "NULL", "ORDER", "LIMIT"],
    ),
    ("EXISTS", &["WHERE", "ORDER", "LIMIT"]),
    // Modifiers and trailing clauses.
    ("OFFSET", &["ORDER", "WHERE", "LIMIT"]),
    ("AS", &["WHERE", "ORDER", "LIMIT"]),
    ("PRIMARY", &["KEY", "WHERE", "ORDER", "LIMIT"]),
    ("KEY", &["WHERE", "ORDER", "LIMIT"]),
    ("CREATE", &["TABLE", "INDEX", "FROM", "WHERE", "LIMIT"]),
    ("TABLE", &["WHERE", "ORDER", "LIMIT"]),
    ("DROP", &["TABLE", "INDEX", "WHERE", "ORDER", "LIMIT"]),
    ("ALTER", &["TABLE", "WHERE", "ORDER", "LIMIT"]),
    ("ADD", &["WHERE", "ORDER", "LIMIT"]),
    ("INSERT", &["INTO", "VALUES", "FROM", "WHERE", "LIMIT"]),
    ("INTO", &["VALUES", "FROM", "WHERE", "LIMIT"]),
    ("VALUES", &["FROM", "WHERE", "LIMIT"]),
    ("UPDATE", &["SET", "WHERE", "ORDER", "LIMIT"]),
    ("DELETE", &["FROM", "WHERE", "ORDER", "LIMIT"]),
    ("CAST", &["AS", "FROM", "WHERE", "LIMIT"]),
    // Sort modifiers that follow a direction, and the null-ordering clause.
    // Listed because `ASC NULLS FIRST` is a habit strong enough that a model
    // writes it without noticing, and "unexpected 'NULLS'" is a far weaker
    // answer than "NULLS is not supported in v1".
    ("NULLS", &["FIRST", "LAST", "WHERE", "ORDER", "LIMIT"]),
    ("FIRST", &["WHERE", "ORDER", "LIMIT"]),
    ("LAST", &["WHERE", "ORDER", "LIMIT"]),
    // Keywords whose AINL spellings are different, so a model writing SQL by
    // habit gets told the AINL one rather than nothing.
    //
    // `NULL` is deliberately **absent**: it is a legal *value* here (`WHERE 1 =
    // null`, see `P::literal`), so it is in `LEGAL_WORDS` instead. That is the
    // whole reason the two tables exist — a word can be a supported literal and
    // an unsupported clause depending on where it appears, and only the
    // position can say which.
];

/// The words the subset *does* accept, in any position where a bare word is
/// legal: the six clauses, the two booleans, the two spellings of nil, the sort
/// directions, and `BY`.
///
/// A "did you mean" is only useful if the suggestion is one the reader can
/// actually type and have accepted — so this list is the filter applied to
/// every close match, and an out-of-scope keyword is never suggested without
/// saying that it is out of scope.
const LEGAL_WORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "ORDER", "BY", "LIMIT", "ASC", "DESC", "AND", "OR", "true", "false",
    "nil", "null",
    // Tier 5. The join keywords are legal, so a near miss of one (`JION`, `LFET`)
    // is a typo with a fix rather than an unknown word — the same treatment
    // `WHERE` gets.
    "JOIN", "INNER", "LEFT", "ON",
];

/// The aggregate functions, refused with the name of the builtin that does the
/// job rather than with the generic subset sentence — because `SELECT COUNT(*)`
/// is the first thing anyone writes, and "use `db-query-count`" is the whole
/// answer.
const AGGREGATES: &[(&str, &str)] = &[
    ("COUNT", DB_QUERY_COUNT),
    ("SUM", ""),
    ("AVG", ""),
    ("MIN", ""),
    ("MAX", ""),
];

// ---- tokens ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The token's own spelling, so a parse error can quote what was written
    /// rather than an internal name.
    fn text(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }

    /// What the operator means, given the ordering of two comparable values.
    fn holds(self, ord: Ordering) -> bool {
        match self {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A bare word: a table name, a keyword, or a typo of one. Which one is
    /// decided by the grammar's position, never by the word itself — that is what
    /// keeps a table called `order` selectable.
    Word(String),
    Int(i64),
    Float(f64),
    /// A quoted string, already unquoted. Both quote styles are accepted and
    /// neither has escapes: AINL's own lexer has already processed the
    /// escapes by the time this text exists, and a second escape language here
    /// would have to be spelled identically in C to stay in parity.
    Str(String),
    Star,
    Comma,
    /// The qualifier separator in a join's `ON` clause: `<alias>.<position>`.
    ///
    /// Its own token rather than punctuation on the word, because the two sides
    /// are read by different parsers — the left is a table name, the right is a
    /// column number — and folding them into one token would mean re-lexing the
    /// dot to tell a qualified reference from an unqualified one.
    Dot,
    Op(CmpOp),
    LParen,
    RParen,
    End,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    /// 1-based, counted in **characters** — the same unit `Loc` uses, so the
    /// two error vocabularies in a message agree.
    line: u32,
    col: u32,
}

impl Token {
    /// How this token is named inside a message.
    fn describe(&self) -> String {
        match &self.tok {
            Tok::Word(w) => format!("'{w}'"),
            Tok::Int(i) => format!("{i}"),
            Tok::Float(f) => format!("{f}"),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Star => "'*'".to_string(),
            Tok::Comma => "','".to_string(),
            Tok::Dot => "'.'".to_string(),
            Tok::Op(o) => format!("'{}'", o.text()),
            Tok::LParen => "'('".to_string(),
            Tok::RParen => "')'".to_string(),
            Tok::End => "the end of the query".to_string(),
        }
    }

    fn word(&self) -> Option<&str> {
        match &self.tok {
            Tok::Word(w) => Some(w),
            _ => None,
        }
    }
}

// ---- the query -------------------------------------------------------------

/// A column reference: a 1-based position, and where it was written so a
/// runtime comparison failure can point at it.
#[derive(Clone, Copy, Debug)]
struct Col {
    /// 0-based internally; the language is 1-based and the conversion happens
    /// once, in the parser, so no later code can be off by one.
    idx: usize,
    line: u32,
    col: u32,
}

#[derive(Clone, Debug)]
struct Cmp {
    left: Col,
    op: CmpOp,
    right: Value,
}

#[derive(Clone, Debug)]
enum Cond {
    One(Cmp),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Asc,
    Desc,
}

/// Which rows of the left table survive when the `ON` matches nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinKind {
    Inner,
    Left,
}

/// One side of a qualified column reference: `alias` is the table it came from,
/// `col` the position **within that table's row**.
#[derive(Clone, Debug)]
struct QualCol {
    /// Case-insensitive match, as everything else in the grammar is.
    alias: String,
    col: Col,
}

/// The `ON` clause: exactly one qualified comparison, `<a>.<c> <op> <b>.<c>`.
///
/// Both sides are qualified and both sides are columns, which is what makes a
/// join different from a `WHERE`: a `WHERE` compares a column against a *value*
/// and is evaluated once per row of one table, while an `ON` compares two tables'
/// rows and is what decides whether the two are combined at all.
#[derive(Clone, Debug)]
struct OnCond {
    left: QualCol,
    op: CmpOp,
    right: QualCol,
}

/// The join clause, if the query has one.
///
/// One table pair only: a second `JOIN` would need the third table's columns to
/// sit at a known offset in the combined row, and v1 has no `AS` to name it, so
/// the combined row is unambiguous only for two.
#[derive(Clone, Debug)]
struct Join {
    kind: JoinKind,
    /// The right table's name, taken literally like the first table's.
    table: String,
    on: OnCond,
}

#[derive(Clone, Debug)]
struct Query {
    /// The projected columns, or `None` for `*`.
    cols: Option<Vec<Col>>,
    table: String,
    /// The join, or `None` for a single-table query — which is bit-for-bit what
    /// this layer did before Tier 5, so a query with no `JOIN` clause takes the
    /// same path it always did.
    join: Option<Join>,
    filter: Option<Cond>,
    order: Option<(Col, Dir)>,
    limit: Option<usize>,
}

impl Query {
    /// The single scalar literal a PK point lookup would use, if this is the one
    /// shape that can use the index.
    ///
    /// `Eq` only: `!=` is not a point lookup, it is "every row but one", and a
    /// conjunction is not either — a key the tree can find still has to be
    /// filtered by the rest, and pretending otherwise would be a filter that can
    /// disagree with the plan.
    fn index_literal(&self) -> Option<&Value> {
        // A join reads two tables, so there is one B-tree that could answer it
        // and no single key that would. Refused here rather than at the call
        // site: the lookup and the join would otherwise share a path where the
        // second table is silently ignored, which is the one answer a join can
        // never give.
        if self.join.is_some() {
            return None;
        }
        let Cond::One(c) = self.filter.as_ref()? else {
            return None;
        };
        if c.left.idx != 0 || c.op != CmpOp::Eq {
            return None;
        }
        // A list or map has no single byte form to order by, so it cannot be a
        // primary key. Reporting it here rather than at `db-insert` time is
        // deliberate: this is a different call and it deserves its own message.
        match &c.right {
            Value::List(_) | Value::Map(_) => None,
            _ => Some(&c.right),
        }
    }

    /// Does `row` pass the `WHERE`? Short-circuits, and evaluates left to right
    /// so the reported error is the first failing comparison in source order.
    fn matches(&self, row: &ConsCell, who: &str) -> Result<bool> {
        let Some(cond) = &self.filter else {
            return Ok(true);
        };
        eval_cond(cond, row, who)
    }
}

/// The result of a query.
pub struct Exec {
    /// The projected rows. Empty in [`Mode::Count`], which does not need them.
    pub rows: Vec<Value>,
    /// How many rows the query yields — after `LIMIT`, which is what makes
    /// `db-query-count` agree with `(len (db-query …))` rather than counting a
    /// page the caller cannot see.
    pub count: usize,
    /// Whether the B-tree answered the `WHERE`. Never observable from AINL on
    /// this card: a scan returns the same rows, so the only way to see this is
    /// the evaluator (Rust unit tests) or a clock (the AOT perf test).
    pub used_index: bool,
}

/// Whether the caller wants the rows or only how many there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Rows,
    Count,
}

// ---- errors ----------------------------------------------------------------

/// Build a query error: what went wrong, where in the query, the query itself,
/// and optionally what to type instead.
///
/// The four parts are in this order deliberately. A model reading only the first
/// sentence still knows the rule it broke; reading more learns the position, the
/// full text it wrote, and a fix. The AOT runtime prints the identical string,
/// which is why this is a function rather than a format string at each site.
fn sql_error(
    who: &str,
    sql: &str,
    at: &Token,
    detail: impl AsRef<str>,
    suggestion: Option<&str>,
) -> Error {
    let mut msg = format!(
        "{who}: at line {}, col {}: {} in the query \"{sql}\"",
        at.line,
        at.col,
        detail.as_ref()
    );
    if let Some(s) = suggestion {
        msg.push_str(&format!(" — did you mean '{s}'?"));
    }
    Error::runtime(msg)
}

/// `detail` plus the subset sentence, for every "not supported in v1" refusal.
fn unsupported(who: &str, sql: &str, at: &Token, what: &str) -> Error {
    sql_error(
        who,
        sql,
        at,
        format!("{what} is not supported in v1; the supported subset is: {SUBSET}"),
        None,
    )
}

/// Is `w` `want` with one pair of adjacent characters swapped?
///
/// The one edit Levenshtein cannot see: it scores `FROM` → `FORM` as two
/// substitutions, and since [`crate::suggest`]'s distance cap is one edit for a
/// short word, the shared repair machinery never suggests `FROM` for `FORM`. A
/// transposed pair is the commonest typo there is — a fast typist's fingers land
/// out of order — so this is checked wherever the expected word is already
/// known, which here is every keyword position.
///
/// Deliberately **not** a change to `close_match`. Making that function
/// transposition-aware would change the suggestion for every unbound symbol in
/// the language, and the 4-backend rule would then require the C runtime and all
/// three transpiler ports to reproduce the new distance exactly. That is a
/// language-wide change to a shared repair path, and it belongs to whichever
/// card next touches it — recorded here rather than smuggled in.
fn is_transposition(w: &str, want: &str) -> bool {
    let a: Vec<char> = w.to_ascii_uppercase().chars().collect();
    let b: Vec<char> = want.to_ascii_uppercase().chars().collect();
    if a.len() != b.len() || a.len() < 2 {
        return false;
    }
    // Every position must agree except at most two, and those two must be each
    // other's value. One pass, no allocation beyond the two vectors.
    let mut diff: Vec<usize> = Vec::with_capacity(2);
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x != y {
            if diff.len() == 2 {
                return false;
            }
            diff.push(i);
        }
    }
    diff.len() == 2 && a[diff[0]] == b[diff[1]] && a[diff[1]] == b[diff[0]]
}

// ---- the tokenizer ---------------------------------------------------------

/// Split `sql` into tokens.
///
/// Byte-oriented but **column-counted in characters**, because a column a model
/// can count is a column worth reporting. A NUL is not special: a NUL cannot
/// reach this text through an AINL string that the log would accept, and the log
/// refuses to store one, so the two engines never see a query that contains it.
fn tokenize(who: &str, sql: &str) -> Result<Vec<Token>> {
    let mut out: Vec<Token> = Vec::new();
    let b = sql.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut col = 1u32;

    // Advance one character, maintaining the position. Every path that consumes
    // a character goes through here, so the two counters cannot drift apart.
    macro_rules! bump {
        () => {{
            let ch = sql[i..].chars().next().expect("i is on a char boundary");
            i += ch.len_utf8();
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            ch
        }};
    }

    // Read one character **without** advancing, for the two paths that name a
    // character and then return. These exist because `bump!()` in a `format!`
    // is the obvious way to write them and the obvious way is wrong twice over:
    // it moves the position a function that is about to die never reads, which
    // the compiler rightly calls a dead store, and it would make the *message*
    // depend on the order `format!` evaluates its arguments. The position for
    // these errors is `line`/`col` as they stand, which is the character.
    macro_rules! peek {
        () => {
            sql[i..].chars().next().expect("i is on a char boundary")
        };
    }

    while i < b.len() {
        let start_line = line;
        let start_col = col;
        let here = |tok: Tok, line: u32, col: u32| Token { tok, line, col };
        let c = b[i];

        // Whitespace.
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0C || c == 0x0B {
            bump!();
            continue;
        }

        // Operators, including the two-character ones. The order matters: `=`
        // must not be tried before `<=`, or `<=` would lex as two tokens.
        let two: Option<(CmpOp, usize)> = if c == b'!' && b.get(i + 1) == Some(&b'=') {
            Some((CmpOp::Ne, 2))
        } else if (c == b'<' || c == b'>') && b.get(i + 1) == Some(&b'=') {
            Some((if c == b'<' { CmpOp::Le } else { CmpOp::Ge }, 2))
        } else if c == b'=' {
            Some((CmpOp::Eq, 1))
        } else if c == b'<' {
            Some((CmpOp::Lt, 1))
        } else if c == b'>' {
            Some((CmpOp::Gt, 1))
        } else {
            None
        };
        if let Some((op, len)) = two {
            for _ in 0..len {
                bump!();
            }
            out.push(here(Tok::Op(op), start_line, start_col));
            continue;
        }

        // Single-character punctuation. `.` is a qualifier separator, and it is
        // only reached here when it cannot be part of a number — the number path
        // above consumes `1.5` whole, so a dot after an integer is this branch.
        let simple = match c {
            b'*' => Some(Tok::Star),
            b',' => Some(Tok::Comma),
            b'.' => Some(Tok::Dot),
            b'(' => Some(Tok::LParen),
            b')' => Some(Tok::RParen),
            _ => None,
        };
        if let Some(tok) = simple {
            bump!();
            out.push(here(tok, start_line, start_col));
            continue;
        }

        // A quoted string, either quote style, with no escapes.
        if c == b'\'' || c == b'"' {
            let quote = c;
            bump!();
            let mut s = String::new();
            loop {
                if i >= b.len() {
                    return Err(sql_error(
                        who,
                        sql,
                        &here(Tok::End, start_line, start_col),
                        format!(
                            "a string literal opened with {} was never closed",
                            quote as char
                        ),
                        None,
                    ));
                }
                let ch = bump!();
                if ch as u32 == quote as u32 {
                    break;
                }
                s.push(ch);
            }
            out.push(here(Tok::Str(s), start_line, start_col));
            continue;
        }

        // A number: an integer, or a decimal with a fraction.
        if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut text = String::new();
            if c == b'-' {
                text.push(bump!());
            }
            while i < b.len() && b[i].is_ascii_digit() {
                text.push(bump!());
            }
            let mut is_float = false;
            if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                text.push(bump!());
                while i < b.len() && b[i].is_ascii_digit() {
                    text.push(bump!());
                }
            }
            // `1abc`, `1.2.3` and `1e9` are one mistake, not two. The position
            // is the **offending character**, because that is the character the
            // message names: in `1abc` it is the `a`, one column right of the
            // `1`. It is a position and not the start of the number, and the C
            // port checks the same thing — the two were once a column apart
            // because the comment here said "start" and the code said "next".
            if i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                return Err(sql_error(
                    who,
                    sql,
                    &here(Tok::End, line, col),
                    format!("a number cannot be followed by '{}'", peek!()),
                    None,
                ));
            }
            let tok = if is_float {
                Tok::Float(text.parse::<f64>().map_err(|_| {
                    sql_error(
                        who,
                        sql,
                        &here(Tok::End, start_line, start_col),
                        format!("{text} is not a number"),
                        None,
                    )
                })?)
            } else {
                Tok::Int(text.parse::<i64>().map_err(|_| {
                    sql_error(
                        who,
                        sql,
                        &here(Tok::End, start_line, start_col),
                        format!("the integer {text} does not fit in a 64-bit signed integer"),
                        None,
                    )
                })?)
            };
            out.push(here(tok, start_line, start_col));
            continue;
        }

        // A bare word: everything up to whitespace, a quote, or punctuation. A
        // bare word is a keyword or a name depending on where it appears, so
        // nothing is classified here.
        if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            let mut w = String::new();
            loop {
                if i >= b.len() {
                    break;
                }
                let d = b[i];
                if d.is_ascii_alphanumeric() || d == b'_' || d >= 0x80 {
                    w.push(bump!());
                } else {
                    break;
                }
            }
            out.push(here(Tok::Word(w), start_line, start_col));
            continue;
        }

        return Err(sql_error(
            who,
            sql,
            &here(Tok::End, start_line, start_col),
            format!("'{}' is not part of the query language", peek!()),
            None,
        ));
    }

    out.push(Token {
        tok: Tok::End,
        line,
        col,
    });
    Ok(out)
}

// ---- the parser ------------------------------------------------------------

struct P<'a> {
    who: &'a str,
    sql: &'a str,
    toks: Vec<Token>,
    i: usize,
}

impl<'a> P<'a> {
    fn peek(&self) -> &Token {
        // `End` is the last token and is never consumed, so this cannot run off
        // the end however far the parse advances.
        &self.toks[self.i.min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        t
    }

    /// An error at `at` that offers the closest of `expected` as a fix.
    ///
    /// The close match is taken **over the whole candidate set**, against the
    /// word that was actually written — not per candidate against itself, which
    /// would always answer "no match" and make the `expected` list decorative.
    /// That is the whole point of the parameter.
    fn err(&self, at: &Token, detail: impl AsRef<str>, expected: &[&str]) -> Error {
        let fix = at.word().and_then(|w| close_match(w, expected));
        sql_error(self.who, self.sql, at, detail, fix.as_deref())
    }

    /// Consume `word` if it is next, case-insensitively.
    fn eat(&mut self, word: &str) -> bool {
        match self.peek().word() {
            Some(w) if w.eq_ignore_ascii_case(word) => {
                self.next();
                true
            }
            _ => false,
        }
    }

    /// The `not supported in v1` refusal, when `t` is a keyword v1 knows.
    ///
    /// The `next` half is the part that makes it worth having: a word that
    /// close-matches something legal here is reported as a typo with the fix
    /// attached, and only a word that matches nothing legal is reported as an
    /// unsupported construct. Without that split, `SELECT * FORM t` and
    /// `SELECT * FROM t GRPUP BY x` produce the same unhelpful sentence.
    fn refuse_unsupported(&self, t: &Token) -> Error {
        let word = t.word().unwrap_or_default().to_ascii_uppercase();
        if let Some((_, fix)) = AGGREGATES.iter().find(|(k, _)| *k == word) {
            let how = if fix.is_empty() {
                format!(
                    "{word} is not supported in v1; the supported subset is: {SUBSET} — \
                     {word} is an aggregate, and v1 has no aggregate expressions"
                )
            } else {
                format!(
                    "{word} is not supported in v1; the supported subset is: {SUBSET} — \
                     use ({fix} handle \"SELECT …\") to count matching rows"
                )
            };
            return sql_error(self.who, self.sql, t, how, None);
        }
        if let Some((_, next)) = UNSUPPORTED.iter().find(|(k, _)| *k == word) {
            // A word that close-matches something legal *after* this one is a
            // typo, and the fix is the whole answer. A word that matches
            // nothing legal here is a construct v1 does not have, and the subset
            // sentence is the whole answer. Deciding which is which from the next
            // token is what the second half of the table is for.
            if let Some(fix) = close_match(t.word().unwrap_or_default(), *next) {
                return self.err(
                    t,
                    format!("'{word}' is not supported in v1"),
                    &[fix.as_str()],
                );
            }
            return unsupported(self.who, self.sql, t, &format!("'{word}'"));
        }
        // A word v1 does not know at all. A near miss of a keyword it *does*
        // know is still worth naming — `GRPUP` should say "did you mean
        // 'GROUP'?" even though `GROUP` is itself out of scope, because both
        // facts are what a model needs to write a legal query next.
        //
        // The two kinds of near miss get different sentences, and the split
        // matters: suggesting a *legal* keyword ("did you mean 'NULL'?") is the
        // fix, and suggesting an out-of-scope one has to say so in the same
        // breath or the reader writes the suggested query and is refused again.
        if let Some(w) = t.word() {
            let refused: Vec<&str> = UNSUPPORTED
                .iter()
                .map(|(k, _)| *k)
                .chain(AGGREGATES.iter().map(|(k, _)| *k))
                .filter(|k| !LEGAL_WORDS.contains(k))
                .collect();
            if let Some(fix) = close_match(w, &refused) {
                return sql_error(
                    self.who,
                    self.sql,
                    t,
                    format!(
                        "'{w}' is not supported in v1; the supported subset is: {SUBSET} \
                         — did you mean '{fix}'?"
                    ),
                    None,
                );
            }
            if let Some(fix) = close_match(w, LEGAL_WORDS) {
                return self.err(t, format!("unexpected '{w}'"), &[fix.as_str()]);
            }
        }
        // Nothing close: a plain typo, or a name where a keyword was required.
        // The caller's `expected` set is the only useful candidate list, so the
        // caller reports this one.
        self.err(
            t,
            format!("unexpected {}", t.describe()),
            &["SELECT", "FROM", "WHERE", "ORDER", "BY", "LIMIT"],
        )
    }

    /// A keyword position: consume `word`, or report what was found instead.
    ///
    /// A truncated query gets its own sentence. "unexpected the end of the
    /// query" names the symptom; "the query ended, but FROM is required" names
    /// the fix, and a model reading only the first clause can act on it.
    ///
    /// The transposition check is the reason this takes the expected keyword as
    /// a parameter rather than always going through [`close_match`]. Plain
    /// Levenshtein scores `FORM` → `FROM` as **two** edits, not one, so the
    /// shared repair machinery cannot suggest it — and a transposed pair is the
    /// single most common typo there is. Comparing against the *known* word is
    /// both exact and local, where fixing `close_match` would change the
    /// suggestion for every unbound symbol in the language.
    fn keyword(&mut self, word: &str) -> Result<()> {
        let t = self.peek().clone();
        if self.eat(word) {
            return Ok(());
        }
        if matches!(t.tok, Tok::End) {
            return Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!(
                    "the query ended, but {word} is required; the supported subset is: {SUBSET}"
                ),
                None,
            ));
        }
        if let Some(w) = t.word() {
            if is_transposition(w, word) {
                // The fix is attached here rather than left to `err`, which
                // would re-derive it through `close_match` and come back with
                // nothing — which is the whole reason this branch exists.
                return Err(sql_error(
                    self.who,
                    self.sql,
                    &t,
                    format!("unexpected '{w}'"),
                    Some(word),
                ));
            }
        }
        Err(self.refuse_unsupported(&t))
    }

    /// A column reference: a 1-based position.
    ///
    /// The token must be a number, and the number must be at least 1. A bare
    /// word here is either a keyword v1 refuses (reported by name) or a column
    /// *name* — and a name is the mistake this module's whole design rests on,
    /// so it gets its own sentence rather than "expected a number".
    fn column(&mut self) -> Result<Col> {
        let t = self.peek().clone();
        match &t.tok {
            Tok::Int(n) => {
                self.next();
                if *n < 1 {
                    return Err(sql_error(
                        self.who,
                        self.sql,
                        &t,
                        format!("column {n} does not exist; columns are numbered from 1"),
                        None,
                    ));
                }
                Ok(Col {
                    idx: *n as usize - 1,
                    line: t.line,
                    col: t.col,
                })
            }
            Tok::Float(_) => Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!("expected a column number, got {}", t.describe()),
                None,
            )),
            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                if UNSUPPORTED.iter().any(|(k, _)| *k == up)
                    || AGGREGATES.iter().any(|(k, _)| *k == up)
                {
                    return Err(self.refuse_unsupported(&t));
                }
                Err(sql_error(
                    self.who,
                    self.sql,
                    &t,
                    format!(
                        "'{w}' is not a column: a row in this database is a list, so columns \
                         are numbered from 1 and 1 is the primary key"
                    ),
                    None,
                ))
            }
            _ => Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!("expected a column number, got {}", t.describe()),
                None,
            )),
        }
    }

    /// A comparison: `<col> <op> <value>`.
    fn comparison(&mut self) -> Result<Cmp> {
        let left = self.column()?;
        let t = self.peek().clone();
        let op = match &t.tok {
            Tok::Op(o) => *o,
            // A keyword where an operator belongs: `WHERE 1 IN (1)` is a
            // predicate v1 does not have, not a missing operator, and the two
            // answers lead a model to different next queries.
            _ if t.word().is_some() => return Err(self.refuse_unsupported(&t)),
            _ => {
                return Err(sql_error(
                    self.who,
                    self.sql,
                    &t,
                    format!(
                        "expected one of '=', '!=', '<', '<=', '>', '>=' after the column, got {}",
                        t.describe()
                    ),
                    None,
                ))
            }
        };
        self.next();
        let right = self.literal()?;
        Ok(Cmp { left, op, right })
    }

    /// The right-hand side of a comparison.
    ///
    /// A bare word is only a literal if it is `true`, `false`, `nil` or `null`
    /// — SQL's spelling of nil included, because a model writing SQL by habit
    /// will reach for it. Anything else is a *column*, and comparing two columns
    /// is not in the subset, so it is refused rather than guessed at.
    fn literal(&mut self) -> Result<Value> {
        let t = self.peek().clone();
        match t.tok.clone() {
            Tok::Int(n) => {
                self.next();
                Ok(Value::Int(BigNum::small(n)))
            }
            Tok::Float(f) => {
                self.next();
                Ok(Value::Float(f))
            }
            Tok::Str(s) => {
                self.next();
                Ok(Value::str(s))
            }
            // A parenthesised value is a subquery, and saying so beats
            // "expected a value": the reader wrote SQL on purpose, and the
            // answer is that this layer has no subqueries.
            Tok::LParen => Err(unsupported(self.who, self.sql, &t, "a subquery as a value")),
            Tok::Op(_) | Tok::RParen | Tok::Comma | Tok::Star | Tok::Dot => Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!(
                    "expected a value — a number, a quoted string, true, false or nil — got {}",
                    t.describe()
                ),
                None,
            )),
            Tok::Word(w) => {
                if w.eq_ignore_ascii_case("true") {
                    self.next();
                    return Ok(Value::Bool(true));
                }
                if w.eq_ignore_ascii_case("false") {
                    self.next();
                    return Ok(Value::Bool(false));
                }
                if w.eq_ignore_ascii_case("nil") || w.eq_ignore_ascii_case("null") {
                    self.next();
                    return Ok(Value::Nil);
                }
                let up = w.to_ascii_uppercase();
                if UNSUPPORTED.iter().any(|(k, _)| *k == up)
                    || AGGREGATES.iter().any(|(k, _)| *k == up)
                {
                    return Err(self.refuse_unsupported(&t));
                }
                Err(sql_error(
                    self.who,
                    self.sql,
                    &t,
                    format!(
                        "'{w}' is not a value: the right-hand side of a comparison is a number, \
                         a quoted string, true, false or nil, and a column name is not accepted \
                         here (only a column on the left)"
                    ),
                    None,
                ))
            }
            Tok::End => Err(sql_error(
                self.who,
                self.sql,
                &t,
                "expected a value after the comparison operator, but the query ended",
                None,
            )),
        }
    }

    /// `cond := cmp (AND|OR cmp)*`, with `AND` binding tighter — SQL's own
    /// rule, and the only one that needs no parentheses, which v1 does not have.
    fn condition(&mut self) -> Result<Cond> {
        let mut left = Cond::One(self.comparison()?);
        loop {
            if self.eat("AND") {
                let right = Cond::One(self.comparison()?);
                left = Cond::And(Box::new(left), Box::new(right));
            } else if self.eat("OR") {
                // Everything to the right of OR is a full AND-chain, so
                // `a OR b AND c` is `a OR (b AND c)`.
                let right = self.and_chain()?;
                left = Cond::Or(Box::new(left), Box::new(right));
                return Ok(left);
            } else {
                return Ok(left);
            }
        }
    }

    fn and_chain(&mut self) -> Result<Cond> {
        let mut left = Cond::One(self.comparison()?);
        while self.eat("AND") {
            let right = Cond::One(self.comparison()?);
            left = Cond::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// The projection: `*`, or a comma-separated list of column numbers.
    fn projection(&mut self) -> Result<Option<Vec<Col>>> {
        if matches!(self.peek().tok, Tok::Star) {
            self.next();
            return Ok(None);
        }
        // A query that is only `SELECT` has no projection at all — and saying
        // "expected a column number, got the end of the query" sends a model
        // looking for a typo that is not there, when the real problem is a
        // missing `FROM`. The `End` case is checked before the column parser so
        // the sentence names the clause that is actually absent.
        if matches!(self.peek().tok, Tok::End) {
            return Err(sql_error(
                self.who,
                self.sql,
                &self.peek().clone(),
                format!("the query ended, but FROM is required; the supported subset is: {SUBSET}"),
                None,
            ));
        }
        let mut cols = vec![self.column()?];
        while matches!(self.peek().tok, Tok::Comma) {
            self.next();
            cols.push(self.column()?);
        }
        Ok(Some(cols))
    }

    /// A table name, taken **literally** — no keyword check, so a table called
    /// `order`, `key` or `group` is selectable. This is the one place where
    /// treating a word as a name regardless of its spelling is not merely
    /// allowed but necessary: a keyword is a keyword only where the grammar
    /// expects one, and after `FROM` the grammar expects a name.
    ///
    /// The same applies to the table a `JOIN` names. `FROM people GROUP BY 1` is
    /// still refused on `GROUP`, because by then `people` has been read and the
    /// clause loop is looking for a clause — not because `GROUP` is a keyword
    /// that could have been a table name.
    fn table_name(&mut self) -> Result<String> {
        let t = self.peek().clone();
        match t.tok.clone() {
            Tok::Word(w) => {
                self.next();
                Ok(w)
            }
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            // A parenthesised source is a subquery, which v1 does not have.
            // Saying so beats "expected a table name": the reader wrote SQL on
            // purpose, and the answer names the missing feature. A parenthesised
            // *join* is the same thing here — v1 joins are spelled
            // `… JOIN t2 ON …`, never `(SELECT …) JOIN t2`.
            Tok::LParen => Err(unsupported(
                self.who,
                self.sql,
                &t,
                "a subquery or a parenthesised table in FROM",
            )),
            _ => Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!("expected a table name after FROM, got {}", t.describe()),
                None,
            )),
        }
    }

    /// One side of an `ON`: `<alias>.<position>`.
    ///
    /// The qualifier is **required** on both sides, and `left_name`/`right_name`
    /// are the two table names so the refusal can name them: a bare number here
    /// would be a column of the *combined* row, which does not exist yet while
    /// the join is being decided — so accepting one would be a reference to a
    /// row shape that depends on the answer.
    fn on_side(&mut self, left_name: &str, right_name: &str) -> Result<QualCol> {
        let t = self.peek().clone();
        let Tok::Word(alias) = t.tok.clone() else {
            return Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!(
                    "a column in a join's ON clause is written <table>.<column> — \
                     here '{left_name}' and '{right_name}' — got {}",
                    t.describe()
                ),
                None,
            ));
        };
        self.next();
        let dot = self.peek().clone();
        if !matches!(dot.tok, Tok::Dot) {
            return Err(sql_error(
                self.who,
                self.sql,
                &dot,
                format!(
                    "expected '.' after '{alias}' in a join's ON clause, got {} — \
                     a column in ON names the table it comes from",
                    dot.describe()
                ),
                None,
            ));
        }
        self.next();
        let col = self.column()?;
        Ok(QualCol { alias, col })
    }

    /// Is the next word one that starts a join clause this layer has?
    ///
    /// Only the three that begin a **supported** join. The out-of-scope types
    /// are deliberately absent, so they are not consumed here and fall through to
    /// the clause loop, which refuses them by name — a refusal that names the
    /// word the reader has to change is worth more than one that asks for the
    /// `ON` it can never have.
    fn starts_join(&self) -> bool {
        let Some(w) = self.peek().word() else {
            return false;
        };
        matches!(w.to_ascii_uppercase().as_str(), "JOIN" | "INNER" | "LEFT")
    }

    /// The join clause: `[INNER|LEFT] JOIN <table> ON <t1>.<col> <op> <t2>.<col>`.
    ///
    /// `left_table` is the table already read by `FROM`. The join types v1 does
    /// not have are **not** consumed here: `CROSS`/`RIGHT`/`FULL`/`OUTER` and
    /// `USING` are refused by name through [`Self::refuse_unsupported`], so the
    /// reader is told which word is out of scope rather than being asked for an
    /// `ON` it could never satisfy.
    fn join(&mut self, left_table: &str) -> Result<Join> {
        let (kind, spelled) = if self.eat("INNER") {
            (JoinKind::Inner, "INNER")
        } else if self.eat("LEFT") {
            (JoinKind::Left, "LEFT")
        } else {
            // A bare `JOIN` is an INNER join, which is SQL's own default.
            (JoinKind::Inner, "JOIN")
        };
        if !self.eat("JOIN") {
            // `INNER t …` / `LEFT t …` — the type was written without its
            // keyword. A missing word, not an unknown one, so it is named.
            let t = self.peek().clone();
            return Err(sql_error(
                self.who,
                self.sql,
                &t,
                format!("{spelled} must be followed by JOIN; got {}", t.describe()),
                Some("JOIN"),
            ));
        }
        let right_table = self.table_name()?;
        self.keyword("ON")?;
        let on_left = self.on_side(left_table, &right_table)?;
        let t = self.peek().clone();
        // Only the six operators are legal here, and the sentence names all six.
        // A word in this position is not routed to the keyword refusal: inside an
        // `ON` the reader is mid-comparison, and "expected one of '=', '!=', …"
        // tells them what to type where "'IN' is not supported in v1" would send
        // them looking for a clause they did not write.
        let op = match &t.tok {
            Tok::Op(o) => *o,
            _ => {
                return Err(sql_error(
                    self.who,
                    self.sql,
                    &t,
                    format!(
                        "expected one of '=', '!=', '<', '<=', '>', '>=' between the two \
                         columns of a join's ON clause, got {}",
                        t.describe()
                    ),
                    None,
                ))
            }
        };
        self.next();
        let on_right = self.on_side(left_table, &right_table)?;
        // `ON` is exactly one comparison. A second one is a different query
        // rather than a longer `ON`, and `AND`/`OR` are `WHERE`'s job — so the
        // word is consumed here and the refusal points at what follows it.
        if self.eat("AND") || self.eat("OR") {
            let t = self.peek().clone();
            return Err(sql_error(
                self.who,
                self.sql,
                &t,
                "a join's ON clause is one comparison; AND and OR belong in WHERE",
                None,
            ));
        }
        Ok(Join {
            kind,
            table: right_table,
            on: OnCond {
                left: on_left,
                op,
                right: on_right,
            },
        })
    }
}

/// Parse a query, or explain what is wrong with it and where.
///
/// Private, and private on purpose: `Query` is a parser's own shape, and
/// exposing it would make the *fields* the public API — so a future refactor
/// could not change how a query is represented without breaking a caller. The
/// public surface of this module is `execute`, `install`, and the name lists;
/// everything else is the implementation of the parser and is free to move.
fn parse(who: &str, sql: &str) -> Result<Query> {
    let toks = tokenize(who, sql)?;
    let mut p = P {
        who,
        sql,
        toks,
        i: 0,
    };

    p.keyword("SELECT")?;
    let cols = p.projection()?;
    p.keyword("FROM")?;
    let table = p.table_name()?;

    // The join, if there is one. Read **before** the clause loop and only when
    // the next word actually starts a join — the out-of-scope join types
    // (`CROSS`, `RIGHT`, `FULL`, `OUTER`) and `USING` are *not* consumed here,
    // so they fall through to the loop and are refused by name with the subset
    // sentence. That is the difference between "JOIN is not supported in v1"
    // and "'CROSS' is not supported in v1", and the second is the one that
    // tells the reader which word to change.
    let join = if p.starts_join() {
        Some(p.join(&table)?)
    } else {
        None
    };
    // One join. A second is out of scope and is refused **by name**, here rather
    // than by the clause loop below — which would report the second `JOIN` as an
    // unexpected word, a sentence that is true but useless: `JOIN` on its own is
    // legal, so the reader has no way to learn that it is the *second* one, or
    // that three tables are what is out of scope.
    //
    // The reason three tables are out of scope is not effort: a position in `ON`
    // resolves against a table, and after two joins a column's table does not
    // tell you its offset in the combined row without the offsets of the tables
    // before it. That is exactly what an alias would fix, and v1 has no `AS`.
    if p.starts_join() {
        let t = p.peek().clone();
        return Err(sql_error(
            who,
            sql,
            &t,
            "a query may join **one** table in v1, so this is the second JOIN \
             clause — with three tables a column's name no longer says where it \
             sits in the combined row",
            Some("JOIN"),
        ));
    }

    let mut filter = None;
    let mut order = None;
    let mut limit = None;

    // The clauses, in the order SQL requires them: `WHERE`, then `ORDER BY`,
    // then `LIMIT` last. Each is optional.
    //
    // The order is **enforced**, not merely documented, and the comment above
    // used to claim the opposite — the first version of this loop accepted any
    // order, so `LIMIT 1 ORDER BY 2` parsed and quietly applied the limit
    // *before* the sort it was written after. A query that is accepted and
    // answered in the wrong order is the failure this module exists to prevent,
    // and the enforcement is three lines because the grammar is small.
    loop {
        let t = p.peek().clone();
        if matches!(t.tok, Tok::End) {
            break;
        }
        if p.eat("WHERE") {
            if order.is_some() || limit.is_some() {
                return Err(sql_error(
                    who,
                    sql,
                    &t,
                    format!(
                        "WHERE comes before ORDER BY and LIMIT in a query, so {} was written \
                         too late",
                        t.describe()
                    ),
                    None,
                ));
            }
            if filter.is_some() {
                return Err(sql_error(
                    who,
                    sql,
                    &t,
                    "a query can have only one WHERE clause",
                    None,
                ));
            }
            filter = Some(p.condition()?);
            continue;
        }
        if p.eat("ORDER") {
            if limit.is_some() {
                return Err(sql_error(
                    who,
                    sql,
                    &t,
                    format!(
                        "ORDER BY comes before LIMIT in a query, so {} was written too late",
                        t.describe()
                    ),
                    None,
                ));
            }
            if order.is_some() {
                return Err(sql_error(
                    who,
                    sql,
                    &t,
                    "a query can have only one ORDER BY clause",
                    None,
                ));
            }
            // `ORDER 2` is a missing `BY`, and "unexpected 2" sends the reader
            // looking for a bad number rather than a missing word. Named here
            // because `BY` is a keyword and the column parser would not
            // otherwise have anything to compare the token against.
            if !p.eat("BY") {
                let t = p.peek().clone();
                if matches!(t.tok, Tok::Int(_) | Tok::Star | Tok::Word(_)) {
                    return Err(sql_error(
                        who,
                        sql,
                        &t,
                        format!("ORDER must be followed by BY; got {}", t.describe()),
                        Some("BY"),
                    ));
                }
                return Err(p.refuse_unsupported(&t));
            }
            let col = p.column()?;
            // ASC is the default, so `else if p.eat("ASC") { Asc }` would be the
            // same answer as the final `else`. The `eat` stays anyway, and has
            // to: it is what *consumes* the word, so without it a written
            // `ORDER BY 2 ASC` would leave `ASC` unparsed and fail as a
            // trailing token. The direction is the default; the token is not
            // optional to read.
            let dir = if p.eat("DESC") {
                Dir::Desc
            } else {
                p.eat("ASC");
                Dir::Asc
            };
            order = Some((col, dir));
            continue;
        }
        if p.eat("LIMIT") {
            if limit.is_some() {
                return Err(sql_error(
                    who,
                    sql,
                    &t,
                    "a query can have only one LIMIT clause",
                    None,
                ));
            }
            let t = p.peek().clone();
            match &t.tok {
                Tok::Int(n) if *n >= 0 => {
                    p.next();
                    limit = Some(*n as usize);
                }
                Tok::Int(n) => {
                    return Err(sql_error(
                        who,
                        sql,
                        &t,
                        format!("LIMIT {n} is negative; a limit is a count of rows"),
                        None,
                    ))
                }
                _ => {
                    return Err(sql_error(
                        who,
                        sql,
                        &t,
                        format!("expected a row count after LIMIT, got {}", t.describe()),
                        None,
                    ))
                }
            }
            continue;
        }
        return Err(p.refuse_unsupported(&t));
    }

    // The two `ON` qualifiers have to name the two tables actually in the query.
    // An unknown qualifier is refused here rather than in the evaluator so the
    // message can name the two legal ones: a qualifier that silently matched
    // nothing would make every row a `LEFT`-join nil-pad, which reads as an
    // answer rather than as a mistake.
    if let Some(j) = &join {
        for side in [&j.on.left, &j.on.right] {
            if side.alias.eq_ignore_ascii_case(&table) || side.alias.eq_ignore_ascii_case(&j.table)
            {
                continue;
            }
            return Err(sql_error(
                who,
                sql,
                &Token {
                    tok: Tok::Word(side.alias.clone()),
                    line: side.col.line,
                    col: side.col.col,
                },
                format!(
                    "'{}' is not a table in this join — a column in ON is qualified with \
                     the name of one of the tables it joins, and these are '{}' and '{}'",
                    side.alias, table, j.table
                ),
                None,
            ));
        }
        // Both sides must come from **different tables**. With two different
        // table names the check above does not guarantee it: `FROM a JOIN b ON
        // a.1 = a.2` qualifies both sides with `a`, which compares a column with
        // itself and so matches every pair or none — a cross product, not a join.
        // The same shape appears in a self-join (`FROM people JOIN people`),
        // which v1 cannot express usefully: there is no `AS`, so both copies
        // share one name and neither can be named in `ON`. It is refused rather
        // than answered, because a self-join whose two sides cannot be told apart
        // has two plausible answers and the reader cannot tell which they got.
        if j.on.left.alias.eq_ignore_ascii_case(&j.on.right.alias) {
            return Err(sql_error(
                who,
                sql,
                &Token {
                    tok: Tok::Word(j.on.left.alias.clone()),
                    line: j.on.left.col.line,
                    col: j.on.left.col.col,
                },
                format!(
                    "both sides of the ON comparison name '{}', so it compares a column with \
                     itself — one side must be qualified with '{}' and the other with '{}'",
                    j.on.left.alias, table, j.table
                ),
                None,
            ));
        }
    }

    Ok(Query {
        cols,
        table,
        join,
        filter,
        order,
        limit,
    })
}

// ---- evaluation ------------------------------------------------------------

/// The order of two values, for `<`, `<=`, `>`, `>=` and for `ORDER BY`.
///
/// The rules are the ones the language already uses, because a query that
/// ordered differently from `sort` would be a second comparison in the language:
/// numbers compare across int and float, strings compare by **bytes** (so the C
/// port's `memcmp` agrees without a collation table), booleans order
/// `false < true`, and `nil` orders equal to itself and nothing else. Anything
/// else is a named error, never a silent 0 — a comparator that returns 0 for
/// values it cannot compare is a sort that leaves the input order in place and
/// reads as a working sort.
fn order_of(a: &Value, b: &Value) -> Result<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Int(_), Value::Float(_))
        | (Value::Float(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_)) => {
            let (x, y) = (as_f64(a), as_f64(b));
            x.partial_cmp(&y)
                .ok_or_else(|| Error::runtime("db-query: cannot order NaN"))
        }
        (Value::Str(x), Value::Str(y)) => Ok(crate::collections::compare_bytes(x, y)),
        (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
        (Value::Nil, Value::Nil) => Ok(Ordering::Equal),
        _ => Err(Error::runtime(format!(
            "db-query: cannot order a {} and a {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(x) => x.to_f64(),
        Value::Float(x) => *x,
        _ => f64::NAN,
    }
}

/// Evaluate one comparison, turning a value that cannot be compared into a
/// message that names the column, the clause and the two types.
///
/// The context is built here rather than by the caller because the *pair* of
/// types is only known at the comparison, and a message that could not say both
/// would be the one message a model needs.
fn eval_cmp(c: &Cmp, row: &ConsCell, who: &str) -> Result<bool> {
    let left = row.nth(c.left.idx).unwrap_or(&Value::Nil);
    match order_of(left, &c.right) {
        Ok(ord) => Ok(c.op.holds(ord)),
        // `=` and `!=` do not order: `(= "a" 1)` is a false answer, not a type
        // error, because that is what `=` already does everywhere else in the
        // language. Only the four ordering operators need two values to be
        // comparable, so only they can reach here.
        Err(_) if matches!(c.op, CmpOp::Eq | CmpOp::Ne) => {
            Ok(matches!(c.op, CmpOp::Ne) && left != &c.right)
        }
        Err(e) => Err(Error::runtime(format!(
            "{who}: at line {}, col {}: WHERE column {} is a {} and the value compared with it \
             is a {} — a column that is compared with <, <=, > or >= has to hold one type in \
             every row ({})",
            c.left.line,
            c.left.col,
            c.left.idx + 1,
            left.type_name(),
            c.right.type_name(),
            e.message()
        ))),
    }
}

fn eval_cond(cond: &Cond, row: &ConsCell, who: &str) -> Result<bool> {
    match cond {
        Cond::One(c) => eval_cmp(c, row, who),
        Cond::And(a, b) => Ok(eval_cond(a, row, who)? && eval_cond(b, row, who)?),
        Cond::Or(a, b) => Ok(eval_cond(a, row, who)? || eval_cond(b, row, who)?),
    }
}

// ---- the join ---------------------------------------------------------------

/// Which of the two rows a qualified `ON` column refers to.
///
/// Resolved once per query rather than per row: the qualifier names a table, not
/// a row, so deciding it inside the loop would repeat the same two string
/// comparisons for every pair — and, worse, make it look like the alias could
/// change per row, which it cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// The two sides of an `ON`, already resolved to sides of the row pair.
fn resolve_side(q: &QualCol, left_table: &str, right_table: &str) -> Result<Side> {
    if q.alias.eq_ignore_ascii_case(left_table) {
        Ok(Side::Left)
    } else if q.alias.eq_ignore_ascii_case(right_table) {
        Ok(Side::Right)
    } else {
        // Unreachable: `parse` checks both qualifiers against both names and
        // returns before a query with an unknown qualifier can reach here. Kept
        // as an error rather than a default so a future caller that skips the
        // check cannot silently swap one table's column for the other's.
        Err(Error::runtime(format!(
            "db-query: '{}' is not a table in this join",
            q.alias
        )))
    }
}

/// Does `on` hold for this pair of rows?
///
/// The same comparison rules as a `WHERE` (`=` and `!=` never order, the four
/// ordering operators need two orderable values and say so), because the values
/// here are the same values and a second set of rules would be a second answer
/// for the same comparison. A `LEFT` join's nil-padded right columns are only
/// ever seen **after** a non-match, never in this function.
///
/// `left_side` is the side the first qualifier named, and `right_side` the side
/// the second did — so the comparison reads left-value-op-right-value whichever
/// order the writer used the two tables in. `parse` refuses a query whose two
/// qualifiers name the same table, so the `(Left, Left)` / `(Right, Right)` arms
/// are unreachable here; they are folded rather than panicking on so a future
/// relaxation of that check cannot silently compare the wrong two cells.
fn eval_on(
    on: &OnCond,
    lrow: &ConsCell,
    rrow: &ConsCell,
    left_side: Side,
    right_side: Side,
    who: &str,
) -> Result<bool> {
    let lval = if left_side == Side::Left {
        lrow.nth(on.left.col.idx).unwrap_or(&Value::Nil)
    } else {
        rrow.nth(on.left.col.idx).unwrap_or(&Value::Nil)
    };
    let rval = if right_side == Side::Right {
        rrow.nth(on.right.col.idx).unwrap_or(&Value::Nil)
    } else {
        lrow.nth(on.right.col.idx).unwrap_or(&Value::Nil)
    };
    match order_of(lval, rval) {
        Ok(ord) => Ok(on.op.holds(ord)),
        // Same rule as `eval_cmp`: `=` and `!=` across types are a false answer,
        // not a type error.
        Err(_) if matches!(on.op, CmpOp::Eq | CmpOp::Ne) => {
            Ok(matches!(on.op, CmpOp::Ne) && lval != rval)
        }
        Err(e) => Err(Error::runtime(format!(
            "{who}: at line {}, col {}: the ON column {}.{} is a {} and {}.{} is a {} — a \
             column that is compared with <, <=, > or >= has to hold one type in every \
             row it is compared with ({})",
            on.left.col.line,
            on.left.col.col,
            on.left.alias,
            on.left.col.idx + 1,
            lval.type_name(),
            on.right.alias,
            on.right.col.idx + 1,
            rval.type_name(),
            e.message()
        ))),
    }
}

/// The nested loop: every row of the left table against every row of the right.
///
/// **Left first, then right** — SQL's own order, and it is what makes the output
/// deterministic without a sort. Both inputs are already in primary-key order
/// (the B-tree walk), so the pairs come out left-major, right-minor.
///
/// An `INNER` join emits only the pairs the `ON` holds. A `LEFT` join emits
/// every left row, and a left row with no match is padded with `nil` in the
/// right table's columns — **once**, no matter how many right rows failed, which
/// is the difference between a left join and a cross product that was filtered.
///
/// A right table that is empty is not an error: an `INNER` join of it returns
/// nothing and a `LEFT` join returns every left row nil-padded, which is the
/// only answer that is right.
fn nested_loop(
    left: &[Value],
    right: &[Value],
    join: &Join,
    left_table: &str,
    right_width: usize,
    who: &str,
) -> Result<Vec<Value>> {
    let lside = resolve_side(&join.on.left, left_table, &join.table)?;
    let rside = resolve_side(&join.on.right, left_table, &join.table)?;
    let mut out = Vec::new();
    for lrow in left {
        let lcell = cell_of(lrow);
        let mut matched = false;
        for rrow in right {
            let rcell = cell_of(rrow);
            if !eval_on(&join.on, &lcell, &rcell, lside, rside, who)? {
                continue;
            }
            matched = true;
            out.push(concat_rows(&lcell, Some(&rcell), right_width));
        }
        if !matched && join.kind == JoinKind::Left {
            out.push(concat_rows(&lcell, None, right_width));
        }
    }
    Ok(out)
}

/// The combined row: the left table's columns, then the right table's.
///
/// `right` is `None` for a `LEFT` join's miss, and the padding is `nil` — the
/// out-of-range-is-`nil` convention a projection already inherits, applied here
/// to a row that genuinely does not have those columns. That is what makes a
/// padded row answerable rather than an error: a projection of column 4 on a
/// padded row is `nil`, the same answer a short row gives.
fn concat_rows(left: &ConsCell, right: Option<&ConsCell>, right_width: usize) -> Value {
    let mut items: Vec<Value> = left.iter().cloned().collect();
    match right {
        Some(r) => items.extend(r.iter().cloned()),
        // `right_width` is the widest row the right table has, so the padding is
        // the same width whichever left row failed to match — otherwise a padded
        // row's columns would depend on which left row it was, and two engines
        // that walked the tables in a different order would disagree about the
        // shape of the answer rather than its contents.
        None => items.extend(std::iter::repeat_n(Value::Nil, right_width)),
    }
    Value::List(ConsCell::from_values(items))
}

/// The width of the widest row in `rows`, in columns, but never less than the
/// positions the query's own `ON` names on that table.
///
/// The `ON` clause may name a column only the widest row has, so the combined
/// row has to be that wide even when the pair that produced it was narrower —
/// otherwise the same logical row would have different widths depending on which
/// right row matched, and `SELECT`/`ORDER BY` positions would point at different
/// columns on different rows.
///
/// `on_col` is the one-based position the `ON` clause names on this table, or 0
/// when the query has no `ON`. It is a **floor**, not a replacement for the
/// measurement: an `ON` can name column 1 of a three-column table, and the
/// measurement is what says the row is three wide. But when the table has **no
/// rows** the measurement says zero, and a `LEFT` join of an empty table would
/// then pad to nothing — the combined row would be just the left table's columns,
/// and `SELECT 4` would answer `nil` for a column that the query plainly
/// referenced. The floor keeps the shape a reader can see in the query even when
/// there is no data to show it.
fn widest(rows: &[Value], on_col: usize) -> usize {
    rows.iter()
        .map(|r| cell_of(r).len)
        .max()
        .unwrap_or(0)
        .max(on_col)
}

/// Bottom-up stable merge, mirroring [`crate::collections`]'s `sort_merge` and
/// the C runtime's `sort_merge`.
///
/// All three are written the same way rather than one delegating to another,
/// because the property that matters here is **stability**: equal keys must keep
/// primary-key order, or a `ORDER BY` on a duplicated value would answer
/// differently on two engines that both pass every equality test. A host
/// `qsort` cannot be used for the same reason the runtime's `sort` does not use
/// one.
fn merge_by<F>(items: &mut Vec<Value>, cmp: &F) -> Result<()>
where
    F: Fn(&Value, &Value) -> Result<Ordering>,
{
    let n = items.len();
    let mut src = std::mem::take(items);
    let mut dst: Vec<Value> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        dst.clear();
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut l, mut r) = (start, mid);
            while l < mid && r < end {
                if cmp(&src[l], &src[r])? != Ordering::Greater {
                    dst.push(src[l].clone());
                    l += 1;
                } else {
                    dst.push(src[r].clone());
                    r += 1;
                }
            }
            while l < mid {
                dst.push(src[l].clone());
                l += 1;
            }
            while r < end {
                dst.push(src[r].clone());
                r += 1;
            }
            start = end;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    *items = src;
    Ok(())
}

/// Decode stored JSON row texts into values, checking each is a list.
///
/// A helper rather than an inline loop because a join decodes **two** tables and
/// the check — a stored row that is not a list — has to say which table it came
/// from. Inlined, the second call would have had to duplicate the message with a
/// different table name, and a copy is exactly where the two would drift.
fn decode_rows(texts: &[String], who: &str, table: &str) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let row = dbtab::decode_row(t, who, table)?;
        if !matches!(row, Value::List(_)) {
            // The log layer only ever writes a list here, and `db-insert`
            // refuses anything else, so a non-list means the file was written by
            // something else — which `decode_row` already covers for unreadable
            // JSON. This arm is the belt to that braces.
            return Err(Error::runtime(format!(
                "{who}: a row of '{table}' is not a list; it was not written by {}",
                dbtab::DB_INSERT
            )));
        }
        out.push(row);
    }
    Ok(out)
}

/// Run a parsed query against an open database.
pub fn execute(db: &mut Db, who: &str, sql: &str, mode: Mode) -> Result<Exec> {
    let q = parse(who, sql)?;

    // The rows to consider. One point lookup, or the tree's own walk — the
    // latter already sorted by primary key, which is what makes the sort below
    // stable *and* what makes an unordered query's output deterministic.
    let (texts, used_index) = match q.index_literal() {
        Some(lit) => {
            // The same encoder `db-insert` uses for a primary key, so the text
            // the tree is asked about is byte-identical to the one it stored.
            let key = dbtab::key_json(lit, who)?;
            match db.tables().row(&q.table, &key, who)? {
                Some(t) => (vec![t], true),
                None => (Vec::new(), true),
            }
        }
        None => (db.tables().rows(&q.table, who)?, false),
    };

    let left = decode_rows(&texts, who, &q.table)?;

    // The join, before the `WHERE`. The order matters and is not negotiable: a
    // `WHERE` on a joined query addresses the **combined** row, so it can only be
    // evaluated once the two rows have been combined. Filtering the left table
    // first would be faster, and would silently change the answer for any query
    // whose `WHERE` names a right-table column.
    let full = match &q.join {
        None => left,
        Some(join) => {
            let right_texts = db.tables().rows(&join.table, who)?;
            let right = decode_rows(&right_texts, who, &join.table)?;
            // The width floor is the ON column the **joined** table's name
            // appears on. The parser has already refused a query whose two
            // qualifiers are the same, so exactly one of the two sides names the
            // joined table — and reading both rather than choosing one means the
            // floor cannot depend on which side was written first.
            let floor = if join.on.right.alias.eq_ignore_ascii_case(&join.table) {
                join.on.right.col.idx + 1
            } else {
                join.on.left.col.idx + 1
            };
            nested_loop(&left, &right, join, &q.table, widest(&right, floor), who)?
        }
    };

    // The filter, on the combined row.
    let mut full: Vec<Value> = match q.filter {
        None => full,
        Some(_) => {
            let mut kept = Vec::with_capacity(full.len());
            for row in full {
                if q.matches(&cell_of(&row), who)? {
                    kept.push(row);
                }
            }
            kept
        }
    };

    if let Some((col, dir)) = q.order {
        // Every row must actually **have** the column, checked separately from
        // the sort and before it.
        //
        // This is not belt-and-braces. The comparator alone cannot catch a
        // column that is out of range on *every* row: `nil` orders equal to
        // `nil`, so the sort would succeed and silently return primary-key
        // order — a query that reads as working and is not doing what it says.
        // It also cannot catch it when the filter left one row, because a merge
        // of one element never calls a comparator. `SELECT 1 FROM t WHERE 1 =
        // 'a' ORDER BY 99` therefore needs this pass and this pass alone.
        //
        // The projection is *not* checked the same way, and the difference is
        // deliberate: `SELECT 9 FROM t` yields `nil` per row, which is the
        // out-of-range-is-nil convention `nth` already uses and which a
        // projection is expected to inherit. `ORDER BY` is a claim about the
        // table's shape, so a false one is an error.
        for row in &full {
            let cell = cell_of(row);
            if cell.nth(col.idx).is_none() {
                return Err(Error::runtime(format!(
                    "{who}: at line {}, col {}: ORDER BY column {} is nil in every row of '{}' — \
                     a row in this database is a list, and no row here is that long",
                    col.line,
                    col.col,
                    col.idx + 1,
                    q.table
                )));
            }
        }
        if full.len() > 1 {
            // Ordered on the **full** row, before projection, so `SELECT 1 FROM t
            // ORDER BY 2` means what it says. The remaining failure a comparator
            // can hit is a column that holds *different* types in different
            // rows, which no per-row check can see.
            merge_by(&mut full, &|a, b| {
                let ac = cell_of(a);
                let bc = cell_of(b);
                let av = ac.nth(col.idx).unwrap_or(&Value::Nil);
                let bv = bc.nth(col.idx).unwrap_or(&Value::Nil);
                let ord = match order_of(av, bv) {
                    Ok(o) => o,
                    Err(e) => {
                        return Err(Error::runtime(format!(
                            "{who}: at line {}, col {}: ORDER BY column {} is a {} and cannot be \
                             ordered ({})",
                            col.line,
                            col.col,
                            col.idx + 1,
                            av.type_name(),
                            e.message()
                        )))
                    }
                };
                Ok(if dir == Dir::Desc { ord.reverse() } else { ord })
            })?;
        }
    }

    if let Some(n) = q.limit {
        full.truncate(n);
    }

    let count = full.len();
    if mode == Mode::Count {
        return Ok(Exec {
            rows: Vec::new(),
            count,
            used_index,
        });
    }

    let mut rows = Vec::with_capacity(full.len());
    for r in &full {
        let Value::List(cell) = r else {
            unreachable!("pushed only lists")
        };
        match &q.cols {
            None => rows.push(r.clone()),
            Some(cols) => {
                let items: Vec<Value> = cols
                    .iter()
                    .map(|c| cell.nth(c.idx).cloned().unwrap_or(Value::Nil))
                    .collect();
                rows.push(Value::List(ConsCell::from_values(items)));
            }
        }
    }
    Ok(Exec {
        rows,
        count,
        used_index,
    })
}

/// The list inside a row value, for the ordering comparator above. A row is
/// always a list by the time it reaches there, and the fallback keeps the
/// comparator total rather than panicking on a case the caller already
/// filtered.
fn cell_of(v: &Value) -> std::rc::Rc<ConsCell> {
    match v {
        Value::List(c) => c.clone(),
        other => ConsCell::from_values(vec![other.clone()]),
    }
}

// ---- the builtins ----------------------------------------------------------

/// Bind the two query builtins.
///
/// Two names, no collisions: the query layer adds nothing that the byte, value
/// or table layers own, so its position in the install order cannot matter to a
/// program — stated explicitly because it is the opposite of `dbkv`'s
/// situation, where the install order *is* load-bearing.
pub fn install(env: &Env) {
    macro_rules! b {
        ($name:expr, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }
    b!(DB_QUERY, db_query);
    b!(DB_QUERY_COUNT, db_query_count);
}

fn handle_and_sql(args: &[Value], who: &str) -> Result<(i64, String)> {
    let [h, sql] = args else {
        return Err(Error::runtime(format!(
            "{who} expects ({who} handle \"SELECT ...\")"
        )));
    };
    let h = match h {
        Value::Int(n) => n.as_i64().ok_or_else(|| {
            Error::runtime(format!("{who} expects a db handle, got {}", h.type_name()))
        })?,
        other => {
            return Err(Error::runtime(format!(
                "{who} expects a db handle, got {}",
                other.type_name()
            )))
        }
    };
    let sql = match sql {
        Value::Str(s) => s.as_str().to_string(),
        other => {
            return Err(Error::runtime(format!(
                "{who} expects a str query, got {}",
                other.type_name()
            )))
        }
    };
    Ok((h, sql))
}

/// `(db-query handle "SELECT …")` → a list of rows.
fn db_query(args: &[Value]) -> Result<Value> {
    let (h, sql) = handle_and_sql(args, DB_QUERY)?;
    let exec = crate::db::with_db(h, DB_QUERY, |db| execute(db, DB_QUERY, &sql, Mode::Rows))?;
    Ok(Value::List(ConsCell::from_values(exec.rows)))
}

/// `(db-query-count handle "SELECT …")` → how many rows match.
///
/// Counted **after** `LIMIT`, so it agrees with `(len (db-query …))` for the
/// same query rather than answering a different question.
fn db_query_count(args: &[Value]) -> Result<Value> {
    let (h, sql) = handle_and_sql(args, DB_QUERY_COUNT)?;
    let exec = crate::db::with_db(h, DB_QUERY_COUNT, |db| {
        execute(db, DB_QUERY_COUNT, &sql, Mode::Count)
    })?;
    Ok(Value::Int(BigNum::small(exec.count as i64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbtab::TABLE_MARKER_KEY;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ainl-dbq-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            Scratch(p)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A database with a `people` table: three rows, keyed by name.
    fn fixture(tag: &str) -> (Scratch, Db) {
        let s = Scratch::new(tag);
        let mut db = Db::open(&s.path()).expect("open");
        db.tables().create("people");
        db.put(&dbtab::row_key("people", TABLE_MARKER_KEY), "")
            .expect("marker");
        for row in [
            vec![
                Value::str("ada"),
                Value::Int(BigNum::small(36)),
                Value::str("math"),
            ],
            vec![
                Value::str("grace"),
                Value::Int(BigNum::small(45)),
                Value::str("navy"),
            ],
            vec![
                Value::str("bob"),
                Value::Int(BigNum::small(41)),
                Value::str("navy"),
            ],
        ] {
            insert(&mut db, "people", row);
        }
        (s, db)
    }

    fn insert(db: &mut Db, table: &str, cols: Vec<Value>) {
        let key = dbtab::key_json(&cols[0], "db-insert").expect("key");
        let encoded = dbtab::encode_row(&Value::List(ConsCell::from_values(cols)), "db-insert")
            .expect("encode");
        db.put(&dbtab::row_key(table, &key), &encoded).expect("put");
        db.tables().put(table, &key, &encoded).expect("tree");
    }

    /// A `people` table plus a `cities` table, for the join battery.
    ///
    /// The join key is **column 3**, not column 1, so a query that addressed the
    /// combined row as if the tables were stacked the other way round would get a
    /// plausible answer — that is the failure this fixture exists to catch.
    ///
    /// The rows are chosen so the join is not 1:1 on the right:
    ///
    /// ```text
    /// people                     cities
    ///   1 ada   36 "london"        1 london   "uk"
    ///   2 bob   41 "london"        2 sydney   "au"
    ///   3 grace 45 "sydney"
    /// ```
    ///
    /// ada and bob share "london" (one right row matches two left rows), and
    /// grace's "sydney" matches too. Every left row matches, so the `LEFT` miss is
    /// produced by the tests that add a row the right table cannot satisfy.
    fn join_fixture(tag: &str) -> (Scratch, Db) {
        let s = Scratch::new(tag);
        let mut db = Db::open(&s.path()).expect("open");
        for t in ["people", "cities"] {
            db.tables().create(t);
            db.put(&dbtab::row_key(t, TABLE_MARKER_KEY), "")
                .expect("marker");
        }
        for row in [
            vec![
                Value::str("ada"),
                Value::Int(BigNum::small(36)),
                Value::str("london"),
            ],
            vec![
                Value::str("bob"),
                Value::Int(BigNum::small(41)),
                Value::str("london"),
            ],
            vec![
                Value::str("grace"),
                Value::Int(BigNum::small(45)),
                Value::str("sydney"),
            ],
        ] {
            insert(&mut db, "people", row);
        }
        for row in [
            vec![Value::str("london"), Value::str("uk")],
            vec![Value::str("sydney"), Value::str("au")],
        ] {
            insert(&mut db, "cities", row);
        }
        (s, db)
    }

    fn q(db: &mut Db, sql: &str) -> Vec<Value> {
        execute(db, DB_QUERY, sql, Mode::Rows)
            .unwrap_or_else(|e| panic!("{sql}\n  -> {}", e.message()))
            .rows
    }

    fn n(db: &mut Db, sql: &str) -> i64 {
        execute(db, DB_QUERY_COUNT, sql, Mode::Count)
            .unwrap_or_else(|e| panic!("{sql}\n  -> {}", e.message()))
            .count as i64
    }

    fn bad(db: &mut Db, sql: &str) -> String {
        match execute(db, DB_QUERY, sql, Mode::Rows) {
            Ok(e) => panic!("{sql} was accepted, returning {} rows", e.rows.len()),
            Err(e) => e.message().to_string(),
        }
    }

    // ---- the query battery ------------------------------------------------

    #[test]
    fn select_star_returns_every_row_in_key_order() {
        let (_s, mut db) = fixture("star");
        let got = q(&mut db, "SELECT * FROM people");
        assert_eq!(got.len(), 3);
        assert_eq!(
            got,
            vec![
                Value::List(ConsCell::from_values(vec![
                    Value::str("ada"),
                    Value::Int(BigNum::small(36)),
                    Value::str("math")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("bob"),
                    Value::Int(BigNum::small(41)),
                    Value::str("navy")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("grace"),
                    Value::Int(BigNum::small(45)),
                    Value::str("navy")
                ])),
            ]
        );
    }

    #[test]
    fn a_projection_selects_columns_in_the_order_written() {
        let (_s, mut db) = fixture("proj");
        assert_eq!(
            q(&mut db, "SELECT 3, 1 FROM people"),
            vec![
                Value::List(ConsCell::from_values(vec![
                    Value::str("math"),
                    Value::str("ada")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("navy"),
                    Value::str("bob")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("navy"),
                    Value::str("grace")
                ])),
            ],
            "projection order is the written order, not the row's"
        );
        assert_eq!(
            q(&mut db, "SELECT 2 FROM people").len(),
            3,
            "one column per row"
        );
    }

    /// All six operators, each with a known-correct answer.
    ///
    /// The ages are 36 (ada), 41 (bob), 45 (grace) — deliberately not
    /// consecutive, so `<` and `<=` cannot be confused by a test that only
    /// checks the middle value.
    #[test]
    fn every_comparison_operator_answers_correctly() {
        let (_s, mut db) = fixture("ops");
        let names = |sql: &str| -> Vec<String> {
            let (_, mut db) = fixture("ops-names");
            q(&mut db, sql)
                .iter()
                .map(|r| match cell_of(r).nth(0) {
                    Some(Value::Str(s)) => s.as_str().to_string(),
                    other => panic!("column 1 is {other:?}, not a str"),
                })
                .collect()
        };
        assert_eq!(names("SELECT 1 FROM people WHERE 2 = 41"), ["bob"]);
        assert_eq!(
            names("SELECT 1 FROM people WHERE 2 != 41"),
            ["ada", "grace"]
        );
        assert_eq!(names("SELECT 1 FROM people WHERE 2 < 41"), ["ada"]);
        assert_eq!(names("SELECT 1 FROM people WHERE 2 <= 41"), ["ada", "bob"]);
        assert_eq!(names("SELECT 1 FROM people WHERE 2 > 41"), ["grace"]);
        assert_eq!(
            names("SELECT 1 FROM people WHERE 2 >= 41"),
            ["bob", "grace"]
        );
        // Strings, by bytes.
        assert_eq!(names("SELECT 1 FROM people WHERE 1 = 'ada'"), ["ada"]);
        assert_eq!(names("SELECT 1 FROM people WHERE 1 < 'bob'"), ["ada"]);
        // nil and booleans are values, not only numbers.
        assert_eq!(n(&mut db, "SELECT * FROM people WHERE 2 = nil"), 0);
        assert_eq!(n(&mut db, "SELECT * FROM people WHERE 3 = 'navy'"), 2);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // No outer fixture: the closure below makes its own per query, because
        // each one needs a database that is untouched by the last. Sharing one
        // would make the tests order-dependent, and an order-dependent test in
        // this file is a test whose failure depends on which assertions the
        // harness happened to run first.
        let names = |sql: &str, tag: &str| -> Vec<String> {
            let (_, mut db) = fixture(tag);
            q(&mut db, sql)
                .iter()
                .map(|r| cell_of(r).nth(0).unwrap().to_string())
                .collect()
        };
        // ada(36,math) bob(41,navy) grace(45,navy)
        assert_eq!(
            names("SELECT 1 FROM people WHERE 2 > 40 AND 3 = 'navy'", "and"),
            ["bob", "grace"]
        );
        assert_eq!(
            names("SELECT 1 FROM people WHERE 2 = 36 OR 2 = 45", "or"),
            ["ada", "grace"]
        );
        // `a OR b AND c` is `a OR (b AND c)`, so ada matches on the left arm
        // and nothing else does: nobody over 40 is a mathematician. Read
        // strictly left to right, `(2 = 36 OR 2 > 40) AND 3 = 'math'` would
        // exclude ada and return nothing at all — so this one case separates
        // the two readings.
        assert_eq!(
            names(
                "SELECT 1 FROM people WHERE 2 = 36 OR 2 > 40 AND 3 = 'math'",
                "prec"
            ),
            ["ada"]
        );
    }

    #[test]
    fn order_by_ascends_descends_and_is_stable() {
        let (_s, mut db) = fixture("order");
        let col2 = |sql: &str| -> Vec<i64> {
            let (_, mut db) = fixture("order-col");
            q(&mut db, sql)
                .iter()
                .filter_map(|r| {
                    cell_of(r).nth(1).and_then(|v| match v {
                        Value::Int(i) => i.as_i64(),
                        _ => None,
                    })
                })
                .collect()
        };
        assert_eq!(col2("SELECT 1, 2 FROM people ORDER BY 2"), vec![36, 41, 45]);
        assert_eq!(
            col2("SELECT 1, 2 FROM people ORDER BY 2 DESC"),
            vec![45, 41, 36]
        );
        // ASC is the default, and an explicit ASC says the same thing.
        assert_eq!(
            col2("SELECT 1, 2 FROM people ORDER BY 2 ASC"),
            vec![36, 41, 45]
        );
        // A column that is not projected, and not the one selected. `SELECT 1`
        // projects only the name, so the ordering has to read the age out of the
        // full row — which is the whole reason ORDER BY runs before projection.
        // The ages are read back from an unprojected query and joined by name,
        // so the assertion is about the *order of the names*, not about a
        // second projection that would answer the question directly.
        let names = |sql: &str| -> Vec<String> {
            let (_, mut db) = fixture("order-not-projected");
            q(&mut db, sql)
                .iter()
                .map(|r| match cell_of(r).nth(0) {
                    Some(Value::Str(s)) => s.as_str().to_string(),
                    _ => String::new(),
                })
                .collect()
        };
        assert_eq!(
            names("SELECT 1 FROM people ORDER BY 2"),
            ["ada", "bob", "grace"],
            "the ages 36/41/45 order the names, which only works if ORDER BY read \
             the full row rather than the one-column projection"
        );
        assert_eq!(
            names("SELECT 1 FROM people ORDER BY 2 DESC"),
            ["grace", "bob", "ada"]
        );
        // A string column, ordered by bytes.
        let col3 = |sql: &str| -> Vec<String> {
            let (_, mut db) = fixture("order-str");
            q(&mut db, sql)
                .iter()
                .filter_map(|r| match cell_of(r).nth(2) {
                    Some(Value::Str(s)) => Some(s.as_str().to_string()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            col3("SELECT * FROM people ORDER BY 3"),
            ["math", "navy", "navy"],
            "math before navy; the tie keeps key order (bob before grace)"
        );
        assert_eq!(
            col3("SELECT * FROM people ORDER BY 3 DESC"),
            ["navy", "navy", "math"],
            "and DESC reverses it, tie still in key order"
        );
        // A second ORDER BY column is not in the subset, and is refused rather
        // than parsed as a tie-breaker nobody documented.
        let msg = bad(&mut db, "SELECT 1 FROM people ORDER BY 3, 2");
        assert!(msg.contains("unexpected ','"), "got: {msg}");
    }

    /// Ties keep primary-key order, because the walk is in key order and the
    /// merge takes from the left on equality. This is the property a non-stable
    /// sort would lose, and it is what makes an unordered query deterministic.
    #[test]
    fn equal_sort_keys_keep_primary_key_order() {
        let s = Scratch::new("stable");
        let mut db = Db::open(&s.path()).expect("open");
        db.tables().create("t");
        db.put(&dbtab::row_key("t", TABLE_MARKER_KEY), "")
            .expect("marker");
        for (k, v) in [("d", 1), ("a", 1), ("c", 1), ("b", 1)] {
            insert(
                &mut db,
                "t",
                vec![Value::str(k), Value::Int(BigNum::small(v))],
            );
        }
        let got: Vec<String> = q(&mut db, "SELECT 1 FROM t ORDER BY 2 DESC")
            .iter()
            .filter_map(|r| match cell_of(r).nth(0) {
                Some(Value::Str(s)) => Some(s.as_str().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            ["a", "b", "c", "d"],
            "every key ties, so the walk's own key order is the answer — and DESC \
             does not reverse a set with no ordering information to reverse"
        );
    }

    #[test]
    fn limit_caps_the_rows_and_count_agrees_with_it() {
        let (_s, mut db) = fixture("limit");
        assert_eq!(q(&mut db, "SELECT 1 FROM people LIMIT 2").len(), 2);
        assert_eq!(n(&mut db, "SELECT 1 FROM people LIMIT 2"), 2);
        assert_eq!(n(&mut db, "SELECT 1 FROM people"), 3);
        // A limit past the end is not an error.
        assert_eq!(n(&mut db, "SELECT 1 FROM people LIMIT 99"), 3);
        // A limit of zero is an empty result, not "no limit".
        assert_eq!(n(&mut db, "SELECT 1 FROM people LIMIT 0"), 0);
        // LIMIT applies after WHERE, so this is 1 and not 2.
        assert_eq!(
            n(&mut db, "SELECT 1 FROM people WHERE 3 = 'navy' LIMIT 1"),
            1
        );
        // …and after ORDER BY, so it is the *first* row in the sorted order.
        let got: Vec<String> = q(&mut db, "SELECT 1, 2 FROM people ORDER BY 2 DESC LIMIT 1")
            .iter()
            .filter_map(|r| match cell_of(r).nth(0) {
                Some(Value::Str(s)) => Some(s.as_str().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(got, ["grace"], "the oldest, not the first in key order");
    }

    // ---- index use --------------------------------------------------------

    /// The plan rule, asserted on the evaluator rather than inferred from a
    /// clock: a single `=` on column 1 is a point lookup, everything else scans.
    #[test]
    fn a_primary_key_equality_uses_the_index_and_nothing_else_does() {
        let (_s, mut db) = fixture("index");
        for (sql, want) in [
            ("SELECT 1 FROM people WHERE 1 = 'bob'", true),
            ("SELECT 1 FROM people WHERE 1 = 'nope'", true),
            ("SELECT 1 FROM people WHERE 1 != 'bob'", false),
            ("SELECT 1 FROM people WHERE 2 = 41", false),
            ("SELECT 1 FROM people WHERE 1 < 'bob'", false),
            ("SELECT 1 FROM people", false),
            ("SELECT 1 FROM people WHERE 1 = 'bob' AND 2 = 41", false),
            ("SELECT 1 FROM people WHERE 1 = 'bob' OR 1 = 'ada'", false),
        ] {
            let e = execute(&mut db, DB_QUERY, sql, Mode::Rows).expect(sql);
            assert_eq!(e.used_index, want, "wrong plan for {sql}");
        }
    }

    /// The index has to return the *same* rows a scan would, or the plan is not
    /// an optimisation but a second answer.
    #[test]
    fn the_index_path_agrees_with_the_scan_path() {
        let (_s, mut db) = fixture("index-agree");
        let by_index = execute(
            &mut db,
            DB_QUERY,
            "SELECT * FROM people WHERE 1 = 'grace'",
            Mode::Rows,
        )
        .expect("query");
        let by_scan = execute(
            &mut db,
            DB_QUERY,
            "SELECT * FROM people WHERE 1 != 'ada' AND 1 != 'bob'",
            Mode::Rows,
        )
        .expect("query");
        assert!(by_index.used_index && !by_scan.used_index);
        assert_eq!(by_index.rows, by_scan.rows);
    }

    // ---- refusals ---------------------------------------------------------

    /// Malformed SQL is an error with a position and a query, never a crash and
    /// never a partial answer.
    #[test]
    fn malformed_sql_is_named_with_a_position() {
        let (_s, mut db) = fixture("malformed");
        for (sql, must_contain) in [
            ("", "the query ended, but SELECT is required"),
            ("SELECT", "the query ended, but FROM is required"),
            ("SELECT *", "the query ended, but FROM is required"),
            ("SELECT * FROM", "expected a table name after FROM"),
            ("SELCT * FROM people", "SELECT"),
            ("SELECT * FORM people", "FROM"),
            ("SELECT * FROM people WHERE", "expected a column number"),
            ("SELECT * FROM people WHERE 2", "expected one of"),
            (
                "SELECT * FROM people WHERE 2 ~ 1",
                "not part of the query language",
            ),
            ("SELECT * FROM people WHERE 2 = 'x", "never closed"),
            ("SELECT * FROM people ORDER 2", "BY"),
            ("SELECT * FROM people LIMIT", "row count"),
            ("SELECT * FROM people LIMIT -1", "negative"),
            ("SELECT * FROM people WHERE 2 = 1 extra", "unexpected"),
            // The clause order is enforced, not documented: LIMIT is last in
            // SQL, and a query written the other way round is a query whose
            // answer depends on which clause the engine happens to apply first.
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
            ("SELECT 0 FROM people", "numbered from 1"),
            ("SELECT * FROM people WHERE age > 3", "not a column"),
            ("SELECT * FROM people WHERE 1 = ada", "not a value"),
            ("SELECT *, 2 FROM people", "unexpected"),
            ("SELECT 1 2 FROM people", "unexpected"),
            ("SELECT * FROM people WHERE 2 = 1.2.3", "cannot be followed"),
        ] {
            let msg = bad(&mut db, sql);
            assert!(
                msg.contains(must_contain),
                "for {sql:?} the message must mention {must_contain:?}\n  got: {msg}"
            );
            assert!(
                msg.starts_with("db-query: at line 1, col "),
                "for {sql:?} the message must start with the position\n  got: {msg}"
            );
            assert!(
                msg.contains("in the query \""),
                "for {sql:?} the message must quote the query\n  got: {msg}"
            );
        }
    }

    /// A line and a column that point at the offending token, not at the start
    /// of the query — the whole reason the tokenizer counts characters.
    #[test]
    fn a_multiline_query_reports_the_right_line_and_column() {
        let (_s, mut db) = fixture("lines");
        let msg = bad(&mut db, "SELECT *\n  FROM people\n  WHERE 2 GRT 30");
        assert!(
            msg.contains("at line 3, col 11"),
            "GRT starts at column 11 of line 3\n  got: {msg}"
        );
    }

    /// The PO's rule: a clause the parser recognises but v1 does not support is
    /// refused *by name*, with the supported subset attached. Silently ignoring
    /// one is how a query engine returns wrong rows while looking correct.
    #[test]
    fn recognised_but_unsupported_sql_is_refused_by_name() {
        let (_s, mut db) = fixture("unsupported");
        for (sql, must_contain) in [
            // Tier 5 promoted `JOIN`/`INNER`/`LEFT`/`ON` into the subset, so the
            // join types that stay out of scope are the ones refused here.
            (
                "SELECT * FROM people CROSS JOIN cities ON 1 = 1",
                "'CROSS' is not supported in v1",
            ),
            (
                "SELECT * FROM people RIGHT JOIN cities ON 1 = 1",
                "'RIGHT' is not supported in v1",
            ),
            (
                "SELECT * FROM people FULL OUTER JOIN cities ON 1 = 1",
                "'FULL' is not supported in v1",
            ),
            (
                "SELECT * FROM people JOIN cities USING (1)",
                "'USING' is not supported in v1",
            ),
            ("SELECT DISTINCT 1 FROM people", "DISTINCT"),
            ("SELECT 1 FROM people GROUP BY 1", "GROUP"),
            ("SELECT 1 FROM people GROUP BY 1 HAVING 1 = 1", "GROUP"),
            ("SELECT COUNT(*) FROM people", "COUNT"),
            ("SELECT SUM(2) FROM people", "SUM"),
            ("SELECT AVG(2) FROM people", "AVG"),
            ("SELECT MAX(2) FROM people", "MAX"),
            ("SELECT 1 FROM people WHERE 1 IN (1)", "IN"),
            ("SELECT 1 FROM people WHERE 1 LIKE 'a%'", "LIKE"),
            ("SELECT 1 FROM people WHERE 2 BETWEEN 1 AND 2", "BETWEEN"),
            ("SELECT 1 FROM people WHERE 1 IS NULL", "IS"),
            ("SELECT 1 FROM people WHERE NOT 1 = 1", "NOT"),
            (
                "SELECT 1 FROM people WHERE 1 = 1 OR 2 = 2 UNION SELECT 1",
                "UNION",
            ),
            ("SELECT 1 FROM people OFFSET 1", "OFFSET"),
            ("SELECT 1 AS x FROM people", "AS"),
            (
                "SELECT 1 FROM people ORDER BY 1 ASC NULLS FIRST",
                "'NULLS' is not supported in v1",
            ),
            (
                "SELECT * FROM (SELECT 1 FROM people)",
                "a subquery or a parenthesised table",
            ),
            (
                "SELECT 1 FROM people WHERE 1 = (SELECT 1)",
                "a subquery as a value",
            ),
            ("INSERT INTO people VALUES (1)", "INSERT"),
            ("DELETE FROM people WHERE 1 = 1", "DELETE"),
            ("UPDATE people SET 1 = 1", "UPDATE"),
            ("CREATE TABLE t (1)", "CREATE"),
            ("DROP TABLE people", "DROP"),
            (
                "SELECT 1 FROM people WHERE 2 = 1 AND 3 = 1 GROUP BY 2",
                "GROUP",
            ),
        ] {
            let msg = bad(&mut db, sql);
            assert!(
                msg.contains(must_contain),
                "for {sql:?} the refusal must name {must_contain:?}\n  got: {msg}"
            );
            assert!(
                msg.contains("the supported subset is:"),
                "for {sql:?} the refusal must print the supported subset\n  got: {msg}"
            );
        }
    }

    /// A near-miss keyword is a typo with a fix, not "unsupported" — the
    /// difference between a model that repairs its query and one that gives up.
    #[test]
    fn a_near_miss_keyword_gets_a_did_you_mean() {
        let (_s, mut db) = fixture("nearmiss");
        let msg = bad(&mut db, "SELECT 1 FROM people GRPUP BY 1");
        assert!(msg.contains("did you mean 'GROUP'?"), "got: {msg}");
        let msg = bad(&mut db, "SELCT 1 FROM people");
        assert!(msg.contains("did you mean 'SELECT'?"), "got: {msg}");
        let msg = bad(&mut db, "SELECT 1 FROM people ORDR BY 1");
        assert!(msg.contains("did you mean 'ORDER'?"), "got: {msg}");
    }

    /// A table named after a keyword is selectable, because a keyword is only
    /// a keyword where the grammar expects one.
    #[test]
    fn a_table_named_after_a_keyword_is_selectable() {
        let s = Scratch::new("kwtable");
        let mut db = Db::open(&s.path()).expect("open");
        for t in ["order", "key", "limit", "group"] {
            db.tables().create(t);
            db.put(&dbtab::row_key(t, TABLE_MARKER_KEY), "")
                .expect("marker");
            insert(
                &mut db,
                t,
                vec![Value::str("k"), Value::Int(BigNum::small(1))],
            );
        }
        for t in ["order", "key", "limit", "group"] {
            assert_eq!(
                n(&mut db, &format!("SELECT 1 FROM {t}")),
                1,
                "table {t} must be selectable"
            );
        }
    }

    /// The engine-level refusals that are not the parser's.
    #[test]
    fn runtime_refusals_name_the_builtin() {
        let (_s, mut db) = fixture("runtime-refusals");
        // A table that does not exist.
        let msg = bad(&mut db, "SELECT * FROM nope");
        assert_eq!(
            msg, "db-query: no table named 'nope' in this database",
            "got: {msg}"
        );
        // A column no row has: refused by name, with the column and the table.
        // This is the case a comparator alone cannot catch, because nil orders
        // equal to nil — the sort would "succeed" and return key order.
        let msg = bad(&mut db, "SELECT 1 FROM people ORDER BY 9");
        assert!(
            msg.contains("ORDER BY column 9 is nil in every row"),
            "got: {msg}"
        );
        // …and the same query with a filter that leaves exactly one row, where
        // the merge never calls a comparator at all.
        let msg = bad(&mut db, "SELECT 1 FROM people WHERE 1 = 'ada' ORDER BY 9");
        assert!(
            msg.contains("ORDER BY column 9 is nil in every row"),
            "got: {msg}"
        );
        // A *projection* of a column no row has is NOT an error: it is nil per
        // row, which is the out-of-range-is-nil convention `nth` already has.
        // The asymmetry with ORDER BY above is deliberate and is the reason both
        // cases are in this test — one of them was a bug first.
        let got = q(&mut db, "SELECT 9 FROM people");
        assert_eq!(got.len(), 3);
        for r in &got {
            assert_eq!(
                r,
                &Value::List(ConsCell::from_values(vec![Value::Nil])),
                "a projection past the end of a row is nil"
            );
        }
        // A WHERE on a column no row has compares `nil`, and `nil` cannot be
        // ordered — named with both types rather than treated as false, because
        // "0 rows" would look like an answer to a question the query did not ask.
        let msg = bad(&mut db, "SELECT 1 FROM people WHERE 9 > 1");
        assert!(
            msg.contains("WHERE column 9 is a nil") && msg.contains("a int"),
            "got: {msg}"
        );
        // `=` and `!=` do not order, so on the same column they are a plain
        // answer — the rule the language's own `=` already has.
        assert_eq!(n(&mut db, "SELECT 1 FROM people WHERE 9 = nil"), 3);
        assert_eq!(n(&mut db, "SELECT 1 FROM people WHERE 9 != nil"), 0);
        // Two types in one compared column.
        let s2 = Scratch::new("mixed-col");
        let mut db2 = Db::open(&s2.path()).expect("open");
        db2.tables().create("t");
        db2.put(&dbtab::row_key("t", TABLE_MARKER_KEY), "")
            .expect("marker");
        insert(
            &mut db2,
            "t",
            vec![Value::str("a"), Value::Int(BigNum::small(1))],
        );
        insert(&mut db2, "t", vec![Value::str("b"), Value::str("x")]);
        let msg = match execute(
            &mut db2,
            DB_QUERY,
            "SELECT 1 FROM t WHERE 2 > 1",
            Mode::Rows,
        ) {
            Ok(_) => panic!("a mixed column must be refused"),
            Err(e) => e.message().to_string(),
        };
        assert!(
            msg.contains("WHERE column 2 is a str") && msg.contains("a int"),
            "got: {msg}"
        );
    }

    /// `=` and `!=` do not order, so a mixed column is a false answer there
    /// rather than a type error — the same rule the language's `=` has.
    #[test]
    fn equality_does_not_require_two_comparable_types() {
        let s = Scratch::new("mixed-eq");
        let mut db = Db::open(&s.path()).expect("open");
        db.tables().create("t");
        db.put(&dbtab::row_key("t", TABLE_MARKER_KEY), "")
            .expect("marker");
        insert(
            &mut db,
            "t",
            vec![Value::str("a"), Value::Int(BigNum::small(1))],
        );
        insert(&mut db, "t", vec![Value::str("b"), Value::str("x")]);
        assert_eq!(n(&mut db, "SELECT 1 FROM t WHERE 2 = 1"), 1);
        assert_eq!(n(&mut db, "SELECT 1 FROM t WHERE 2 != 1"), 1);
    }

    /// Both builtins' argument checking, and the shape of the refusal.
    #[test]
    fn the_builtins_check_their_operands() {
        for f in [db_query, db_query_count] {
            let e = f(&[]).expect_err("no args");
            assert!(
                e.message().contains("expects (db-query"),
                "got: {}",
                e.message()
            );
            let e = f(&[Value::str("h"), Value::str("SELECT 1")]).expect_err("str handle");
            assert!(e.message().contains("expects a db handle, got str"));
            let e = f(&[Value::Int(BigNum::small(1)), Value::Int(BigNum::small(3))])
                .expect_err("int query");
            assert!(e.message().contains("expects a str query, got int"));
        }
    }

    /// A query is a **read**: it must not append a record, and a rejected query
    /// must not either. The same rule the table layer states for `db-insert`.
    #[test]
    fn a_query_never_writes_to_the_log() {
        let s = Scratch::new("readonly");
        let mut db = Db::open(&s.path()).expect("open");
        db.tables().create("t");
        db.put(&dbtab::row_key("t", TABLE_MARKER_KEY), "")
            .expect("marker");
        insert(
            &mut db,
            "t",
            vec![Value::str("a"), Value::Int(BigNum::small(1))],
        );
        let before = db.keys().len();
        q(&mut db, "SELECT * FROM t");
        q(&mut db, "SELECT 1 FROM t WHERE 1 = 'a'");
        let _ = bad(&mut db, "SELECT * FROM nope");
        let _ = bad(&mut db, "SELECT 1 FROM t GRPUP BY 1");
        assert_eq!(db.keys().len(), before, "a query wrote to the log");
    }

    /// The tokenizer, on the cases that decide whether the C port can agree.
    #[test]
    fn the_tokenizer_covers_its_own_edge_cases() {
        let t = tokenize(DB_QUERY, "a<=-1 1.5 -2 'x' \"y\" != >= = < > * , ( )").expect("lexes");
        let kinds: Vec<&Tok> = t.iter().map(|x| &x.tok).collect();
        assert!(matches!(kinds[1], Tok::Op(CmpOp::Le)));
        assert!(matches!(kinds[2], Tok::Int(-1)));
        assert!(matches!(kinds[3], Tok::Float(_)));
        assert!(matches!(kinds[4], Tok::Int(-2)));
        assert_eq!(kinds[5], &Tok::Str("x".into()));
        assert_eq!(kinds[6], &Tok::Str("y".into()));
        assert!(matches!(kinds[7], Tok::Op(CmpOp::Ne)));
        assert!(matches!(kinds[8], Tok::Op(CmpOp::Ge)));
        // Columns count characters, so a multi-byte word advances one column.
        let t = tokenize(DB_QUERY, "日本 語").expect("lexes");
        assert_eq!(t[1].col, 4, "one column per character, not per byte");
        // A `.` is a qualifier separator. `people.3` lexes as three tokens — the
        // bare word stops at the dot, because a word is alphanumerics and `_` and
        // the dot is neither — so the `ON` parser sees exactly what it needs.
        // `1.5` stays one number: the number path has to win there.
        let t = tokenize(DB_QUERY, "people.3 1.5").expect("lexes");
        let kinds: Vec<&Tok> = t.iter().map(|x| &x.tok).collect();
        assert_eq!(
            kinds[0],
            &Tok::Word("people".into()),
            "a bare word stops at the dot"
        );
        assert_eq!(kinds[1], &Tok::Dot, "the dot is its own token");
        assert_eq!(kinds[2], &Tok::Int(3), "the column number follows the dot");
        assert!(
            matches!(kinds[3], Tok::Float(f) if *f == 1.5),
            "a digit after a dot is a decimal, not a qualifier: {:?}",
            kinds[3]
        );
        // A dot with no digit after it is not part of a number, so it is the
        // separator even where it starts a token.
        let t = tokenize(DB_QUERY, ". 1").expect("lexes");
        assert_eq!(t[0].tok, Tok::Dot);
        assert_eq!(t[1].tok, Tok::Int(1));
    }

    // ---- the join (Tier 5) ------------------------------------------------

    /// An INNER join returns the combined rows: the left table's columns, then
    /// the right table's, one row per matching pair.
    ///
    /// "london" matches two left rows, so the answer has **four** rows for three
    /// left rows — a join is not a lookup, and an implementation that kept one row
    /// per left row would quietly answer a different question.
    #[test]
    fn an_inner_join_returns_the_combined_rows() {
        let (_s, mut db) = join_fixture("inner");
        assert_eq!(
            q(
                &mut db,
                "SELECT * FROM people JOIN cities ON people.3 = cities.1"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![
                    Value::str("ada"),
                    Value::Int(BigNum::small(36)),
                    Value::str("london"),
                    Value::str("london"),
                    Value::str("uk"),
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("bob"),
                    Value::Int(BigNum::small(41)),
                    Value::str("london"),
                    Value::str("london"),
                    Value::str("uk"),
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("grace"),
                    Value::Int(BigNum::small(45)),
                    Value::str("sydney"),
                    Value::str("sydney"),
                    Value::str("au"),
                ])),
            ],
            "the combined row is the left columns then the right columns, one row per pair"
        );
        // `INNER` spelled out and a bare `JOIN` are the same query — SQL's own
        // default, and a reader who writes either must get the same answer.
        assert_eq!(
            q(
                &mut db,
                "SELECT 5 FROM people INNER JOIN cities ON people.3 = cities.1"
            ),
            q(
                &mut db,
                "SELECT 5 FROM people JOIN cities ON people.3 = cities.1"
            ),
            "a bare JOIN and INNER JOIN are the same query"
        );
        // The pair order is left-major: both people rows that match "london" come
        // before grace, in primary-key order. That is the B-tree walk order, and
        // it is what makes an unordered join deterministic.
        assert_eq!(
            q(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![Value::str("ada")])),
                Value::List(ConsCell::from_values(vec![Value::str("bob")])),
                Value::List(ConsCell::from_values(vec![Value::str("grace")])),
            ]
        );
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1"
            ),
            3
        );
    }

    /// A LEFT join keeps every left row and nil-pads the right table's columns on
    /// a miss — **once**, no matter how many right rows failed to match.
    ///
    /// The `oslo` row is in `people` and in no `cities` row, so the `LEFT` answer
    /// has four rows where the `INNER` answer has three. The padded columns are
    /// `nil` rather than absent, so `SELECT 5` is `nil` rather than an error —
    /// the out-of-range-is-`nil` convention a projection already inherits.
    #[test]
    fn a_left_join_keeps_every_left_row_and_nil_pads_the_miss() {
        let (_s, mut db) = join_fixture("left");
        insert(
            &mut db,
            "people",
            vec![
                Value::str("oslo"),
                Value::Int(BigNum::small(1)),
                Value::str("oslo"),
            ],
        );
        // An INNER join of the same data drops oslo; a LEFT join keeps it. Both
        // are asserted on the same fixture so the difference is the join type and
        // nothing else.
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1"
            ),
            3,
            "an INNER join drops the row with no match"
        );
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people LEFT JOIN cities ON people.3 = cities.1"
            ),
            4,
            "a LEFT join keeps the row with no match"
        );
        assert_eq!(
            q(
                &mut db,
                "SELECT 1, 5 FROM people LEFT JOIN cities ON people.3 = cities.1 \
                 WHERE 1 = 'oslo'"
            ),
            vec![Value::List(ConsCell::from_values(vec![
                Value::str("oslo"),
                Value::Nil,
            ]))],
            "the right table's columns are nil-padded on a miss"
        );
        // A padded row is still a row of the combined width: `SELECT *` shows the
        // nils rather than dropping the columns, so the answer's shape does not
        // depend on which left row missed.
        let padded = q(
            &mut db,
            "SELECT * FROM people LEFT JOIN cities ON people.3 = cities.1 WHERE 1 = 'oslo'",
        );
        assert_eq!(
            padded,
            vec![Value::List(ConsCell::from_values(vec![
                Value::str("oslo"),
                Value::Int(BigNum::small(1)),
                Value::str("oslo"),
                Value::Nil,
                Value::Nil,
            ]))],
            "a padded row has the same width as a matched one"
        );
    }

    /// A right table with no rows at all: an INNER join returns nothing and a LEFT
    /// join returns every left row nil-padded.
    ///
    /// Not an error, and not an empty result for the LEFT — an empty right table
    /// is the extreme case of "no match", and answering "no rows" for it would be
    /// the one case where the two join types agree when they must not.
    ///
    /// The padding is **one column**, not two, because the right table has no rows
    /// to measure and the ON clause names column 1 of it. That is a deliberate
    /// floor rather than an accident: `SELECT 4` — a right-table column — is then
    /// `nil` rather than an out-of-range position, and the combined row keeps the
    /// shape the query describes even with no data to show it.
    #[test]
    fn an_empty_right_table_is_not_an_error() {
        let s = Scratch::new("empty-right");
        let mut db = Db::open(&s.path()).expect("open");
        for t in ["people", "cities"] {
            db.tables().create(t);
            db.put(&dbtab::row_key(t, TABLE_MARKER_KEY), "")
                .expect("marker");
        }
        for row in [
            vec![
                Value::str("ada"),
                Value::Int(BigNum::small(36)),
                Value::str("london"),
            ],
            vec![
                Value::str("bob"),
                Value::Int(BigNum::small(41)),
                Value::str("london"),
            ],
        ] {
            insert(&mut db, "people", row);
        }
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1"
            ),
            0
        );
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people LEFT JOIN cities ON people.3 = cities.1"
            ),
            2,
            "a LEFT join of an empty table keeps every left row"
        );
        // The ON column's position is the floor for the padding, so the combined
        // row is `people`'s three columns plus one — the column the query named —
        // and reading that column answers nil rather than being out of range.
        assert_eq!(
            q(
                &mut db,
                "SELECT 4 FROM people LEFT JOIN cities ON people.3 = cities.1"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![Value::Nil])),
                Value::List(ConsCell::from_values(vec![Value::Nil])),
            ],
            "a right-table column of an empty table is nil, not out of range"
        );
    }

    /// A second `JOIN` is refused **as the second one**, naming three tables as
    /// what is out of scope.
    ///
    /// The tempting message here is "unexpected 'JOIN'", and it is both true and
    /// useless: `JOIN` on its own is legal as of Tier 5, so a reader who saw that
    /// would have no way to learn that it is the *second* clause that v1 does not
    /// have. The refusal names the count instead.
    #[test]
    fn a_second_join_clause_is_refused_as_the_second_one() {
        let (_s, mut db) = join_fixture("second-join");
        let msg = bad(
            &mut db,
            "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 \
             JOIN towns ON cities.1 = towns.1",
        );
        assert!(
            msg.contains("may join **one** table in v1"),
            "the refusal must name the limit: {msg}"
        );
        assert!(
            msg.contains("second JOIN clause"),
            "the refusal must say which JOIN is the problem: {msg}"
        );
        assert!(
            !msg.contains("unexpected 'JOIN'"),
            "a legal word cannot be reported as unexpected: {msg}"
        );
        // `INNER` and `LEFT` open a join clause too, so a second one spelled
        // either way is refused the same way.
        for sql in [
            "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 \
             LEFT JOIN towns ON cities.1 = towns.1",
            "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 \
             INNER JOIN towns ON cities.1 = towns.1",
        ] {
            assert!(
                bad(&mut db, sql).contains("second JOIN clause"),
                "for {sql:?} the refusal must name the second clause"
            );
        }
    }

    /// Every operator works in an `ON`, and the comparison reads the two columns
    /// in the order they were written.
    #[test]
    fn every_operator_works_in_an_on_clause() {
        let (_s, mut db) = join_fixture("on-ops");
        // `=` on the join key: three pairs match.
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1"
            ),
            3
        );
        // `!=` on the join key: three left rows × two right rows is six pairs, and
        // three of them match, so the other three. A `!=` join is a real join, not
        // the negation of an answer — the rows it keeps are the *pairs* that fail
        // the comparison, not the complements of the rows an `=` join returned.
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 != cities.1"
            ),
            3,
            "six pairs, three of which match, so three do not"
        );
        // The ordering operators work when both columns are orderable, and name
        // both types when they are not. `people.2` is an age and `cities.1` a
        // name, so `<` across the two is an error — not a silent false, which
        // would answer "no rows match" for a comparison nobody can evaluate.
        let msg = bad(
            &mut db,
            "SELECT 1 FROM people JOIN cities ON people.2 < cities.1",
        );
        assert!(
            msg.contains("people.2 is a int") && msg.contains("cities.1 is a str"),
            "an ordering comparison across types names both:\n  {msg}"
        );
        // The comparison is written left-to-right, so the reverse spelling has the
        // types the other way round and is a *different* message.
        let msg = bad(
            &mut db,
            "SELECT 1 FROM people JOIN cities ON cities.1 < people.2",
        );
        assert!(
            msg.contains("cities.1 is a str") && msg.contains("people.2 is a int"),
            "the message follows the order the columns were written:\n  {msg}"
        );
    }

    /// A join combined with `WHERE`, `ORDER BY` and `LIMIT` — and the combined row
    /// is what the bare positions address.
    ///
    /// Column 5 is `cities.2` (the country), column 2 is `people.2` (the age). A
    /// `WHERE` naming column 5 cannot be evaluated before the join, which is why
    /// the join runs first.
    #[test]
    fn a_join_combines_with_where_order_by_and_limit() {
        let (_s, mut db) = join_fixture("clauses");
        assert_eq!(
            q(
                &mut db,
                "SELECT 5 FROM people JOIN cities ON people.3 = cities.1 \
                 WHERE 5 = 'uk'"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![Value::str("uk")])),
                Value::List(ConsCell::from_values(vec![Value::str("uk")])),
            ],
            "a WHERE on a right-table column sees the combined row"
        );
        // ORDER BY on a combined-row column, DESC: the two "uk" rows first, in
        // primary-key order — ada then bob, not reversed.
        assert_eq!(
            q(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 ORDER BY 5 DESC"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![Value::str("ada")])),
                Value::List(ConsCell::from_values(vec![Value::str("bob")])),
                Value::List(ConsCell::from_values(vec![Value::str("grace")])),
            ]
        );
        assert_eq!(
            q(
                &mut db,
                "SELECT 1, 2 FROM people JOIN cities ON people.3 = cities.1 \
                 ORDER BY 2 DESC LIMIT 2"
            ),
            vec![
                Value::List(ConsCell::from_values(vec![
                    Value::str("grace"),
                    Value::Int(BigNum::small(45)),
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("bob"),
                    Value::Int(BigNum::small(41)),
                ])),
            ],
            "ORDER BY runs on the combined row, then LIMIT"
        );
        // `db-query-count` counts the same rows the query returns.
        assert_eq!(
            n(
                &mut db,
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 \
                 WHERE 5 = 'uk' LIMIT 1"
            ),
            1
        );
    }

    /// The join types v1 does not have are refused **by name**, with the subset
    /// sentence — never silently treated as an INNER join.
    ///
    /// This is the failure Tier 5 exists to avoid on the other side: an engine
    /// that read `CROSS JOIN` as a plain join would answer a cross product while
    /// the reader believed they had asked for something else.
    #[test]
    fn a_refused_join_type_is_named() {
        let (_s, mut db) = join_fixture("refuse-type");
        for (sql, must_contain) in [
            (
                "SELECT * FROM people CROSS JOIN cities ON people.3 = cities.1",
                "'CROSS' is not supported in v1",
            ),
            (
                "SELECT * FROM people RIGHT JOIN cities ON people.3 = cities.1",
                "'RIGHT' is not supported in v1",
            ),
            (
                "SELECT * FROM people FULL OUTER JOIN cities ON people.3 = cities.1",
                "'FULL' is not supported in v1",
            ),
            (
                "SELECT * FROM people JOIN cities USING (3)",
                "'USING' is not supported in v1",
            ),
        ] {
            let msg = bad(&mut db, sql);
            assert!(
                msg.contains(must_contain),
                "for {sql:?} the refusal must name {must_contain:?}\n  got: {msg}"
            );
            assert!(
                msg.contains("the supported subset is:"),
                "for {sql:?} the refusal must print the supported subset\n  got: {msg}"
            );
        }
    }

    /// A malformed `ON` is refused with a message that says what an `ON` is,
    /// rather than failing on a token the reader did not know was a token.
    #[test]
    fn a_malformed_on_clause_is_explained() {
        let (_s, mut db) = join_fixture("bad-on");
        for (sql, must_contain) in [
            // An unqualified column: the combined row does not exist yet.
            (
                "SELECT 1 FROM people JOIN cities ON 1 = 1",
                "is written <table>.<column>",
            ),
            (
                "SELECT 1 FROM people JOIN cities ON 3 = cities.1",
                "is written <table>.<column>",
            ),
            // A missing dot.
            (
                "SELECT 1 FROM people JOIN cities ON people = cities.1",
                "expected '.'",
            ),
            // Two comparisons in the ON.
            (
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 AND people.2 = 36",
                "one comparison",
            ),
            (
                "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 OR people.2 = 36",
                "one comparison",
            ),
            // A value on the right of the operator, not a qualified column.
            (
                "SELECT 1 FROM people JOIN cities ON people.3 = 1",
                "is written <table>.<column>",
            ),
            // No operator at all: the sentence names the six that are legal,
            // rather than blaming the word that happened to follow.
            (
                "SELECT 1 FROM people JOIN cities ON people.3 cities.1",
                "between the two columns of a join's ON clause",
            ),
            // A join type with no JOIN.
            (
                "SELECT 1 FROM people LEFT cities ON people.3 = cities.1",
                "must be followed by JOIN",
            ),
            // A qualifier that is not one of the two tables.
            (
                "SELECT 1 FROM people JOIN cities ON people.3 = towns.1",
                "is not a table in this join",
            ),
            // Both sides on one table: a column compared with itself.
            (
                "SELECT 1 FROM people JOIN cities ON people.3 = people.2",
                "compares a column with itself",
            ),
            // A missing ON entirely.
            (
                "SELECT 1 FROM people JOIN cities",
                "the query ended, but ON is required",
            ),
        ] {
            let msg = bad(&mut db, sql);
            assert!(
                msg.contains(must_contain),
                "for {sql:?} the refusal must say {must_contain:?}\n  got: {msg}"
            );
        }
    }

    /// A self-join is refused with a reason, not silently answered.
    ///
    /// The spec allows a self-join (one table under two names), but v1 has no
    /// `AS`, so both copies share the one name and neither can be named in `ON` —
    /// which is why this shape is refused by name rather than answered with a
    /// cross product. The refusal says exactly that, so the reader knows what to
    /// add.
    #[test]
    fn a_self_join_is_refused_because_there_is_no_as() {
        let (_s, mut db) = join_fixture("self");
        let msg = bad(
            &mut db,
            "SELECT 1 FROM people JOIN people ON people.1 = people.2",
        );
        assert!(msg.contains("compares a column with itself"), "got: {msg}");
        // An alias written with `AS` is still refused — `AS` is out of scope — but
        // it is refused as the out-of-scope keyword it is, not as a join problem.
        let msg = bad(
            &mut db,
            "SELECT 1 FROM people JOIN people AS p ON people.1 = p.2",
        );
        assert!(msg.contains("'AS' is not supported in v1"), "got: {msg}");
    }

    /// A single-table query is byte-for-byte what it was before Tier 5: the same
    /// rows, the same order, the same count, and the index path still chosen.
    ///
    /// Asserted against the **fixture that has two tables**, so a join that leaked
    /// into a query with no `JOIN` clause would change an answer rather than pass
    /// unnoticed.
    #[test]
    fn a_query_with_no_join_is_unchanged() {
        let (_s, mut db) = join_fixture("no-join");
        // The `people` rows here have three columns, unlike the single-table
        // fixture, so this asserts the values rather than a shape.
        assert_eq!(
            q(&mut db, "SELECT * FROM people"),
            vec![
                Value::List(ConsCell::from_values(vec![
                    Value::str("ada"),
                    Value::Int(BigNum::small(36)),
                    Value::str("london"),
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("bob"),
                    Value::Int(BigNum::small(41)),
                    Value::str("london"),
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("grace"),
                    Value::Int(BigNum::small(45)),
                    Value::str("sydney"),
                ])),
            ]
        );
        assert_eq!(n(&mut db, "SELECT 1 FROM people"), 3);
        // The point-lookup path is still chosen for a single-table query — a join
        // must not have disabled it — and a join must not take it, because the
        // index answers about one table and the query reads two.
        let e = execute(
            &mut db,
            DB_QUERY,
            "SELECT 1 FROM people WHERE 1 = 'ada'",
            Mode::Rows,
        )
        .expect("indexed");
        assert!(
            e.used_index,
            "a single-table point lookup still uses the index"
        );
        let e = execute(
            &mut db,
            DB_QUERY,
            "SELECT 1 FROM people JOIN cities ON people.3 = cities.1 WHERE 1 = 'ada'",
            Mode::Rows,
        )
        .expect("joined");
        assert!(
            !e.used_index,
            "a join reads two tables, so no single-key lookup can answer it"
        );
        // `cities` is still a plain table when nothing joins it.
        assert_eq!(n(&mut db, "SELECT 1 FROM cities"), 2);
    }

    /// The `SUBSET` sentence a refusal prints is the grammar that is actually
    /// accepted, so a join has to be in it — and the same string is compared
    /// byte-for-byte against the C runtime's `DBQ_SUBSET` by `dbq_aot.rs`.
    #[test]
    fn the_subset_sentence_documents_the_join() {
        assert!(
            SUBSET.contains("LEFT JOIN <table> ON"),
            "the subset sentence must show the join clause: {SUBSET}"
        );
        assert!(
            !SUBSET.contains("GROUP BY") && !SUBSET.contains("COUNT"),
            "the subset sentence must not advertise what it refuses: {SUBSET}"
        );
        // Every keyword the join grammar uses is a legal word, so a near miss of
        // one is a typo with a fix.
        for kw in ["JOIN", "INNER", "LEFT", "ON"] {
            assert!(
                LEGAL_WORDS.contains(&kw),
                "{kw} is in the grammar, so it must be a legal word"
            );
        }
        // And the join types that stay out of scope are still in the refusal table.
        for kw in ["RIGHT", "FULL", "OUTER", "CROSS", "USING"] {
            assert!(
                UNSUPPORTED.iter().any(|(k, _)| *k == kw),
                "{kw} is still out of scope and must be refused by name"
            );
        }
        assert!(
            !UNSUPPORTED.iter().any(|(k, _)| *k == "JOIN"),
            "JOIN is supported now and must not be in the refusal table"
        );
    }
}
