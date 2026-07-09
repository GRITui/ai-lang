//! External context providers (master plan §2.3).
//!
//! Before compiling the final prompt, the auditor can pull real-time context
//! from external sources — the local filesystem, enterprise databases, or MCP
//! servers / third-party routers. Each source implements [`ContextProvider`];
//! the pipeline queries them with the user's request and folds the results into
//! the Planner's prompt.

use ainl_core::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub trait ContextProvider {
    fn name(&self) -> &str;
    /// Return context relevant to `query`, or an empty string if none.
    fn fetch(&self, query: &str) -> Result<String>;
}

/// Pulls context from the local filesystem: reads text files under `root` and
/// returns snippets whose content mentions any word from the query.
pub struct FsContextProvider {
    name: String,
    root: PathBuf,
    max_bytes: usize,
}

impl FsContextProvider {
    pub fn new(root: impl AsRef<Path>) -> FsContextProvider {
        FsContextProvider {
            name: format!("fs:{}", root.as_ref().display()),
            root: root.as_ref().to_path_buf(),
            max_bytes: 2000,
        }
    }
}

impl ContextProvider for FsContextProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch(&self, query: &str) -> Result<String> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            .map(|w| w.to_lowercase())
            .collect();
        let entries = fs::read_dir(&self.root)
            .map_err(|e| Error::runtime(format!("cannot read {}: {e}", self.root.display())))?;
        let mut out = String::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let lower = content.to_lowercase();
            if terms.iter().any(|t| lower.contains(t)) {
                let snippet: String = content.chars().take(self.max_bytes).collect();
                out.push_str(&format!("### {}\n{}\n\n", path.display(), snippet));
            }
        }
        Ok(out)
    }
}

/// Placeholder for the MCP / third-party-router hook (§2.3). Wiring a live MCP
/// client goes here; the trait boundary is what the pipeline depends on.
pub struct McpContextProvider {
    pub server: String,
}

impl ContextProvider for McpContextProvider {
    fn name(&self) -> &str {
        &self.server
    }

    fn fetch(&self, _query: &str) -> Result<String> {
        Err(Error::runtime(format!(
            "MCP provider '{}' not yet wired — implement fetch() to call the server",
            self.server
        )))
    }
}
