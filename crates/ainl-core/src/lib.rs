//! AINL — the AI-Native Language core.
//!
//! Pipeline: source `&str` -> [`lexer`] tokens -> [`parser`] AST ([`Node`]) ->
//! [`eval`] runtime [`Value`]. Zero external dependencies so the runtime can be
//! packaged as a static, zero-dependency binary (see docs/MASTER_PLAN.md §1.2).

pub mod code;
pub mod deserialize;
pub mod error;
pub mod eval;
pub mod grammar;
pub mod lexer;
pub mod parser;
pub mod serialize;
pub mod value;
pub mod vm;

pub use deserialize::json_to_forms;
pub use error::{Error, Result};
pub use eval::Env;
pub use grammar::{Dialect, GBNF};
pub use parser::{Node, Span};
pub use serialize::{forms_to_json, LineIndex};
pub use value::{ConsCell, Value};

/// Parse a source string into the AST (a sequence of top-level forms).
pub fn parse(src: &str) -> Result<Vec<Node>> {
    let toks = lexer::lex(src)?;
    parser::parse(&toks)
}

/// Parse a program and serialize its AST to stable JSON (with source-map
/// `span`/`loc` on every node). `source_name` is embedded when provided.
pub fn parse_to_json(src: &str, source_name: Option<&str>) -> Result<String> {
    let forms = parse(src)?;
    Ok(forms_to_json(&forms, src, source_name))
}

/// Parse and evaluate a whole program in a fresh environment with the prelude
/// loaded. Returns the value of the final form. Runs on the bytecode VM.
pub fn run_str(src: &str) -> Result<Value> {
    let env = Env::with_prelude();
    run_in(src, &env)
}

/// Parse and evaluate a program in an existing environment (used by the REPL so
/// bindings persist across lines). Runs on the bytecode VM. Each call gets a
/// fresh step budget (the VM's step counter is local to the run) so runaway
/// work on one line/run can't starve the next.
pub fn run_in(src: &str, env: &Env) -> Result<Value> {
    vm::run_in(src, env)
}

/// Parse and evaluate a program using the tree-walking evaluator (the semantic
/// reference and fallback path). Runs in a fresh preloaded environment, so it
/// is a drop-in comparison target for [`run_str`]. Exposed for differential
/// testing against the VM. Each call gets a fresh step budget (see
/// `eval::MAX_STEPS`).
pub fn run_in_tree_walk(src: &str) -> Result<Value> {
    let env = Env::with_prelude();
    eval::reset_limits();
    let forms = parse(src)?;
    let mut last = Value::Nil;
    for form in &forms {
        last = eval::eval(form, &env)?;
    }
    Ok(last)
}
