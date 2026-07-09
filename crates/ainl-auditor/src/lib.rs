//! Phase 2 — the multi-SLM orchestrated Prompt Auditor.
//!
//! Compiles vague natural language into a strict, grammar-validated AINL schema
//! through a five-stage local-model pipeline (master plan §2). The Auditor stage
//! emits AINL under the GBNF grammar exported by `ainl-core` and validates it
//! with the real parser, so the pipeline's output is guaranteed to parse.
//!
//! ```no_run
//! use ainl_auditor::{Auditor, MockBackend};
//! let backend = MockBackend;
//! let report = Auditor::new(&backend).run("sum a list of numbers").unwrap();
//! assert!(report.ainl_valid);
//! println!("{}", report.context_prompt);
//! ```

pub mod backend;
pub mod context;
pub mod pipeline;
pub mod role;

pub use backend::{Backend, GenRequest, HttpBackend, MockBackend};
pub use context::{ContextProvider, FsContextProvider, McpContextProvider};
pub use pipeline::{AuditReport, Auditor, StageOutput};
pub use role::Role;
