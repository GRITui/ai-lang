//! AINL transpiler plugins — project the AINL AST into traditional languages.
//!
//! Master plan §1.4: a modular plugin system so AINL programs interoperate with
//! existing codebases. Each plugin is an AST→source projection. This crate ships
//! the first target, **Python**; JavaScript and Ruby follow the same `Node`-in,
//! `String`-out shape.

pub mod python;

pub use python::{transpile_python, transpile_python_src};

/// The set of target languages a plugin can be selected by on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Python,
}

impl Target {
    pub fn from_name(name: &str) -> Option<Target> {
        match name {
            "python" | "py" => Some(Target::Python),
            _ => None,
        }
    }
}
