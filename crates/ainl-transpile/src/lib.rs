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
/// that is `import`: module resolution happens in the interpreter's load-time
/// loader, which evaluates a module in a fresh environment and passes the
/// resulting *values* to the interpreter. A transpiler emits a single source
/// file for a single language, and has no equivalent phase — the honest options
/// would be to inline a module's forms (changing its evaluation semantics) or to
/// emit a host-language import (which is a *different* feature, under a
/// different name, with different resolution rules).
///
/// This matters more than a normal unsupported-feature error. `import` is a
/// **keyword** in Python, Ruby and JavaScript, so an unhandled `(import "m")`
/// would lower to a call to the host's own import machinery — or, in Python,
/// `import("m")`, which parses and silently does something else entirely. A
/// program that transpiles to the wrong thing is far worse than one that
/// refuses, so the transpilers refuse, and `ainl_cc::generate` refuses for the
/// same program.
pub fn transpile(target: Target, forms: &[Node], src: &str) -> Result<String> {
    if let Some(at) = find_import(forms) {
        return Err(ainl_core::Error::runtime(format!(
            "ainl transpile --to {}: `import` is interpreter-only (found at byte {}) — \
             a transpiler emits one source file with no module-resolution phase, and \
             `import` is a {} keyword with different semantics. \
             Run the program with `ainl run` instead.",
            target.label(),
            at,
            target.label()
        )));
    }
    match target {
        Target::Python => transpile_python(forms, src),
        Target::JavaScript => transpile_js(forms, src),
        Target::Ruby => transpile_ruby(forms, src),
    }
}

/// The byte offset of the first `import` anywhere in `forms`, or `None`.
///
/// The search skips `quote`, whose contents are data and are never evaluated —
/// so `(quote (import "x"))` is legal data and must transpile normally. The
/// same predicate lives in `ainl_cc`, so the AOT backend and the three
/// transpilers refuse exactly the same programs.
fn find_import(forms: &[Node]) -> Option<usize> {
    for form in forms {
        let ainl_core::Node::List(items, _) = form else {
            continue;
        };
        if let Some(ainl_core::Node::Sym(head, _)) = items.first() {
            if head == ainl_core::import::IMPORT_SYM {
                return Some(form.span().start);
            }
            if head == "quote" {
                continue;
            }
        }
        // `()` is legal AINL — an empty `fn` parameter list, for one — so skip
        // the head only when there IS a head. Slicing from 1 on an empty list
        // panics, and the crash would be reachable from ordinary source.
        if !items.is_empty() {
            if let Some(at) = find_import(&items[1..]) {
                return Some(at);
            }
        }
    }
    None
}

/// Parse a source string and transpile it to the given target language.
pub fn transpile_src(target: Target, src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile(target, &forms, src)
}
