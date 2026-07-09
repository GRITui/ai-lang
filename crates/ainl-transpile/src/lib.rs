//! AINL transpiler plugins — project the AINL AST into traditional languages.
//!
//! Master plan §1.4: a modular plugin system so AINL programs interoperate with
//! existing codebases. Each plugin is an AST→source projection sharing the same
//! `Node`-in, `String`-out shape. Phase-1 rollout targets: **Python**,
//! **JavaScript**, **Ruby**.

pub mod js;
pub mod python;
pub mod ruby;

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
pub fn transpile(target: Target, forms: &[Node], src: &str) -> Result<String> {
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
