//! AINL — the AI-Native Language core.
//!
//! Pipeline: source `&str` -> [`lexer`] tokens -> [`parser`] AST ([`Node`]) ->
//! [`eval`] runtime [`Value`]. Zero external dependencies so the runtime can be
//! packaged as a static, zero-dependency binary (see docs/MASTER_PLAN.md §1.2).

pub mod code;
pub mod collection_forms;
pub mod collections;
pub mod deserialize;
pub mod error;
pub mod eval;
pub mod grammar;
pub mod http;
pub mod import;
pub mod interpreter_only;
pub mod json_value;
pub mod lexer;
pub mod parser;
pub mod pkg;
pub mod serialize;
pub mod suggest;
pub mod testing;
pub mod value;
pub mod vm;

pub use deserialize::json_to_forms;
pub use error::{Error, Result};
pub use eval::Env;
pub use grammar::{Dialect, GBNF};
pub use import::Executor;
pub use parser::{Node, Span};
pub use serialize::{forms_to_json, LineIndex};
pub use value::{ConsCell, Value};

/// Parse a source string into the AST (a sequence of top-level forms).
///
/// Errors carry a resolved line/column as well as the raw byte offset, so a
/// reader (or a model) is told where the problem is in terms of the text.
pub fn parse(src: &str) -> Result<Vec<Node>> {
    let toks = lexer::lex(src).map_err(|e| e.located_in(src))?;
    parser::parse(&toks).map_err(|e| e.located_in(src))
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

/// Run a *named* program file on the bytecode VM, with `import` resolution
/// relative to the file's own directory.
///
/// This is the entry point `ainl run` uses. A program is `import`ed by
/// *location*, so the file's directory is part of its meaning: running the same
/// text with [`run_str`] resolves its imports against the process working
/// directory instead, and a program that works with one may not with the other.
/// [`run_named_in`] is the same thing in an existing environment, which is what
/// the REPL needs.
///
/// Imports are resolved *before* any of the program's own forms run, and a
/// module's bindings are defined into `env` first, so a `def` in the importing
/// file always wins over an imported name of the same spelling — except that
/// this is enforced as an error up front rather than silently (see
/// `import`'s collision rule), so in practice the two cannot disagree.
pub fn run_named_in(src: &str, path: &std::path::Path, env: &Env) -> Result<Value> {
    run_named_with(src, path, env, import::Executor::Vm)
}

/// The tree-walking twin of [`run_named_in`], for differential testing: same
/// file, same environment, same modules — evaluated by the other interpreter.
/// This is what makes "both interpreters agree about `import`" a testable claim
/// rather than an assertion.
pub fn run_named_tree_walk_in(src: &str, path: &std::path::Path, env: &Env) -> Result<Value> {
    run_named_with(src, path, env, import::Executor::TreeWalk)
}

fn run_named_with(
    src: &str,
    path: &std::path::Path,
    env: &Env,
    executor: import::Executor,
) -> Result<Value> {
    let forms = parse(src)?;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let loader = import::Loader::new(executor);
    let prepared = import::prepare(&forms, src, dir, &loader)?;
    for (name, value) in prepared.names {
        env.define(name, value);
    }
    let result = match executor {
        import::Executor::Vm => vm::run_forms(&prepared.forms, env),
        import::Executor::TreeWalk => eval::run_forms(&prepared.forms, env),
    };
    result.map_err(|e| e.located_in(src))
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
    tree_walk_in(src, &env)
}

/// Tree-walking twin of [`vm::run_in`], for differential testing: same program,
/// same existing environment, so a REPL's two evaluation paths can be compared
/// on identical input.
pub fn tree_walk_in(src: &str, env: &Env) -> Result<Value> {
    eval::reset_limits();
    let forms = parse(src)?;
    eval::run_forms(&forms, env).map_err(|e| e.located_in(src))
}
