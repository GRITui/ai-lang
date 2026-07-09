//! Model backends: how a role's prompt gets turned into text.
//!
//! Two implementations:
//! * [`MockBackend`] — deterministic, offline. Lets the whole 5-stage pipeline
//!   run and be tested without any models present. Its Auditor output is always
//!   valid AINL so downstream stages exercise the real parser.
//! * [`HttpBackend`] — talks to a local model server (Ollama / llama.cpp-style
//!   `/api/generate`) via `curl`, passing the GBNF grammar for the Auditor's
//!   grammar-constrained decoding (§2.1). Used for real runs (M7).

use crate::role::Role;
use ainl_core::{Error, Result};
use std::process::Command;

/// A single generation request for one role.
pub struct GenRequest<'a> {
    pub role: Role,
    pub system: &'a str,
    /// The original natural-language request (stable across all stages).
    pub request: &'a str,
    /// The stage-specific prompt (already includes upstream context).
    pub prompt: &'a str,
    /// GBNF grammar for constrained decoding (Auditor stage only).
    pub grammar: Option<&'a str>,
}

pub trait Backend {
    fn generate(&self, req: &GenRequest) -> Result<String>;
    fn describe(&self) -> String;
}

// ---- Mock ------------------------------------------------------------------

/// Offline, deterministic backend. Produces role-appropriate output derived
/// from the request so the pipeline is fully runnable without models.
pub struct MockBackend;

impl Backend for MockBackend {
    fn describe(&self) -> String {
        "mock (offline, deterministic)".to_string()
    }

    fn generate(&self, req: &GenRequest) -> Result<String> {
        let intent = one_line(req.request);
        Ok(match req.role {
            Role::Orchestrator => format!("intent: {intent}\nroute: code"),
            Role::Planner => format!(
                "1. Parse the request: {intent}\n2. Identify inputs and outputs\n\
                 3. Handle the empty / boundary case\n4. Produce the result"
            ),
            // Must be valid AINL — this is what the real parser validates.
            Role::Auditor => format!(
                "; audited intent\n(def task (fn ()\n  (print \"{}\")))\n(task)",
                ainl_string_escape(&intent)
            ),
            Role::CodeEngine => format!(
                "; final AINL\n(def main (fn ()\n  (print \"{}\")))\n(main)",
                ainl_string_escape(&intent)
            ),
            Role::Generalist => format!(
                "Here's what your request compiles to: a small program that {intent}. \
                 Paste the audited AINL prompt into any model to generate it in one shot."
            ),
        })
    }
}

// ---- HTTP (local model server) --------------------------------------------

/// Talks to a local model server via `curl`. `endpoint` defaults to Ollama's
/// `http://localhost:11434/api/generate`.
pub struct HttpBackend {
    pub endpoint: String,
}

impl HttpBackend {
    pub fn new(endpoint: impl Into<String>) -> HttpBackend {
        HttpBackend {
            endpoint: endpoint.into(),
        }
    }

    pub fn ollama() -> HttpBackend {
        HttpBackend::new("http://localhost:11434/api/generate")
    }
}

impl Backend for HttpBackend {
    fn describe(&self) -> String {
        format!("http ({})", self.endpoint)
    }

    fn generate(&self, req: &GenRequest) -> Result<String> {
        // Build the request JSON by hand (no serde dependency).
        let mut json = String::from("{");
        json.push_str(&format!("\"model\":{},", json_string(req.role.model())));
        json.push_str(&format!("\"system\":{},", json_string(req.system)));
        json.push_str(&format!("\"prompt\":{},", json_string(req.prompt)));
        json.push_str("\"stream\":false");
        if let Some(g) = req.grammar {
            // llama.cpp-style grammar field; harmless if the server ignores it.
            json.push_str(&format!(",\"grammar\":{}", json_string(g)));
        }
        json.push('}');

        let out = Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                &self.endpoint,
                "-H",
                "Content-Type: application/json",
                "-d",
                &json,
            ])
            .output()
            .map_err(|e| Error::runtime(format!("failed to run curl: {e}")))?;
        if !out.status.success() {
            return Err(Error::runtime(format!(
                "model server request failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let body = String::from_utf8_lossy(&out.stdout);
        // Ollama returns {"response":"..."}; llama.cpp returns {"content":"..."}.
        extract_json_string(&body, "response")
            .or_else(|| extract_json_string(&body, "content"))
            .ok_or_else(|| Error::runtime(format!("could not parse model response: {body}")))
    }
}

// ---- helpers ---------------------------------------------------------------

fn one_line(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let line = line.trim_start_matches("intent:").trim();
    if line.len() > 120 {
        format!("{}…", &line[..120])
    } else {
        line.to_string()
    }
}

/// Escape a string for embedding inside an AINL `"..."` literal.
fn ainl_string_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

fn json_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Minimal extractor for a top-level string value `"key":"..."` from a JSON
/// body, handling backslash escapes. Sufficient for single-object model
/// responses; not a general JSON parser.
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let mut chars = after.char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for (_, c) in chars {
        if escaped {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}
