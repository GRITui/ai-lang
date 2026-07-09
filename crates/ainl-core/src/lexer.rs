//! Tokenizer for AINL.
//!
//! The grammar is deliberately tiny (see docs/SYNTAX.md): parentheses group
//! forms, everything else is either a string literal or a bare atom. `;` starts
//! a comment that runs to end of line. Whitespace is insignificant beyond
//! separating atoms.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    LParen(usize),
    RParen(usize),
    /// A bare atom (number / symbol / bool / nil) with its source span.
    Atom {
        text: String,
        start: usize,
        end: usize,
    },
    /// A string literal (already unescaped) with the span of the full literal
    /// including quotes.
    Str {
        text: String,
        start: usize,
        end: usize,
    },
}

impl Tok {
    pub fn start(&self) -> usize {
        match self {
            Tok::LParen(p) | Tok::RParen(p) => *p,
            Tok::Atom { start, .. } | Tok::Str { start, .. } => *start,
        }
    }
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || c == '(' || c == ')' || c == ';' || c == '"'
}

pub fn lex(src: &str) -> Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let bytes = src.as_bytes();
    let mut chars = src.char_indices().peekable();

    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == ';' {
            // comment to end of line
            while let Some(&(_, c)) = chars.peek() {
                chars.next();
                if c == '\n' {
                    break;
                }
            }
        } else if c == '(' {
            toks.push(Tok::LParen(i));
            chars.next();
        } else if c == ')' {
            toks.push(Tok::RParen(i));
            chars.next();
        } else if c == '"' {
            let start = i;
            chars.next(); // opening quote
            let mut text = String::new();
            let mut closed = false;
            while let Some((j, ch)) = chars.next() {
                match ch {
                    '"' => {
                        toks.push(Tok::Str {
                            text: std::mem::take(&mut text),
                            start,
                            end: j + 1,
                        });
                        closed = true;
                        break;
                    }
                    '\\' => match chars.next() {
                        Some((_, 'n')) => text.push('\n'),
                        Some((_, 't')) => text.push('\t'),
                        Some((_, 'r')) => text.push('\r'),
                        Some((_, '\\')) => text.push('\\'),
                        Some((_, '"')) => text.push('"'),
                        Some((_, other)) => text.push(other),
                        None => {
                            return Err(Error::Lex {
                                msg: "unterminated escape".into(),
                                at: j,
                            })
                        }
                    },
                    _ => text.push(ch),
                }
            }
            if !closed {
                return Err(Error::Lex {
                    msg: "unterminated string".into(),
                    at: start,
                });
            }
        } else {
            // bare atom: read until a delimiter
            let start = i;
            let mut end = i;
            while let Some(&(j, ch)) = chars.peek() {
                if is_delim(ch) {
                    break;
                }
                end = j + ch.len_utf8();
                chars.next();
            }
            let text = std::str::from_utf8(&bytes[start..end])
                .map_err(|_| Error::Lex {
                    msg: "invalid utf-8".into(),
                    at: start,
                })?
                .to_string();
            toks.push(Tok::Atom { text, start, end });
        }
    }
    Ok(toks)
}
