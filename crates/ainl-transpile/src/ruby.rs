//! AINL AST → Ruby source projection. (Implemented below; see js.rs/python.rs
//! for the shared two-context model.)

use ainl_core::parser::Node;
use ainl_core::{Error, Result};

pub fn transpile_ruby_src(src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile_ruby(&forms, src)
}

pub fn transpile_ruby(_forms: &[Node], _src: &str) -> Result<String> {
    Err(Error::runtime("ruby target not yet implemented"))
}
