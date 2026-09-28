//! Error type shared across the lexer, parser and evaluator.
//!
//! # The model-readable contract
//!
//! Every error AINL reports answers three questions, in this order:
//!
//! 1. **What** went wrong — a short phrase naming the thing
//!    (`unbound symbol 'doble'`).
//! 2. **Where** it happened — a 1-based `at line N, col M` position pointing
//!    into the source, never a bare byte offset. A byte offset means nothing
//!    to a reader (or a model) looking at the text; a line and column can be
//!    found and acted on.
//! 3. **The likely fix**, when one can be inferred, appended after an em dash
//!    (`— did you mean 'double'?`, `— an expression was left unclosed`).
//!
//! The parts are carried as separate fields rather than baked into a string
//! so a consumer can render them differently and a test can assert on them
//! individually.
//!
//! # Why the position is resolved at the boundary
//!
//! Byte offsets are what every internal layer has; line/column needs the
//! source text, and the source is only in hand at the entry points
//! ([`crate::parse`], `run_str`, `run_in`, …). So an error records the byte
//! offset at the raise site and [`Error::locate`] fills in the line/column at
//! the boundary, exactly once. That keeps each of the ~100 raise sites a
//! one-liner and means a position is never guessed from partial state.
//!
//! The byte offset is retained even after a position is resolved: it is what
//! `ainl ast --json` emits as `span`, so a reader can correlate an error with
//! the serialized AST.
//!
//! # The 4-backend rule
//!
//! The interpreter, the bytecode VM, the AOT C runtime and the three
//! transpilers must agree byte-for-byte on stderr. That constrains the *text*
//! of every message, so message construction lives here and the other backends
//! reuse it rather than re-deriving their own wording. Where a backend
//! genuinely cannot know a position (the AOT C runtime compiles spans away
//! entirely) it says so explicitly rather than inventing a line number — see
//! docs/SYNTAX.md "Error messages".

use std::fmt;

use crate::serialize::LineIndex;

pub type Result<T> = std::result::Result<T, Error>;

/// A resolved source position: 1-based line and column.
///
/// Columns count **characters**, not bytes, so they line up with what a reader
/// (or an editor) counts. [`LineIndex`] is the single implementation of that
/// conversion, shared with the JSON serializer's `loc` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    pub line: usize,
    pub col: usize,
}

impl Loc {
    pub fn new(line: usize, col: usize) -> Loc {
        Loc { line, col }
    }
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Lexing failure (e.g. unterminated string).
    Lex {
        msg: String,
        at: usize,
        loc: Option<Loc>,
    },
    /// Parsing failure (e.g. unbalanced parens).
    Parse {
        msg: String,
        at: usize,
        loc: Option<Loc>,
    },
    /// JSON AST deserialization failure (malformed document).
    Json {
        msg: String,
        at: usize,
        loc: Option<Loc>,
    },
    /// Runtime failure (unbound symbol, arity, type mismatch, user `error`).
    ///
    /// `at` is the byte offset of the node that failed, when one is known.
    /// Resource limits (step/depth) are properties of the run rather than of
    /// any one node, so they carry `None` and print no position.
    Runtime {
        msg: String,
        at: Option<usize>,
        loc: Option<Loc>,
        suggest: Option<String>,
    },
}

impl Error {
    pub fn lex(msg: impl Into<String>, at: usize) -> Error {
        Error::Lex {
            msg: msg.into(),
            at,
            loc: None,
        }
    }

    pub fn parse(msg: impl Into<String>, at: usize) -> Error {
        Error::Parse {
            msg: msg.into(),
            at,
            loc: None,
        }
    }

    pub fn json(msg: impl Into<String>, at: usize) -> Error {
        Error::Json {
            msg: msg.into(),
            at,
            loc: None,
        }
    }

    /// A runtime error with no known position — the escape hatch for the
    /// resource-limit errors (step/depth), which are properties of the run
    /// rather than of any one node.
    pub fn runtime(msg: impl Into<String>) -> Error {
        Error::Runtime {
            msg: msg.into(),
            at: None,
            loc: None,
            suggest: None,
        }
    }

    /// A runtime error at a known source offset. The offset becomes a
    /// line/column once the source is available (see [`Error::locate`]).
    pub fn runtime_at(msg: impl Into<String>, at: usize) -> Error {
        Error::Runtime {
            msg: msg.into(),
            at: Some(at),
            loc: None,
            suggest: None,
        }
    }

    /// Attach a "did you mean …?" fix to a runtime error.
    pub fn with_suggestion(mut self, suggest: impl Into<String>) -> Error {
        if let Error::Runtime { suggest: slot, .. } = &mut self {
            *slot = Some(suggest.into());
        }
        self
    }

    /// The byte offset this error points at, if any.
    pub fn offset(&self) -> Option<usize> {
        match self {
            Error::Lex { at, .. } | Error::Parse { at, .. } | Error::Json { at, .. } => Some(*at),
            Error::Runtime { at, .. } => *at,
        }
    }

    /// Fill in a byte offset on an error that does not already have one.
    ///
    /// This is the backstop for builtins: `(abs "x")` fails inside a
    /// `Value::Builtin` that was handed only a `&[Value]` and has no access to
    /// the call form, so it cannot know where it was called from. The call
    /// boundary knows, and stamps the position on the way out. An error that
    /// already knows where it happened keeps its own position — a nested
    /// failure is always more precise than the call that led to it.
    ///
    /// A **resource-limit** error is deliberately left unpositioned. The step
    /// and depth limits are properties of the *run*, not of any one form:
    /// attributing `(while true 1)`'s step-limit failure to the `while` that
    /// started it would be a guess, and a plausible-looking wrong line is worse
    /// than none. (It would also make the tree-walk and the VM disagree, since
    /// the VM raises it from the run loop with no form in hand.)
    pub fn or_at(self, at: usize) -> Error {
        if self.is_resource_limit() {
            return self;
        }
        match self {
            Error::Lex { msg, at: own, loc } if own == 0 && loc.is_none() => {
                Error::Lex { msg, at, loc }
            }
            Error::Runtime {
                msg,
                at: None,
                loc,
                suggest,
            } => Error::Runtime {
                msg,
                at: Some(at),
                loc,
                suggest,
            },
            other => other,
        }
    }

    /// True for the two resource-limit errors: the step budget and the call
    /// depth. Both describe the run as a whole rather than a position in the
    /// source.
    pub fn is_resource_limit(&self) -> bool {
        let Error::Runtime { msg, .. } = self else {
            return false;
        };
        msg.starts_with("step limit exceeded") || msg.starts_with("recursion limit exceeded")
    }

    /// The resolved line/column, if the error has been through [`Error::locate`].
    pub fn location(&self) -> Option<Loc> {
        match self {
            Error::Lex { loc, .. }
            | Error::Parse { loc, .. }
            | Error::Json { loc, .. }
            | Error::Runtime { loc, .. } => *loc,
        }
    }

    /// The inferred fix, when one is attached (runtime errors only).
    pub fn suggestion(&self) -> Option<&str> {
        match self {
            Error::Runtime { suggest, .. } => suggest.as_deref(),
            _ => None,
        }
    }

    /// The description of what went wrong, without the position or the fix.
    pub fn message(&self) -> &str {
        match self {
            Error::Lex { msg, .. } | Error::Parse { msg, .. } | Error::Json { msg, .. } => msg,
            Error::Runtime { msg, .. } => msg,
        }
    }

    /// Resolve this error's byte offset into a line/column against `src`.
    ///
    /// This is the single place a position becomes reader-facing, and it runs
    /// at the entry points that still hold the source. An error with no offset
    /// (a resource-limit failure) passes through unchanged.
    pub fn located_in(self, src: &str) -> Error {
        let Some(at) = self.offset() else {
            return self;
        };
        let (line, col) = LineIndex::new(src).locate(at);
        let loc = Loc::new(line, col);
        match self {
            Error::Lex { msg, at, .. } => Error::Lex {
                msg,
                at,
                loc: Some(loc),
            },
            Error::Parse { msg, at, .. } => Error::Parse {
                msg,
                at,
                loc: Some(loc),
            },
            Error::Json { msg, at, .. } => Error::Json {
                msg,
                at,
                loc: Some(loc),
            },
            Error::Runtime {
                msg, at, suggest, ..
            } => Error::Runtime {
                msg,
                at,
                loc: Some(loc),
                suggest,
            },
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Lex { msg, at, loc } => {
                write!(f, "lex error: {msg}")?;
                write_pos(f, *at, *loc)
            }
            Error::Parse { msg, at, loc } => {
                write!(f, "parse error: {msg}")?;
                write_pos(f, *at, *loc)
            }
            Error::Json { msg, at, loc } => {
                write!(f, "json error: {msg}")?;
                write_pos(f, *at, *loc)
            }
            Error::Runtime { msg, at, loc, .. } => {
                write!(f, "runtime error: {msg}")?;
                if let Some(l) = loc {
                    write!(f, " at {l}")?;
                }
                if let Some(at) = at {
                    write!(f, " (byte {at})")?;
                }
                if let Some(s) = self.suggestion() {
                    write!(f, " — did you mean '{s}'?")?;
                }
                Ok(())
            }
        }
    }
}

/// Write the position fragment: ` at line N, col M (byte B)` when a line index
/// resolved it, and ` at byte B` when it could not. The byte offset is always
/// retained, so a message never loses information the byte-only format had.
fn write_pos(f: &mut fmt::Formatter<'_>, at: usize, loc: Option<Loc>) -> fmt::Result {
    match loc {
        Some(l) => write!(f, " at {l} (byte {at})"),
        None => write!(f, " at byte {at}"),
    }
}

impl std::error::Error for Error {}
