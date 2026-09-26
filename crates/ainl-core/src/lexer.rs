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
                        Some((_, '/')) => text.push('/'),
                        Some((_, other)) => {
                            // Only \" \\ \/ \n \r \t are valid (see the GBNF
                            // `string` rule). Anything else is a silent-data-
                            // corruption bug, so reject it explicitly.
                            return Err(Error::Lex {
                                msg: format!("invalid escape '\\{}'", other),
                                at: j,
                            });
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_str(src: &str) -> Result<String> {
        // Lex a single string literal and return its unescaped text.
        let toks = lex(src)?;
        match toks.into_iter().next() {
            Some(Tok::Str { text, .. }) => Ok(text),
            other => Err(Error::Lex {
                msg: format!("expected one Str token, got {:?}", other),
                at: 0,
            }),
        }
    }

    #[test]
    fn valid_escapes_unescape() {
        assert_eq!(lex_str(r#""a\nb""#).unwrap(), "a\nb");
        assert_eq!(lex_str(r#""a\tb""#).unwrap(), "a\tb");
        assert_eq!(lex_str(r#""a\rb""#).unwrap(), "a\rb");
        assert_eq!(lex_str(r#""a\\b""#).unwrap(), "a\\b");
        assert_eq!(lex_str(r#""a\"b""#).unwrap(), "a\"b");
        assert_eq!(lex_str(r#""a\/b""#).unwrap(), "a/b");
    }

    #[test]
    fn invalid_escape_is_rejected() {
        // \q is not a valid escape — must error, not silently become "q".
        let err = lex_str(r#""a\qb""#).unwrap_err();
        match err {
            Error::Lex { msg, .. } => assert!(msg.contains("invalid escape"), "{msg}"),
            other => panic!("expected Lex error, got {other:?}"),
        }
    }

    #[test]
    fn every_invalid_letter_escape_is_rejected() {
        // Only \" \\ \/ \n \r \t are valid; every other letter must error.
        // (n, r, t are valid — excluded from this set.)
        for ch in "abcdefghijklmopqsuvwxyzABCDEFGHIJKLMOPQSUVWXYZ".chars() {
            let src = format!(r#""a\{}b""#, ch);
            assert!(
                matches!(lex_str(&src), Err(Error::Lex { .. })),
                "escape \\{} should be rejected, but was accepted",
                ch
            );
        }
    }

    #[test]
    fn unterminated_escape_still_errors() {
        // A backslash as the very last character: the escape has no body.
        assert!(matches!(lex_str(r#""a\"#), Err(Error::Lex { .. })));
    }

    #[test]
    fn plain_string_unchanged() {
        assert_eq!(lex_str(r#""hello, world""#).unwrap(), "hello, world");
    }
}
