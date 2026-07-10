//! Parser: tokens -> AST.
//!
//! The AST is intentionally uniform (atoms + lists) and every node carries a
//! [`Span`] into the original source. Those spans are the foundation for the
//! source-map projection described in the master plan (§1.1): a projected
//! human-readable line can always be traced back to the AINL bytes that
//! produced it.

use crate::error::{Error, Result};
use crate::lexer::Tok;

/// Byte span `[start, end)` into the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }
}

/// AST node. Numbers are split into `Int`/`Float` at parse time so the grammar
/// stays regular and downstream tooling never re-parses text.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Sym(String, Span),
    List(Vec<Node>, Span),
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Node::Int(_, s)
            | Node::Float(_, s)
            | Node::Str(_, s)
            | Node::Sym(_, s)
            | Node::List(_, s) => *s,
        }
    }
}

/// Max `(` nesting depth. Parsing recurses per level of nesting; this bound
/// keeps a maliciously/accidentally deep-nested source (e.g. thousands of
/// unmatched `(`) from overflowing the native stack before eval even runs.
const MAX_NEST_DEPTH: usize = 512;

pub fn parse(toks: &[Tok]) -> Result<Vec<Node>> {
    let mut pos = 0;
    let mut forms = Vec::new();
    while pos < toks.len() {
        forms.push(parse_form(toks, &mut pos, 0)?);
    }
    Ok(forms)
}

fn parse_form(toks: &[Tok], pos: &mut usize, depth: usize) -> Result<Node> {
    let tok = &toks[*pos];
    match tok {
        Tok::LParen(start) => {
            let start = *start;
            if depth >= MAX_NEST_DEPTH {
                return Err(Error::Parse {
                    msg: format!("nesting too deep (max {MAX_NEST_DEPTH} levels)"),
                    at: start,
                });
            }
            *pos += 1;
            let mut items = Vec::new();
            loop {
                match toks.get(*pos) {
                    None => {
                        return Err(Error::Parse {
                            msg: "unclosed '('".into(),
                            at: start,
                        })
                    }
                    Some(Tok::RParen(end)) => {
                        let end = *end + 1;
                        *pos += 1;
                        return Ok(Node::List(items, Span::new(start, end)));
                    }
                    Some(_) => items.push(parse_form(toks, pos, depth + 1)?),
                }
            }
        }
        Tok::RParen(at) => Err(Error::Parse {
            msg: "unexpected ')'".into(),
            at: *at,
        }),
        Tok::Str { text, start, end } => {
            *pos += 1;
            Ok(Node::Str(text.clone(), Span::new(*start, *end)))
        }
        Tok::Atom { text, start, end } => {
            *pos += 1;
            Ok(classify_atom(text, Span::new(*start, *end)))
        }
    }
}

/// Turn a bare atom's text into the right typed node. Anything that parses as a
/// number is a number; everything else is a symbol (including `true`/`false`/
/// `nil`, which the evaluator resolves).
fn classify_atom(text: &str, span: Span) -> Node {
    if let Ok(i) = text.parse::<i64>() {
        return Node::Int(i, span);
    }
    // Only treat as float if it contains a '.' or exponent, so tokens like
    // "1e" or "-" fall through to symbols rather than surprising parses.
    let looks_numeric = {
        let mut chars = text.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_digit() || ((c == '-' || c == '+' || c == '.') && text.len() > 1))
    };
    if looks_numeric && (text.contains('.') || text.contains('e') || text.contains('E')) {
        if let Ok(f) = text.parse::<f64>() {
            return Node::Float(f, span);
        }
    }
    Node::Sym(text.to_string(), span)
}
