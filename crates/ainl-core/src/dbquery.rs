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
const UNSUPPORTED: &[(&str, &[&str])] = &[
    // Joins.
    ("JOIN", &["ON", "USING", "WHERE", "ORDER", "GROUP", "LIMIT"]),
    ("INNER", &["JOIN", "ON", "WHERE", "ORDER", "LIMIT"]),
    ("LEFT", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("RIGHT", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("FULL", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("OUTER", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("CROSS", &["JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT"]),
    ("ON", &["WHERE", "ORDER", "GROUP", "LIMIT"]),
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

#[derive(Clone, Debug)]
struct Query {
    /// The projected columns, or `None` for `*`.
    cols: Option<Vec<Col>>,
    table: String,
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

        // Single-character punctuation.
        let simple = match c {
            b'*' => Some(Tok::Star),
            b',' => Some(Tok::Comma),
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
                text.push(bump!() as char);
            }
            while i < b.len() && b[i].is_ascii_digit() {
                text.push(bump!() as char);
            }
            let mut is_float = false;
            if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                text.push(bump!() as char);
                while i < b.len() && b[i].is_ascii_digit() {
                    text.push(bump!() as char);
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
                Ok(Value::Int(n))
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
            Tok::Op(_) | Tok::RParen | Tok::Comma | Tok::Star => Err(sql_error(
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

    // The table name. A quoted string or a bare word, taken **literally** — no
    // keyword check, so a table called `order` or `key` is selectable. This is
    // the one place where treating a word as a name regardless of its spelling
    // is not merely allowed but necessary.
    let t = p.peek().clone();
    let table = match t.tok.clone() {
        Tok::Word(w) => {
            p.next();
            w
        }
        Tok::Str(s) => {
            p.next();
            s
        }
        // A parenthesised source is a subquery or a join, and both are out of
        // scope. Saying so is the whole difference between a model that knows
        // what to do next and one that retries the same shape.
        Tok::LParen => {
            return Err(unsupported(
                who,
                sql,
                &t,
                "a subquery or a parenthesised table (a join) in FROM",
            ))
        }
        _ => {
            return Err(sql_error(
                who,
                sql,
                &t,
                format!("expected a table name after FROM, got {}", t.describe()),
                None,
            ))
        }
    };

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
            let dir = if p.eat("DESC") {
                Dir::Desc
            } else if p.eat("ASC") {
                Dir::Asc
            } else {
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

    Ok(Query {
        cols,
        table,
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
        Value::Int(x) => *x as f64,
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

    let mut full: Vec<Value> = Vec::with_capacity(texts.len());
    for t in &texts {
        let row = dbtab::decode_row(t, who, &q.table)?;
        let Value::List(cell) = &row else {
            // The log layer only ever writes a list here, and `db-insert`
            // refuses anything else, so a non-list means the file was written by
            // something else — which `decode_row` already covers for unreadable
            // JSON. This arm is the belt to that braces.
            return Err(Error::runtime(format!(
                "{who}: a row of '{}' is not a list; it was not written by {}",
                q.table,
                dbtab::DB_INSERT
            )));
        };
        if q.matches(cell, who)? {
            full.push(row);
        }
    }

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
        Value::Int(n) => *n,
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
    Ok(Value::Int(exec.count as i64))
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
            vec![Value::str("ada"), Value::Int(36), Value::str("math")],
            vec![Value::str("grace"), Value::Int(45), Value::str("navy")],
            vec![Value::str("bob"), Value::Int(41), Value::str("navy")],
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
                    Value::Int(36),
                    Value::str("math")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("bob"),
                    Value::Int(41),
                    Value::str("navy")
                ])),
                Value::List(ConsCell::from_values(vec![
                    Value::str("grace"),
                    Value::Int(45),
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
                        Value::Int(i) => Some(*i),
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
            insert(&mut db, "t", vec![Value::str(k), Value::Int(v)]);
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
            ("SELECT * FROM people JOIN t2 ON 1 = 1", "JOIN"),
            ("SELECT * FROM people LEFT JOIN t2 ON 1 = 1", "LEFT"),
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
            insert(&mut db, t, vec![Value::str("k"), Value::Int(1)]);
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
        insert(&mut db2, "t", vec![Value::str("a"), Value::Int(1)]);
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
        insert(&mut db, "t", vec![Value::str("a"), Value::Int(1)]);
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
            let e = f(&[Value::Int(1), Value::Int(3)]).expect_err("int query");
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
        insert(&mut db, "t", vec![Value::str("a"), Value::Int(1)]);
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
    }
}
