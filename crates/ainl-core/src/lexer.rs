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

/// The lexer's own view of a partial source: when is more input needed before
/// a text can be handed to the parser?
///
/// This is the scanner behind the REPL's multi-line continuation, so it is
/// deliberately written here next to [`lex`] rather than in the CLI: if the
/// lexer's idea of a string or a delimiter ever changes, the continuation
/// decision changes with it in the same commit instead of drifting.
///
/// Two constructs, and only two, can be open at end of input:
///
/// * **A string literal with no closing `"`.** A string may legally contain a
///   newline, so a line break does not close it.
/// * **A `(` with no matching `)`.** List nesting is the whole of AINL's
///   structure, and a form is routinely written over several lines.
///
/// What it deliberately does *not* do is judge balance in the other direction.
/// An *extra* `)` (`(+ 1 2))`) leaves the scan complete, so the parser gets to
/// report the real `unexpected ')'` — a REPL that waited for a `(` it will
/// never find would leave the user staring at a `…` prompt with no error at
/// all, which is the one failure mode a REPL must not have. Balanced-ness is
/// only ever used to decide "keep reading", never to decide "this is right".
pub fn scan(src: &str) -> ScanState {
    let mut chars = src.char_indices().peekable();
    // Byte offsets of the `(` that are still open, outermost first. A stack
    // rather than a depth counter because a stray `)` must not corrupt the
    // count: with a counter, `)) (+ 1` sits at depth −1 and the `(` that opens
    // a genuinely unfinished form is mistaken for a closing one. Popping an
    // empty stack is a no-op, so a stray `)` is simply ignored — which is the
    // parser's job to complain about, not ours.
    let mut open: Vec<usize> = Vec::new();
    let mut stray_close = false;
    while let Some(&(i, c)) = chars.peek() {
        if c == ';' {
            // A comment runs to end of line and can never continue one, so a
            // trailing comment must not make the REPL wait for a continuation.
            while let Some(&(_, c)) = chars.peek() {
                chars.next();
                if c == '\n' {
                    break;
                }
            }
        } else if c == '"' {
            chars.next(); // opening quote
            let string = 'outer: loop {
                match chars.next() {
                    Some((_, '"')) => break 'outer StringEnd::Closed,
                    // An escape consumes the next character unconditionally —
                    // including a newline, so `\"` never closes the string.
                    Some((_, '\\')) => {
                        if chars.next().is_none() {
                            // A `\` as the very last character has no body.
                            // This is deliberately reported as *complete* even
                            // though the string is open: the only character
                            // that could follow is the newline the next line
                            // would start with, and `\<newline>` is an invalid
                            // escape in every position. No future line can
                            // rescue it, so the user is shown the lexer's
                            // `unterminated escape` now instead of being
                            // invited to type a line that cannot help.
                            break 'outer StringEnd::DanglingEscape;
                        }
                    }
                    Some(_) => {}
                    None => break 'outer StringEnd::RanOffEnd,
                }
            };
            match string {
                StringEnd::Closed => {}
                StringEnd::RanOffEnd => {
                    return ScanState::Incomplete {
                        at: i,
                        why: IncompleteWhy::Str,
                    }
                }
                StringEnd::DanglingEscape => return ScanState::Complete { stray_close },
            }
        } else if c == '(' {
            open.push(i);
            chars.next();
        } else if c == ')' {
            if open.pop().is_none() {
                stray_close = true;
            }
            chars.next();
        } else {
            chars.next();
        }
    }
    match open.last() {
        // Point at the *innermost* still-open `(`, which is the one the user
        // has to add a `)` to; the enclosing ones are usually where they
        // started, so they are obvious.
        Some(&at) => ScanState::Incomplete {
            at,
            why: IncompleteWhy::Paren {
                depth: open.len() as i64,
            },
        },
        None => ScanState::Complete { stray_close },
    }
}

/// How a string literal ended when [`scan`] reached it: the three outcomes
/// that are not "keep reading", kept separate because only one of them is.
enum StringEnd {
    /// A closing `"` was found.
    Closed,
    /// Input ran out with the string still open — a continuation.
    RanOffEnd,
    /// A `\` was the last character, so the escape has no body and nothing the
    /// user can type on a later line can supply one.
    DanglingEscape,
}

/// Why a submission was judged incomplete, so the diagnostic can name the
/// construct that is still open instead of printing a bare `…`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteWhy {
    /// A string literal has no closing `"` yet.
    Str,
    /// A `(` is still open, at this many levels.
    Paren { depth: i64 },
}

/// Whether a partial REPL submission can be handed to the parser yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanState {
    /// Safe to parse. `stray_close` is true when a `)` closed more forms than
    /// were opened — information for the caller, which is free to ignore it;
    /// the parser will reject such a submission anyway.
    Complete { stray_close: bool },
    /// A string or a paren is still open; the REPL must read another line.
    /// `at` is the byte offset of the construct that started it.
    Incomplete { at: usize, why: IncompleteWhy },
}

impl ScanState {
    /// True when the REPL must read another line before evaluating.
    pub fn needs_more(self) -> bool {
        matches!(self, ScanState::Incomplete { .. })
    }
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

    // ---- scan() — the REPL's "do I need another line?" decision ----

    /// True when a source needs more input before it can be parsed.
    fn needs_more(src: &str) -> bool {
        scan(src).needs_more()
    }

    #[test]
    fn scan_says_a_balanced_form_is_complete() {
        assert!(!needs_more("(+ 1 2)"));
        assert!(!needs_more(""));
        assert!(!needs_more("   \n  "));
    }

    #[test]
    fn scan_says_an_unclosed_paren_needs_more() {
        // The REPL's headline case: a form spread over several lines.
        assert!(needs_more("(+ 1"));
        assert!(needs_more("(def fib (fn (n)"));
        // One more level than the previous line, and still incomplete.
        assert!(needs_more("(+ (list 1"));
    }

    #[test]
    fn scan_completes_once_the_paren_closes() {
        assert!(!needs_more("(+ 1\n 2)"));
    }

    #[test]
    fn scan_reports_the_depth_of_the_open_form() {
        // Two levels open, so the diagnostic can say how far the user is from
        // balance instead of just "incomplete".
        match scan("(+ (list 1") {
            ScanState::Incomplete {
                why: IncompleteWhy::Paren { depth },
                ..
            } => assert_eq!(depth, 2),
            other => panic!("expected an open paren, got {other:?}"),
        }
    }

    #[test]
    fn scan_points_at_the_opening_paren() {
        // The user has to look at the `(` to work out where to add the `)`.
        match scan("(+ 1\n  (+ 2") {
            ScanState::Incomplete { at, .. } => assert_eq!(at, 7, "the second `(`"),
            other => panic!("expected incomplete, got {other:?}"),
        }
    }

    #[test]
    fn scan_says_an_unclosed_string_needs_more() {
        // A string may legally span lines, so this must not be a parse error.
        assert!(needs_more("(+ \"abc"));
        assert!(needs_more("\""));
    }

    #[test]
    fn a_trailing_escape_is_not_a_continuation() {
        // `\` as the last character has no escape body, and the only character
        // that could follow is the newline the next line starts with — and
        // `\<newline>` is an invalid escape. No continuation can rescue it, so
        // the scanner reports *complete* and the user immediately gets the
        // lexer's real `unterminated escape` rather than being invited to type
        // a line that cannot help. The one exception: inside a string that is
        // *also* still open, the `\` is not the last character, because the
        // joining newline follows it.
        assert!(!needs_more("(print \"a\\"));
        // With the newline the REPL actually inserts, the escape is complete
        // and the string stays open — so this one does continue.
        assert!(needs_more("(print \"a\\\n"));
    }

    #[test]
    fn an_escaped_quote_does_not_close_the_string() {
        // `\"` is a quote *inside* the string: a naive quote count would close
        // it here and then report the real terminator as an error.
        assert!(needs_more(r#"(print "a\"b"#));
        assert!(!needs_more(r#"(print "a\"b")"#));
    }

    #[test]
    fn parens_inside_a_string_do_not_count() {
        assert!(!needs_more(r#"(print "(")"#));
        assert!(needs_more(r#"(print "("#));
    }

    #[test]
    fn a_comment_never_demands_a_continuation() {
        // A `;` comment runs to end of line and can never continue one, so a
        // trailing comment must submit immediately.
        assert!(!needs_more("(+ 1 2) ; done"));
        // Parens inside a comment are not delimiters, in either direction. A
        // `(` there does not open a form, and a `)` there does not close one —
        // so a `(` hidden in a comment leaves the real form open, and a `)` in
        // a comment cannot rescue it.
        assert!(needs_more("(+ 1 2 ; a stray ( in a comment"));
        assert!(needs_more("(+ 1 2 ; a stray ( and a stray ) in a comment"));
        // Only a real `)` balances the form; a comment is just noise to the
        // scanner, and it still ends at the newline.
        assert!(!needs_more("(+ 1 2) ; done\n"));
        assert!(needs_more("(+ 1 ; one\n"));
    }

    #[test]
    fn a_stray_close_paren_is_complete_not_incomplete() {
        // Deliberate: an extra `)` must reach the parser so the user gets the
        // real `unexpected ')'`. A REPL that waited for a `(` here would hang
        // on a `…` prompt forever with no error.
        assert!(!needs_more("(+ 1 2))"));
        assert!(!needs_more(")"));
        // But it is still *reported* as unbalanced, for a caller that cares.
        assert_eq!(scan("(+ 1 2))"), ScanState::Complete { stray_close: true });
        assert_eq!(scan("(+ 1 2)"), ScanState::Complete { stray_close: false });
    }

    #[test]
    fn parens_after_a_stray_close_still_balance() {
        // `)) (` is at depth 0 after the stray, so the `(` is a fresh open one
        // and the submission is genuinely incomplete.
        assert!(needs_more(")) (+ 1"));
    }

    #[test]
    fn scan_agrees_with_the_parser_on_a_well_formed_program() {
        // The strongest form of the property: for anything the parser accepts,
        // the scanner must never have claimed more input was needed. If the
        // two ever disagree, the REPL would refuse to evaluate a valid program.
        for src in [
            "(+ 1 2)",
            "(print \"a\")",
            "(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))",
            "(list 1 2 3)",
            r#"(print "(((")"#,
            "(print \"a\\\"b\")",
            ";; a comment\n(+ 1 2)",
        ] {
            assert!(crate::parse(src).is_ok(), "fixture should parse: {src}");
            assert!(!needs_more(src), "scanner should not ask for more: {src}");
        }
    }

    #[test]
    fn a_deeply_nested_open_form_still_reports_depth() {
        // The scanner must not overflow or cap before the parser does; the
        // parser's own MAX_NEST_DEPTH is the limit that matters.
        let src = "(".repeat(600);
        match scan(&src) {
            ScanState::Incomplete {
                why: IncompleteWhy::Paren { depth },
                at,
            } => {
                assert_eq!(depth, 600);
                // The innermost open paren — the last one typed.
                assert_eq!(at, 599);
            }
            other => panic!("expected incomplete, got {other:?}"),
        }
    }

    #[test]
    fn the_innermost_open_paren_is_the_one_reported() {
        // A user closing their form starts from the inside, so the diagnostic
        // must point at the `(` they last opened, not the outermost one.
        match scan("(def f (fn (x)") {
            ScanState::Incomplete { at, .. } => assert_eq!(at, 7),
            other => panic!("expected incomplete, got {other:?}"),
        }
    }
}
