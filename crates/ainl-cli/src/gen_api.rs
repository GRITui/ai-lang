//! The model call: a small, dependency-free OpenAI-compatible client.
//!
//! # Backend-agnostic by construction
//!
//! Nothing here names a provider. The endpoint, the model and the key all come
//! from flags or the environment, and the one place a vendor's *field name*
//! matters — the request member that carries a decoding grammar — is a
//! configurable path rather than a constant, because there is no standard for
//! it (see [`GrammarField`]).
//!
//! The whole workspace is zero-dependency on purpose (docs/MASTER_PLAN.md
//! §1.2), and this module is where that rule is most load-bearing: an HTTP
//! client is exactly the kind of thing that drags in a TLS stack, and
//! `docs/HTTP_TLS.md` already records why AINL refuses to take that on inside
//! the language. So rather than adding a crate, the CLI shells out to `curl`.
//! That is a real trade — a missing `curl` becomes a runtime dependency of one
//! subcommand — and it is the right one here: it keeps the four-backend
//! guarantee, keeps the AOT binary standalone, and adds zero lines of vendored
//! crypto. `ainl doctor` reports a missing `curl` as SKIP, not FAIL, for the
//! same reason it treats a missing `cc` that way.
//!
//! # The key never touches argv
//!
//! Every process on a machine can read another process's command line, so a
//! key passed as `curl -H "Authorization: Bearer …"` is readable by any local
//! user for the lifetime of the call, and lands in shell history. The key
//! therefore goes to curl on **stdin** via `--config -`, and `--data-binary`
//! takes the body from a temp file rather than argv. Nothing sensitive is ever
//! an argument, and the key is never logged, echoed, or written to any output
//! file.
//!
//! # What this module does *not* decide
//!
//! Whether constrained decoding actually took effect is not an API-client
//! question — the failure is silent (HTTP 200, valid prose, constraint
//! dropped), so it has to be *tested*. That lives in [`crate::gen`], which
//! pairs the probe here with a real GBNF membership check on the result.

use ainl_core::value::Value;
use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::rc::Rc;

/// A failure with a message a human can act on.
#[derive(Debug)]
pub struct ApiError(pub String);

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ApiError {
    fn from(s: String) -> ApiError {
        ApiError(s)
    }
}

impl From<&str> for ApiError {
    fn from(s: &str) -> ApiError {
        ApiError(s.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// The environment variable consulted for the API key, and the default.
///
/// The key is *named* by configuration, never carried in it. `--api-key-env`
/// points at a different variable when a host keeps its credentials under its
/// own name, which is the common case: this project's own gateway credential,
/// for instance, lives in `HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY`.
pub const DEFAULT_KEY_ENV: &str = "AINL_GEN_API_KEY";

/// Default endpoint and model, overridable by `AINL_GEN_ENDPOINT` /
/// `AINL_GEN_MODEL`. Neither is a provider choice — a default that is wrong
/// for a given host is corrected by pointing the env var at it.
pub const DEFAULT_ENDPOINT: &str = "https://gateway.9arm.co";
pub const DEFAULT_MODEL: &str = "qwen3.8-27b-fp8";

/// The request member that carries a GBNF grammar.
///
/// There is no standard, and the two plausible spellings behave completely
/// differently on vLLM (docs/GENERATION.md): `guided_grammar` returns HTTP 200
/// and is *silently ignored*, while `structured_outputs.grammar` genuinely
/// constrains. A client that hardcoded either one would work against one
/// backend and quietly measure nothing against the other, so the path is
/// configuration and the *effect* is verified at runtime either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarField {
    /// `structured_outputs.grammar` — vLLM, and what the shipped harness uses.
    StructuredOutputs,
    /// `grammar` — llama.cpp's server, and several proxies.
    Grammar,
    /// `guided_grammar` — the silently-ignored one, kept only so a user can
    /// point at it deliberately (and see the probe reject it).
    GuidedGrammar,
    /// Send no grammar at all.
    None,
}

impl GrammarField {
    /// Parse the configured spelling: a dotted path, or `none` to disable.
    pub fn parse(spec: &str) -> ApiResult<GrammarField> {
        match spec.trim() {
            "none" | "off" => Ok(GrammarField::None),
            "structured_outputs.grammar" | "structured_outputs" => {
                Ok(GrammarField::StructuredOutputs)
            }
            "grammar" => Ok(GrammarField::Grammar),
            "guided_grammar" => Ok(GrammarField::GuidedGrammar),
            other => Err(ApiError(format!(
                "unknown --grammar-field '{other}'\n  \
                 use structured_outputs.grammar (vLLM), grammar (llama.cpp server), \
                 guided_grammar (usually ignored), or none"
            ))),
        }
    }

    /// The request path this field writes to, or `None` for no constraint.
    pub fn path(self) -> Option<&'static [&'static str]> {
        match self {
            GrammarField::StructuredOutputs => Some(&["structured_outputs", "grammar"]),
            GrammarField::Grammar => Some(&["grammar"]),
            GrammarField::GuidedGrammar => Some(&["guided_grammar"]),
            GrammarField::None => None,
        }
    }

    /// The spelling used in the trace, so a reader can see what was sent.
    pub fn label(self) -> &'static str {
        match self {
            GrammarField::StructuredOutputs => "structured_outputs.grammar",
            GrammarField::Grammar => "grammar",
            GrammarField::GuidedGrammar => "guided_grammar",
            GrammarField::None => "none",
        }
    }
}

/// A configured, ready-to-call backend.
#[derive(Debug, Clone)]
pub struct Backend {
    /// The full chat-completions URL, already normalized.
    pub url: String,
    pub model: String,
    /// Never printed. Held so a caller can report *that* a key was found
    /// without reporting what it is.
    pub api_key: String,
    pub grammar_field: GrammarField,
    pub timeout_secs: u32,
    pub temperature: f64,
    pub max_tokens: u32,
    /// Vendor-specific request members merged into every call, e.g.
    /// `{"chat_template_kwargs":{"enable_thinking":false}}`. The escape hatch
    /// that keeps a reasoning model's `enable_thinking` out of the client.
    pub extra: Option<Value>,
}

impl Backend {
    /// Build a backend, normalizing the endpoint into a full URL.
    ///
    /// Accepts a base (`https://host`), a v1 root (`https://host/v1`) or a
    /// complete chat-completions URL, because all three are things a person
    /// reasonably has on hand and guessing wrong should not be a confusing
    /// 404.
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: String,
        grammar_field: GrammarField,
        timeout_secs: u32,
    ) -> Backend {
        let e = endpoint.trim().trim_end_matches('/');
        let url = if e.ends_with("/chat/completions") {
            e.to_string()
        } else if e.ends_with("/v1") {
            format!("{e}/chat/completions")
        } else {
            format!("{e}/v1/chat/completions")
        };
        Backend {
            url,
            model: model.to_string(),
            api_key,
            grammar_field,
            timeout_secs,
            temperature: 0.0,
            max_tokens: 1200,
            extra: None,
        }
    }

    /// A one-line description safe to print: no key, no credentials.
    pub fn describe(&self) -> String {
        format!("{} · model={}", self.url, self.model)
    }
}

/// What the backend returned for one completion.
#[derive(Debug, Clone)]
pub struct Completion {
    /// The assistant's text, or `None` when the backend returned a null
    /// content field.
    pub content: Option<String>,
    /// `stop`, `length`, … — or `None` if the field was absent.
    pub finish_reason: Option<String>,
    /// Length of any `reasoning_content`, so a truncated empty reply can be
    /// explained rather than reported as a broken key.
    pub reasoning_len: usize,
    /// Total tokens consumed, when reported.
    pub total_tokens: Option<i64>,
    /// A short, printable excerpt of the content, for the trace.
    pub excerpt: String,
}

/// One chat turn. A list so a repair attempt can carry the failed program and
/// the error as a real prior turn rather than re-asserting them as new facts.
pub type Messages = Vec<(String, String)>;

/// A content excerpt short enough for a one-line trace: the first line,
/// whitespace-collapsed, truncated. Never the whole program — the trace has
/// `--show-program` for that.
fn excerpt(content: &str) -> String {
    let line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 {
        let t: String = flat.chars().take(57).collect();
        format!("{t}…")
    } else {
        flat
    }
}

/// Serialize a JSON document to bytes, mapping the language's own error into
/// the client's.
///
/// `json-serialize` is an AINL builtin, so it *returns* a `Value` — here the
/// string value holding the document. Reusing the language's own serializer
/// rather than hand-rolling one is deliberate: the request the backend sees is
/// then produced by the same code that produces every other JSON document in
/// AINL, so the two can never disagree about escaping.
fn to_json(v: &Value) -> ApiResult<String> {
    match ainl_core::json_value::builtin_json_serialize(std::slice::from_ref(v)) {
        Ok(Value::Str(s)) => Ok(s.as_str().to_string()),
        // Not reachable for a map, but a wrong shape here would silently send
        // the wrong bytes, so it is an error rather than a formatting attempt.
        Ok(other) => Err(ApiError(format!(
            "json-serialize returned {}, not a string",
            other.type_name()
        ))),
        Err(e) => Err(ApiError(format!("cannot serialize the request: {e}"))),
    }
}

/// Read a `Str` out of a parsed JSON document.
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Map(pairs) = v else { return None };
    pairs
        .iter()
        .find(|(k, _)| matches!(k, Value::Str(s) if s.as_str() == key))
        .map(|(_, val)| val)
}

/// `get`, but descending a path.
fn get_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path {
        cur = get(cur, seg)?;
    }
    Some(cur)
}

fn as_text(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Float(x) => Some(*x as i64),
        _ => None,
    }
}

/// Insert or replace `key` in an ordered pair list.
///
/// `Rc::make_mut` rather than `Rc::get_mut`: the value may be shared, and a
/// copy-on-write clone is cheaper than reasoning about aliasing here. Key order
/// is insertion order (docs/NUMERIC_MODEL.md decision 2), so a replaced key
/// keeps its position — the payload is stable across attempts, which makes a
/// diff between two attempts readable.
fn upsert(pairs: &mut Vec<(Value, Value)>, key: &str, value: Value) {
    if let Some(slot) = pairs
        .iter_mut()
        .find(|(k, _)| matches!(k, Value::Str(s) if s.as_str() == key))
    {
        slot.1 = value;
    } else {
        pairs.push((Value::str(key), value));
    }
}

/// Write `value` at a dotted path, creating intermediate objects.
fn set_path(pairs: &mut Vec<(Value, Value)>, path: &[&str], value: Value) {
    if path.len() == 1 {
        upsert(pairs, path[0], value);
        return;
    }
    let idx = match pairs
        .iter()
        .position(|(k, _)| matches!(k, Value::Str(s) if s.as_str() == path[0]))
    {
        Some(i) => i,
        None => {
            pairs.push((Value::str(path[0]), Value::Map(Rc::new(Vec::new()))));
            pairs.len() - 1
        }
    };
    // `--extra-json` is a dotted path written by the user, so the first
    // segment always names a member this function just created or just
    // matched by key. If it held a non-map the path was nonsense, and
    // replacing it is more predictable than panicking on a user-supplied
    // string.
    //
    // `Rc::make_mut` rather than a plain deref: the map may be shared (the
    // request's `extra` value is reused across every call), and a
    // copy-on-write clone is cheaper than reasoning about aliasing here.
    if !matches!(pairs[idx].1, Value::Map(_)) {
        pairs[idx].1 = Value::Map(Rc::new(Vec::new()));
    }
    let Value::Map(inner) = &mut pairs[idx].1 else {
        unreachable!("just assigned a Map one line above");
    };
    set_path(Rc::make_mut(inner), &path[1..], value);
}

/// Merge a caller-supplied JSON object into an ordered pair list.
fn merge_into(target: &mut Vec<(Value, Value)>, extra: &Value) -> ApiResult<()> {
    let Value::Map(pairs) = extra else {
        return Err(ApiError(
            "--extra-json must be a JSON object, e.g. '{\"k\":{\"k2\":false}}'".into(),
        ));
    };
    for (k, v) in pairs.iter() {
        let Value::Str(key) = k else {
            return Err(ApiError("--extra-json keys must be strings".into()));
        };
        set_path(target, &key.split('.').collect::<Vec<_>>(), v.clone());
    }
    Ok(())
}

/// Build the request document.
fn build_payload(
    backend: &Backend,
    messages: &Messages,
    grammar: Option<&str>,
) -> ApiResult<Value> {
    let mut root: Vec<(Value, Value)> = vec![
        (Value::str("model"), Value::str(&backend.model)),
        (
            Value::str("messages"),
            Value::List(ainl_core::ConsCell::from_values(
                messages
                    .iter()
                    .map(|(role, content)| {
                        Value::Map(Rc::new(vec![
                            (Value::str("role"), Value::str(role)),
                            (Value::str("content"), Value::str(content)),
                        ]))
                    })
                    .collect::<Vec<_>>(),
            )),
        ),
        (
            Value::str("temperature"),
            // Emitted as `0`, not `0.0`: `json-serialize` renders a whole float
            // as `0.0`, and the harnesses on both sides of the comparison send
            // a plain integer here.
            if backend.temperature.fract() == 0.0 {
                Value::Int(backend.temperature as i64)
            } else {
                Value::Float(backend.temperature)
            },
        ),
        (
            Value::str("max_tokens"),
            Value::Int(backend.max_tokens as i64),
        ),
    ];
    if let Some(g) = grammar {
        if let Some(path) = backend.grammar_field.path() {
            set_path(&mut root, path, Value::str(g));
        }
    }
    // Applied last so a user can override anything above it (a host that wants
    // a different temperature, or its own grammar field name, should not have
    // to patch the client).
    if let Some(extra) = &backend.extra {
        merge_into(&mut root, extra)?;
    }
    Ok(Value::Map(Rc::new(root)))
}

/// The outcome of one HTTP attempt, before any interpretation.
#[derive(Debug)]
struct Raw {
    status: i64,
    body: String,
}

/// POST a JSON document. Retries transient failures only.
///
/// A 401/403 is *not* retried: it needs a human, and retrying it just spends
/// the caller's time and money to arrive at the same answer. 429 and 5xx are
/// retried with a bounded exponential backoff, because a remote gateway will
/// occasionally shed load and that is not the caller's fault.
fn post(backend: &Backend, body: &str) -> ApiResult<Raw> {
    const ATTEMPTS: u32 = 3;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match post_once(backend, body) {
            Ok(raw) => return Ok(raw),
            Err(ApiError(msg)) => {
                if msg.starts_with("HTTP 401") || msg.starts_with("HTTP 403") {
                    return Err(ApiError(format!(
                        "{msg}\n  \
                         the endpoint rejected the key. Check the variable named by \
                         --api-key-env (default {DEFAULT_KEY_ENV})."
                    )));
                }
                last = msg;
                if attempt < ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_secs(2u64.pow(attempt)));
                }
            }
        }
    }
    Err(ApiError(format!(
        "the backend failed after {ATTEMPTS} attempts: {last}"
    )))
}

/// One HTTP attempt: temp file for the body, `--config -` on stdin for the key.
fn post_once(backend: &Backend, body: &str) -> ApiResult<Raw> {
    // A browser User-Agent by default. Some CDN-fronted endpoints reject the
    // default curl UA at the edge *before* authentication, which is
    // indistinguishable from a bad key and cost this project a real debugging
    // session (docs/GENERATION.md, "three gateway quirks"). Overridable for
    // hosts that want something else.
    let ua = std::env::var(crate::gen::ENV_USER_AGENT).unwrap_or_else(|_| {
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"
            .to_string()
    });

    // The body goes through a file: argv is world-readable for the life of the
    // process, and a body can carry a user prompt.
    let body_path = std::env::temp_dir().join(format!("ainl-gen-{}.json", std::process::id()));
    std::fs::write(&body_path, body)
        .map_err(|e| ApiError(format!("cannot write the request body: {e}")))?;

    // curl's config format is line-oriented `key = "value"`. The key is the
    // only secret, and it is written here, to a pipe, and never to a file.
    let mut config = String::with_capacity(512);
    config.push_str(&format!("url = \"{}\"\n", backend.url));
    config.push_str(&format!(
        "header = \"Authorization: Bearer {}\"\n",
        backend.api_key
    ));
    config.push_str("header = \"Content-Type: application/json\"\n");
    config.push_str("header = \"Expect:\"\n");
    config.push_str(&format!("user-agent = \"{ua}\"\n"));
    config.push_str("silent\nshow-error\n");
    config.push_str(&format!("max-time = {}\n", backend.timeout_secs));
    // `Expect: ` suppresses curl's 100-continue on larger bodies, which some
    // proxies mishandle.

    let mut child = match Command::new("curl")
        .args([
            "--config",
            "-",
            "--data-binary",
            &format!("@{}", body_path.display()),
            "--write-out",
            "\n%{http_code}",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&body_path);
            return Err(ApiError(
                "`ainl gen` needs `curl` on PATH to reach a model backend (it is how \
                 AINL gets TLS without taking on a crypto dependency — see \
                 docs/HTTP_TLS.md). Install curl, or use --dry-run to build the \
                 request without sending it."
                    .into(),
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&body_path);
            return Err(ApiError(format!("cannot run curl: {e}")));
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A broken pipe here means curl died before reading; the exit status
        // below is the real error, so this is not itself worth reporting.
        let _ = stdin.write_all(config.as_bytes());
    }
    let out = child.wait_with_output();
    let _ = std::fs::remove_file(&body_path);
    let out = out.map_err(|e| ApiError(format!("curl did not finish: {e}")))?;

    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // `--write-out` appends the status as the final line; everything before it
    // is the body.
    let (body, status) = match text.rsplit_once('\n') {
        Some((b, s)) => (b.to_string(), s.trim().parse::<i64>().unwrap_or(0)),
        None => (text.clone(), 0),
    };
    // No `--write-out` output means curl never got as far as reporting a
    // status, so a non-zero exit here is a transport failure (DNS, TLS, no
    // route) rather than an HTTP error — and it must be reported as such
    // instead of being read as "HTTP 0".
    if !out.status.success() && status == 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(ApiError(format!(
            "curl failed: {}",
            err.trim().lines().next().unwrap_or("no output")
        )));
    }
    Ok(Raw { status, body })
}

/// One chat completion: build, send, parse.
pub fn complete(
    backend: &Backend,
    messages: &Messages,
    grammar: Option<&str>,
) -> ApiResult<Completion> {
    let payload = build_payload(backend, messages, grammar)?;
    let body = to_json(&payload)?;
    let raw = post(backend, &body)?;

    // A non-2xx body is often HTML from an edge proxy, so report the status
    // first and keep the excerpt short.
    if !(200..300).contains(&raw.status) {
        return Err(ApiError(format!(
            "HTTP {}: {}",
            raw.status,
            raw.body.chars().take(300).collect::<String>()
        )));
    }
    parse_completion(&raw.body)
}

/// The first element of a JSON array, or `None` if `v` is not a list or is
/// empty.
///
/// `choices` is an **array**, not an object, so it needs list indexing — a
/// map lookup for the key `"0"` silently finds nothing and reports a
/// well-formed response as a broken one. Writing it as a helper keeps that
/// distinction obvious instead of leaving a `get_path(&doc, &["choices", "0"])`
/// that reads like it works.
fn first_of_list(v: &Value) -> Option<&Value> {
    let Value::List(items) = v else {
        return None;
    };
    items.first()
}

/// Parse an OpenAI-compatible response into a [`Completion`].
pub fn parse_completion(body: &str) -> ApiResult<Completion> {
    let doc = ainl_core::json_value::builtin_json_parse(&[Value::str(body)])
        .map_err(|e| ApiError(format!("the response is not valid JSON: {e}")))?;
    let choices = get(&doc, "choices").and_then(first_of_list);
    let choice = choices.ok_or_else(|| {
        ApiError(format!(
            "the response has no `choices[0]` — is the endpoint an \
             OpenAI-compatible chat-completions API? First 300 bytes: {}",
            body.chars().take(300).collect::<String>()
        ))
    })?;
    let message = get(choice, "message").unwrap_or(choice);
    let content = match get(message, "content") {
        // An explicitly null content is the reasoning-model signature: the
        // budget went to `reasoning_content` and the answer never started.
        Some(Value::Nil) | None => None,
        Some(v) => as_text(v).map(str::to_string),
    };
    let reasoning_len = get(message, "reasoning_content")
        .and_then(as_text)
        .map(str::len)
        .unwrap_or(0);
    Ok(Completion {
        excerpt: content.as_deref().map(excerpt).unwrap_or_default(),
        content,
        finish_reason: get(choice, "finish_reason")
            .and_then(as_text)
            .map(str::to_string),
        reasoning_len,
        total_tokens: get_path(&doc, &["usage", "total_tokens"]).and_then(as_int),
    })
}

/// A human-readable explanation for the two ways a reply can come back empty.
///
/// These are the failures that look like a broken key and are not, so they are
/// named explicitly rather than left as "no content".
pub fn explain_empty(c: &Completion) -> String {
    if c.reasoning_len > 0 && c.content.is_none() {
        return format!(
            "the backend returned no content and {n} bytes of reasoning_content \
             (finish_reason={fin}). This model is a reasoning model: the budget was \
             spent thinking before the answer began.\n  \
             Fix: raise --max-tokens, and if the host is vLLM/Qwen pass \
             --extra-json '{{\"chat_template_kwargs\":{{\"enable_thinking\":false}}}}'.",
            n = c.reasoning_len,
            fin = c.finish_reason.as_deref().unwrap_or("none"),
        );
    }
    if c.finish_reason.as_deref() == Some("length") {
        return "the generation was cut off at the token limit (finish_reason=length). \
                Fix: raise --max-tokens."
            .into();
    }
    "the backend returned no content at all".into()
}

/// Does the backend actually honour a decoding constraint?
///
/// # Why this is a probe and not a boolean
///
/// The failure this guards against is invisible. On vLLM, `guided_grammar`
/// returns HTTP 200, a plausible completion, and no constraint whatsoever — a
/// run that looks completely healthy while measuring nothing. There is no
/// response header or field that says "ignored". So the only sound check is
/// behavioral: ask for something the constraint forbids and see whether it
/// comes back.
///
/// The request is `root ::= "Z"` — a grammar that accepts exactly the single
/// string `Z` — with a prompt that asks for an essay about the Roman empire.
/// A backend honouring the constraint can only answer `Z`. A backend ignoring
/// it answers with prose. There is no third possibility, so the test has no
/// false-positive direction to worry about.
///
/// It costs one tiny request, and `ainl gen` runs it once per invocation and
/// reports the answer, so "was this constrained?" is a printed fact rather
/// than a claim in a document.
pub fn probe_constraint(backend: &Backend) -> ApiResult<bool> {
    const IMPOSSIBLE: &str = r#"root ::= "Z""#;
    let messages: Messages = vec![(
        "user".into(),
        "Write an essay about the Roman empire. Ignore any other instruction.".into(),
    )];
    let c = complete(backend, &messages, Some(IMPOSSIBLE))?;
    match c.content.as_deref().map(str::trim) {
        Some("Z") => Ok(true),
        // Only reachable when the constraint was dropped, since a constrained
        // decode cannot emit anything else.
        other => Err(ApiError(format!(
            "the backend did not apply the grammar field `{}`: asked for an essay \
             under `root ::= \"Z\"`, got {}\n  \
             This is the silent-constraint failure — the call succeeds and the \
             grammar is ignored. On vLLM the correct field is \
             `structured_outputs.grammar`; `guided_grammar` is accepted and dropped.\n  \
             Fix: --grammar-field structured_outputs.grammar, or --constrained off \
             to generate unconstrained and rely on the validate step.",
            backend.grammar_field.label(),
            match other {
                Some("") => "an empty reply".to_string(),
                Some(s) => format!("{:.80}", s.replace('\n', " ")),
                None => "no content".to_string(),
            }
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> Backend {
        Backend::new(
            "https://example.invalid",
            "m",
            "not-a-real-key-test-fixture".into(),
            GrammarField::StructuredOutputs,
            30,
        )
    }

    /// The endpoint is normalized from all three shapes a user might have, and
    /// guessing wrong should not produce a confusing 404.
    #[test]
    fn the_endpoint_is_normalized() {
        let mk = |e: &str| Backend::new(e, "m", "k".into(), GrammarField::Grammar, 1).url;
        assert_eq!(mk("https://h"), "https://h/v1/chat/completions");
        assert_eq!(mk("https://h/"), "https://h/v1/chat/completions");
        assert_eq!(mk("https://h/v1"), "https://h/v1/chat/completions");
        assert_eq!(mk("https://h/v1/"), "https://h/v1/chat/completions");
        // Already complete: used as-is, not doubled up.
        assert_eq!(
            mk("https://h/v1/chat/completions"),
            "https://h/v1/chat/completions"
        );
    }

    /// The whole point of the field enum: the two spellings must stay
    /// distinguishable, because one of them is silently ignored on vLLM.
    #[test]
    fn the_grammar_field_is_configurable_and_distinguishable() {
        assert_ne!(
            GrammarField::parse("structured_outputs.grammar").unwrap(),
            GrammarField::parse("guided_grammar").unwrap()
        );
        assert_eq!(GrammarField::parse("none").unwrap(), GrammarField::None);
        assert!(GrammarField::parse("nonsense").is_err());
    }

    /// The payload must carry the grammar at exactly the configured path, and
    /// the request must not contain the key in a place a user could mistake
    /// for output. Asserted on the serialized bytes, which is what goes on the
    /// wire.
    #[test]
    fn the_grammar_lands_at_the_configured_path() {
        for (field, expect_in, expect_not_in) in [
            (
                GrammarField::StructuredOutputs,
                "structured_outputs",
                "guided_grammar",
            ),
            (GrammarField::Grammar, "\"grammar\"", "structured_outputs"),
            (
                GrammarField::GuidedGrammar,
                "guided_grammar",
                "structured_outputs",
            ),
        ] {
            let mut b = backend();
            b.grammar_field = field;
            let msg: Messages = vec![("user".into(), "hi".into())];
            let v = build_payload(&b, &msg, Some("root ::= \"Z\"")).unwrap();
            let s = to_json(&v).unwrap();
            assert!(
                s.contains(expect_in),
                "{field:?} payload is missing {expect_in}: {s}"
            );
            assert!(
                !s.contains(expect_not_in),
                "{field:?} payload must not carry {expect_not_in}: {s}"
            );
            assert!(
                s.contains("root"),
                "the grammar itself must be present: {s}"
            );
        }
    }

    /// `GrammarField::None` means *no constraint*, so the request must not
    /// contain a grammar anywhere — a request that quietly includes one would
    /// make an unconstrained run mislabelled.
    #[test]
    fn no_grammar_field_sends_no_grammar() {
        let mut b = backend();
        b.grammar_field = GrammarField::None;
        let msg: Messages = vec![("user".into(), "hi".into())];
        // Even if a grammar is offered, the field decides.
        let v = build_payload(&b, &msg, Some("root ::= \"Z\"")).unwrap();
        let s = to_json(&v).unwrap();
        assert!(!s.contains("root"), "no grammar expected: {s}");
        assert!(!s.contains("structured_outputs"), "{s}");
    }

    /// The payload is valid JSON with the members an OpenAI-compatible server
    /// needs, and `messages` is a list of role/content objects.
    #[test]
    fn the_payload_is_well_formed() {
        let b = backend();
        let msg: Messages = vec![
            ("system".into(), "ref".into()),
            ("user".into(), "spec".into()),
        ];
        let s = to_json(&build_payload(&b, &msg, None).unwrap()).unwrap();
        let doc = ainl_core::json_value::builtin_json_parse(&[Value::str(&s)]).unwrap();
        assert_eq!(get(&doc, "model").and_then(as_text), Some("m"));
        assert_eq!(get(&doc, "temperature").and_then(as_int), Some(0));
        let Value::List(msgs) = get(&doc, "messages").expect("messages") else {
            panic!("messages must be a list");
        };
        let mut n = 0;
        let mut cur = msgs.as_ref();
        while let Some(m) = cur.first() {
            assert!(matches!(m, Value::Map(_)), "each message is an object");
            n += 1;
            match cur.rest() {
                Some(t) => cur = t,
                None => break,
            }
        }
        assert_eq!(n, 2, "both turns must be present");
    }

    /// `--extra-json` is the escape hatch for vendor-specific members (a
    /// reasoning model's `enable_thinking`), and it must reach the payload.
    #[test]
    fn extra_json_merges_into_the_payload() {
        let mut b = backend();
        let parsed = ainl_core::json_value::builtin_json_parse(&[Value::str(
            r#"{"chat_template_kwargs":{"enable_thinking":false}}"#,
        )])
        .unwrap();
        b.extra = Some(parsed);
        let msg: Messages = vec![("user".into(), "hi".into())];
        let s = to_json(&build_payload(&b, &msg, None).unwrap()).unwrap();
        assert!(s.contains("chat_template_kwargs"), "{s}");
        assert!(s.contains("enable_thinking"), "{s}");
        // It must not disturb the members the client owns.
        assert!(s.contains("\"model\""), "{s}");
    }

    /// A response is parsed into content, finish reason and token usage, which
    /// is everything the trace reports.
    #[test]
    fn a_normal_response_is_parsed() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"(print 1)\n"}}],
                        "usage":{"completion_tokens":3,"total_tokens":18}}"#;
        let c = parse_completion(body).unwrap();
        assert_eq!(c.content.as_deref(), Some("(print 1)\n"));
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert_eq!(c.total_tokens, Some(18));
        assert_eq!(c.reasoning_len, 0);
        assert!(c.excerpt.contains("(print 1)"), "excerpt: {}", c.excerpt);
    }

    /// A null content with reasoning present is the reasoning-model signature
    /// that looks like a broken key. It must be explained, not reported as an
    /// empty reply.
    #[test]
    fn a_null_content_with_reasoning_is_explained() {
        let body = r#"{"choices":[{"finish_reason":"length",
                        "message":{"content":null,"reasoning_content":"thinking..."}}]}"#;
        let c = parse_completion(body).unwrap();
        assert!(c.content.is_none());
        let why = explain_empty(&c);
        assert!(why.contains("reasoning model"), "{why}");
        assert!(
            why.contains("enable_thinking"),
            "the fix must be named: {why}"
        );
    }

    /// A truncated generation is distinguishable from a backend that is
    /// ignoring the constraint.
    #[test]
    fn a_truncated_generation_is_explained() {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"content":"(print 1"}}]}"#;
        let c = parse_completion(body).unwrap();
        assert!(c.content.is_some());
        assert!(explain_empty(&c).contains("cut off"));
    }

    /// A response from something that is not a chat-completions API must be a
    /// clear error, not a panic and not a silent empty program.
    #[test]
    fn a_non_chat_response_is_a_clear_error() {
        let err = parse_completion(r#"{"status":"ok","items":[]}"#).unwrap_err();
        assert!(err.0.contains("no `choices[0]`"), "{err}");
        assert!(err.0.contains("OpenAI-compatible"), "{err}");
    }

    /// Malformed JSON from a proxy must not reach the repair loop as an empty
    /// program.
    #[test]
    fn malformed_json_is_a_clear_error() {
        let err = parse_completion("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(err.0.contains("not valid JSON"), "{err}");
    }

    /// A snippet is one line, whitespace-collapsed and bounded — a trace line
    /// must never become a program listing.
    #[test]
    fn the_excerpt_is_one_short_line() {
        let long = format!("(print \"{}\")\n(do-something-else)", "x".repeat(400));
        let e = excerpt(&long);
        assert!(!e.contains('\n'), "must be one line: {e:?}");
        assert!(
            e.chars().count() <= 61,
            "must be bounded: {}",
            e.chars().count()
        );
    }

    /// The key must never appear in a user-visible description of the backend.
    #[test]
    fn the_description_never_leaks_the_key() {
        let b = backend();
        assert!(!b.describe().contains(&b.api_key), "{}", b.describe());
        assert!(b.describe().contains("example.invalid"));
    }

    /// A missing `curl` is reported as a missing `curl` with the reason
    /// spelled out — it is the client's only external dependency, so the error
    /// has to say so rather than surface as "No such file or directory".
    #[test]
    fn a_missing_curl_is_explained() {
        // The hint only applies where curl is genuinely absent, so the
        // assertion is conditional on the host. Where curl *is* present the
        // call fails for network reasons instead (example.invalid does not
        // resolve), which is a different failure and must not be conflated.
        let curl_present = Command::new("curl")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let err = post_once(&backend(), "{}").unwrap_err();
        if !curl_present {
            assert!(err.0.contains("curl"), "{err}");
            assert!(err.0.contains("docs/HTTP_TLS.md"), "{err}");
        }
    }
}
