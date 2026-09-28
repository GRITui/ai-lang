//! AINL transpiler plugins — project the AINL AST into traditional languages.
//!
//! Master plan §1.4: a modular plugin system so AINL programs interoperate with
//! existing codebases. Each plugin is an AST→source projection sharing the same
//! `Node`-in, `String`-out shape. Phase-1 rollout targets: **Python**,
//! **JavaScript**, **Ruby**.

pub mod js;
pub mod python;
pub mod ruby;
mod shared;

use ainl_core::parser::Node;
use ainl_core::Result;

pub use js::{transpile_js, transpile_js_src};
pub use python::{transpile_python, transpile_python_src};
pub use ruby::{transpile_ruby, transpile_ruby_src};

/// The target languages a plugin can be selected by on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Python,
    JavaScript,
    Ruby,
}

impl Target {
    pub fn from_name(name: &str) -> Option<Target> {
        match name {
            "python" | "py" => Some(Target::Python),
            "javascript" | "js" | "node" => Some(Target::JavaScript),
            "ruby" | "rb" => Some(Target::Ruby),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Python => "python",
            Target::JavaScript => "js",
            Target::Ruby => "ruby",
        }
    }
}

/// Transpile already-parsed forms to the given target language.
///
/// Returns `Err` for a program the transpiler cannot faithfully express. Today
/// that is the interpreter-only set (see [`ainl_core::interpreter_only`]):
///
/// - `import`, because module resolution happens in the interpreter's load-time
///   loader, which evaluates a module in a fresh environment and passes the
///   resulting *values* to the interpreter. A transpiler emits a single source
///   file for a single language and has no equivalent phase. Worse, `import` is
///   a **keyword** in Python, Ruby and JavaScript, so an unhandled
///   `(import "m")` would lower to a call to the host's own import machinery —
///   or, in Python, `import("m")`, which parses and silently does something
///   else entirely.
/// - `http-get`/`http-post`, because each host has its own HTTP library with
///   its own header, redirect, timeout and encoding behavior. Mapping onto them
///   would produce a transpiled program that builds cleanly and does something
///   subtly different from the interpreter's — the one failure mode a
///   four-backend language cannot have.
///
/// A program that transpiles to the wrong thing is far worse than one that
/// refuses, so the transpilers refuse, and `ainl_cc::generate` refuses for the
/// same programs.
pub fn transpile(target: Target, forms: &[Node], src: &str) -> Result<String> {
    if let Some((at, sym)) = ainl_core::interpreter_only::find_interpreter_only(forms) {
        return Err(ainl_core::Error::runtime(format!(
            "ainl transpile --to {}: `{sym}` is interpreter-only (found at byte {}) — \
             this backend emits one source file for one host language, which has no phase \
             that can resolve modules or speak AINL's HTTP semantics. \
             Run the program with `ainl run` instead.",
            target.label(),
            at,
        )));
    }
    match target {
        Target::Python => transpile_python(forms, src),
        Target::JavaScript => transpile_js(forms, src),
        Target::Ruby => transpile_ruby(forms, src),
    }
}

/// Parse a source string and transpile it to the given target language.
pub fn transpile_src(target: Target, src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile(target, &forms, src)
}
