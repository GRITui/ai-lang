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
/// - `db-open`/`db-put`/`db-get`/`db-flush`/`db-close`, because the storage
///   engine's contract *is* the file format: a header, an append-only log, and
///   a CRC per record so a torn tail from a crash is rejected on replay. A
///   host `open()` cannot append to a file another writer may also hold, has
///   no log to replay, and no way to reproduce the recovery — and the three
///   hosts disagree about durability (`fsync` vs `fdatasync` vs nothing at
///   all). A transpiled database that silently diverged from the interpreter's
///   would be worse than no database, so these refuse too.
///
/// The AOT C backend does **not** fall in that last group: it carries a
/// hand-port of the engine in its own runtime, so a compiled AINL binary opens
/// the same `.ainl-db` and recovers from the same torn tail. That asymmetry is
/// why the restricted set is a table keyed by backend rather than one list —
/// see [`ainl_core::interpreter_only::RESTRICTED`].
///
/// A program that transpiles to the wrong thing is far worse than one that
/// refuses, so the transpilers refuse, and `ainl_cc::generate` refuses for the
/// programs that *it* cannot run.
pub fn transpile(target: Target, forms: &[Node], src: &str) -> Result<String> {
    if let Some((at, sym)) = ainl_core::interpreter_only::find_restricted(
        forms,
        ainl_core::interpreter_only::Backend::Transpilers,
    ) {
        // The reason differs by symbol, and the label differs by target. Saying
        // "interpreter-only" for `db-open` would be a lie in both directions:
        // `ainl compile` runs it, and the message used to point at `--to
        // python` even for `--to ruby`.
        let (what, because) = match sym {
            ainl_core::db::DB_OPEN
            | ainl_core::db::DB_PUT
            | ainl_core::db::DB_GET
            | ainl_core::db::DB_FLUSH
            | ainl_core::db::DB_CLOSE => (
                "transpiler-only",
                "this backend emits one source file for one host language, and a host \
                 file API has no append-only log, no per-record checksum, and no \
                 crash-tail recovery — a program that ran here would read a different \
                 file than the one the interpreter wrote",
            ),
            _ => (
                "interpreter-only",
                "this backend emits one source file for one host language, which has no \
                 phase that can resolve modules or speak AINL's HTTP semantics",
            ),
        };
        return Err(ainl_core::Error::runtime(format!(
            "ainl transpile --to {}: `{sym}` is {what} (found at byte {at}) — {because}. \
             Run the program with `ainl run` instead, or `ainl compile` for the AOT C binary.",
            target.label(),
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
