//! Error type shared across the lexer, parser and evaluator.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Lexing failure (e.g. unterminated string) with a byte offset.
    Lex { msg: String, at: usize },
    /// Parsing failure (e.g. unbalanced parens).
    Parse { msg: String, at: usize },
    /// Runtime failure (unbound symbol, arity, type mismatch, user `error`).
    Runtime(String),
}

impl Error {
    pub fn runtime(msg: impl Into<String>) -> Error {
        Error::Runtime(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Lex { msg, at } => write!(f, "lex error at byte {at}: {msg}"),
            Error::Parse { msg, at } => write!(f, "parse error at byte {at}: {msg}"),
            Error::Runtime(msg) => write!(f, "runtime error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
