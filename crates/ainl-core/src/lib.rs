//! AINL — the AI-Native Language core.
//!
//! Pipeline: source `&str` -> [`lexer`] tokens -> [`parser`] AST ([`Node`]) ->
//! [`eval`] runtime [`Value`]. Zero external dependencies so the runtime can be
//! packaged as a static, zero-dependency binary (see docs/MASTER_PLAN.md §1.2).

pub mod error;
pub mod lexer;
pub mod parser;
pub mod value;
pub mod eval;

pub use error::{Error, Result};
pub use eval::Env;
pub use parser::{Node, Span};
pub use value::Value;

/// Parse a source string into the AST (a sequence of top-level forms).
pub fn parse(src: &str) -> Result<Vec<Node>> {
    let toks = lexer::lex(src)?;
    parser::parse(&toks)
}

/// Parse and evaluate a whole program in a fresh environment with the prelude
/// loaded. Returns the value of the final form.
pub fn run_str(src: &str) -> Result<Value> {
    let env = Env::with_prelude();
    run_in(src, &env)
}

/// Parse and evaluate a program in an existing environment (used by the REPL so
/// bindings persist across lines).
pub fn run_in(src: &str, env: &Env) -> Result<Value> {
    let forms = parse(src)?;
    let mut last = Value::Nil;
    for form in &forms {
        last = eval::eval(form, env)?;
    }
    Ok(last)
}
