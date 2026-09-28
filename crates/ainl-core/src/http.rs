//! `http-get` / `http-post` — the AINL HTTP client.
//!
//! This module is the **normative** definition of both builtins, the same
//! relationship `json_value.rs` has to its ports.
//!
//! # TLS: plain HTTP only, and that is a decision
//!
//! AINL's zero-dependency rule is not a preference — it is what makes the
//! AOT-compiled binary link against nothing but libc and lets the `aot-
//! standalone` CI job prove it with a musl-static link. Every production TLS
//! implementation is a C-transitive dependency tree (`rustls` needs `ring` or
//! `aws-lc-rs`; `native-tls` needs OpenSSL), so adding TLS means giving up
//! the static-binary property that the project actually sells. AINL therefore
//! speaks **plain HTTP over TCP**, and `https://` is refused up front with an
//! error that names the fix. See docs/SYNTAX.md §3c and the follow-up note in
//! docs/HTTP_TLS.md.
//!
//! This is the option-(a) outcome the Tier 1 HTTP card asks to choose between,
//! taken deliberately: option (b) — the smallest TLS path — would be a
//! conscious exception to the zero-dep rule, and the price is the standalone
//! binary. `docs/HTTP_TLS.md` prices both remaining paths instead of leaving
//! it as "we should do TLS someday".
//!
//! # Scope: the interpreter only
//!
//! The AOT C backend and the three transpilers refuse a program containing
//! `http-get`/`http-post`, with the same `interpreter-only` contract `import`
//! uses. See `ainl_cc::generate` and `ainl_transpile::transpile`.
//!
//! # The response value
//!
//! A response is an ordinary AINL map — no new value type, no new access
//! syntax, so it composes with `get`/`has`/`keys` the way everything else
//! does:
//!
//! ```lisp
//! (def r (http-get "http://127.0.0.1:8080/health"))
//! (get r "status")                 ; => 200  (an int)
//! (get r "ok")                    ; => true
//! (get r "body")                  ; => "…"
//! (get r "headers")               ; => {"content-type" "text/plain"}
//! ```
//!
//! Keys, all `str`: `status` (int), `ok` (bool), `body` (str), `headers` (a
//! map of lowercased header name to value).

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::value::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The longest AINL will wait for a TCP connect. A builtin that can hang
/// forever makes every program that touches the network untestable, so the
/// timeout is a language rule, not a tuning knob. Mirrors the shape of the
/// step counter: a hard bound, overridable in the environment.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest AINL will wait for anything to be *read* from the socket. A
/// server that accepts a connection and then stalls is the common real failure
/// (a hung process, a half-open connection), and without this a fetch blocks
/// the interpreter forever.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest response AINL will buffer, in bytes.
///
/// Unbounded reads are the other half of the "a builtin that can hang"
/// problem: a server that streams forever fills memory until the process dies.
/// A body over the cap is a **truncation**, and the response says so — see
/// [`truncated`].
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The largest response *header block* AINL will read. A server that never
/// sends a blank line would otherwise grow the header buffer without bound.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The names of the two builtins this module installs. Kept as constants
/// because the backend refusal scans for exactly these two symbols, and a
/// copy in three places would drift.
pub const HTTP_GET: &str = "http-get";
pub const HTTP_POST: &str = "http-post";

/// Bind `http-get` and `http-post` into `env`.
pub fn install(env: &Env) {
    env.define(
        HTTP_GET,
        Value::Builtin {
            name: HTTP_GET,
            f: builtin_http_get,
        },
    );
    env.define(
        HTTP_POST,
        Value::Builtin {
            name: HTTP_POST,
            f: builtin_http_post,
        },
    );
}

// ---- URL ------------------------------------------------------------------

/// A parsed `http://` URL: just enough to build a request line and a Host
/// header. Anything beyond that is the server's business, not AINL's.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    host: String,
    port: u16,
    /// The origin-form request target: `/` plus path, plus `?query`.
    target: String,
}

/// The one scheme this module speaks. `https://` is recognized precisely so it
/// can be *refused* with a good message rather than parsed as a host named
/// `https` and failing somewhere less legible.
const SCHEME: &str = "http://";

/// Parse a URL, or explain why it cannot be used.
///
/// The rules are deliberately small and total — there is no "best effort" path
/// that produces a partially-valid request, because a request built from a
/// half-understood URL is how you end up sending credentials to a host the
/// caller did not name.
fn parse_url(url: &str) -> Result<Url> {
    if let Some(rest) = url.strip_prefix("https://") {
        // A separate arm so the error can name the fix, and so the message
        // survives someone loosening the scheme check later.
        let _ = rest;
        return Err(Error::runtime(format!(
            "https is not supported: AINL speaks plain HTTP only, so its runtime can stay \
             dependency-free and its AOT binaries stay standalone. Use http://{rest} instead, \
             or front the endpoint with a local proxy that terminates TLS. \
             (docs/HTTP_TLS.md)"
        )));
    }
    let Some(rest) = url.strip_prefix(SCHEME) else {
        return Err(Error::runtime(format!(
            "http-get expects an http:// URL, got '{url}' (AINL speaks plain HTTP only)"
        )));
    };
    // Split authority from target at the first `/` (or the end). A `?query`
    // cannot appear before the first `/` in a well-formed URL, so this is the
    // authority boundary.
    let (authority, target) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(Error::runtime(format!("http-get: '{url}' has no host")));
    }
    // Reject credentials in the authority. AINL has no way to make them safe
    // (no redirect policy, no cookie jar) and a URL that carries a password
    // into a request line is a leak waiting to be logged by a server.
    if authority.contains('@') {
        return Err(Error::runtime(format!(
            "http-get: '{url}' carries credentials in the URL; \
             put them in a header instead, or use a URL with no userinfo"
        )));
    }
    // Host and optional port. The port is the text after the LAST `:`, which
    // is right for `host:port` and harmless for a bare IPv6 literal only
    // because those arrive bracketed — see below.
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]` or `[::1]:8080`. The host keeps its
        // brackets because `ToSocketAddrs` requires them.
        let Some(close) = stripped.find(']') else {
            return Err(Error::runtime(format!(
                "http-get: '{url}' has an unclosed '[' in its IPv6 host"
            )));
        };
        let h = &authority[..close + 2];
        match &stripped[close + 1..] {
            "" => (h.to_string(), 80),
            p => {
                let port = parse_port(p, url)?;
                (h.to_string(), port)
            }
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), parse_port(p, url)?),
            None => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return Err(Error::runtime(format!("http-get: '{url}' has no host")));
    }
    Ok(Url {
        host,
        port,
        // An empty target would produce the invalid request line `GET ` with
        // no path; HTTP requires at least `/`.
        target: if target.is_empty() {
            "/".to_string()
        } else {
            target.to_string()
        },
    })
}

fn parse_port(text: &str, url: &str) -> Result<u16> {
    // The leading `:` is still on `text` in the `rsplit_once` arm.
    let digits = text.strip_prefix(':').unwrap_or(text);
    if digits.is_empty() {
        return Err(Error::runtime(format!(
            "http-get: '{url}' has an empty port"
        )));
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::runtime(format!(
            "http-get: '{url}' has a non-numeric port '{digits}'"
        )));
    }
    digits.parse::<u16>().map_err(|_| {
        Error::runtime(format!(
            "http-get: '{url}' has a port outside 1-65535 ('{digits}')"
        ))
    })
}

// ---- the request ----------------------------------------------------------

/// A header block: lowercase name → value, first value per name wins.
///
/// Repeated headers are *not* joined. `Set-Cookie` arrives several times on a
/// real response and joining it with `, ` is wrong for every header that uses
/// commas as a list separator, so AINL keeps the first and says so in
/// docs/SYNTAX.md rather than inventing a representation it would then have to
/// keep consistent across backends.
#[derive(Debug)]
struct Headers(Vec<(String, String)>);

impl Headers {
    fn new() -> Headers {
        Headers(Vec::new())
    }

    fn push(&mut self, name: &str, value: &str) {
        let key = name.trim().to_ascii_lowercase();
        if self.0.iter().any(|(k, _)| *k == key) {
            return;
        }
        self.0.push((key, value.trim().to_string()));
    }

    fn get(&self, name: &str) -> Option<&str> {
        let key = name.trim().to_ascii_lowercase();
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    fn into_value(self) -> Value {
        // A map, built through the same insertion-order association list every
        // other map in AINL uses, so `keys`/`vals`/`get` all behave.
        let mut pairs: Vec<(Value, Value)> = self
            .0
            .into_iter()
            .map(|(k, v)| (Value::str(k), Value::str(v)))
            .collect();
        pairs.sort_by_key(|(k, _)| k.to_string());
        Value::Map(std::rc::Rc::new(pairs))
    }
}

/// Read the headers, then the body, from an open stream.
///
/// Split out from the socket so the same code serves the 1xx/204/304 shapes
/// that have no body, and so the tests can drive it over any `Read`.
fn read_response<R: Read>(mut r: R) -> Result<Response> {
    // Header block: read until CRLFCRLF (LF LF is accepted too, since a
    // hand-written test server is a first-class case here).
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find_header_end(&buf) {
            break i;
        }
        // Backstop only, for a server that never sends a terminator at all: a
        // read that already carries more than a whole capped body is far past
        // any real header block, so waiting for `\r\n\r\n` here is waiting
        // forever. The real cap is applied to `head_end` once it is found.
        if buf.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES {
            return Err(Error::runtime(
                "http: no end to the response headers after 72 MiB (the 64 KiB limit) \
                 — the server is not terminating its header block",
            ));
        }
        let n = r
            .read(&mut chunk)
            .map_err(|e| io_error("reading response headers", &e))?;
        if n == 0 {
            // EOF before the header terminator. Distinguish "server said
            // nothing" from "connection dropped" only in wording.
            if buf.is_empty() {
                return Err(Error::runtime(
                    "http: the server closed the connection without sending a response",
                ));
            }
            return Err(Error::runtime(
                "http: the connection closed in the middle of the response headers",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8(buf[..head_end].to_vec())
        .map_err(|_| Error::runtime("http: response headers are not valid utf-8"))?;
    // The cap applies to the header block, checked *after* the terminator is
    // located. Checking the read buffer instead would let a response whose
    // terminator happens to arrive in the same read that crossed the cap slip
    // through — the cap is then whatever the read granularity happened to be,
    // which is not a rule. The in-loop check below is only a backstop against
    // a server that never sends a terminator at all.
    if head_end > MAX_HEADER_BYTES {
        return Err(Error::runtime(format!(
            "http: response headers are {head_end} bytes, over the {} byte limit",
            MAX_HEADER_BYTES
        )));
    }
    let rest = &buf[head_end..];

    // Status line: `HTTP/1.1 200 OK`. AINL accepts HTTP/1.0 and 1.1 and
    // nothing else — it never negotiated anything above 1.1.
    let mut lines = head.split("\r\n").flat_map(|l| l.split('\n'));
    let Some(status_line) = lines.next() else {
        return Err(Error::runtime("http: the response has no status line"));
    };
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(Error::runtime(format!(
            "http: unsupported protocol version '{version}' in status line"
        )));
    }
    let code: i64 = match parts.next().map(str::trim) {
        Some(s) => s.parse().map_err(|_| {
            Error::runtime(format!("http: unparseable status code in '{status_line}'"))
        })?,
        None => {
            return Err(Error::runtime(format!(
                "http: status line has no code: '{status_line}'"
            )))
        }
    };
    let reason = parts.next().unwrap_or("").trim().to_string();

    let mut headers = Headers::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // RFC 9110 obsolete line folding: a continuation line starts with
        // SP/HTAB and continues the previous header's value. Joining with a
        // space is what every other HTTP client does.
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, last)) = headers.0.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            // A header line with no colon is malformed. Skipping it silently
            // would hide a server bug behind a working-looking response, so it
            // is an error, naming the line (which is usually enough to identify
            // the server that sent it).
            return Err(Error::runtime(format!(
                "http: malformed response header line '{line}'"
            )));
        };
        headers.push(name, value);
    }

    // Body framing. Exactly two mechanisms exist in HTTP/1.1 and AINL handles
    // both: a Content-Length, or chunked transfer-encoding. Anything else
    // (a close-delimited body) is refused rather than read-to-EOF, because
    // read-to-EOF on a keep-alive connection never returns.
    //
    // 1xx, 204 and 304 are bodyless by definition, so they are exempt from the
    // check even if the headers make a claim about a length.
    let bodyless = (100..200).contains(&code) || code == 204 || code == 304;
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if !bodyless && headers.get("content-length").is_none() && !chunked {
        return Err(Error::runtime(format!(
            "http: response has no Content-Length and no chunked Transfer-Encoding \
             (status {code}); AINL cannot tell where the body ends. \
             A close-delimited body needs chunked encoding or a length."
        )));
    }

    let (body, truncated) = if bodyless {
        (Vec::new(), false)
    } else if chunked {
        read_chunked(&mut r, rest.to_vec())?
    } else {
        // A declared length over the cap is truncated rather than refused:
        // the caller asked for a URL, not for a 2 GB download, and returning
        // the first 8 MiB with `truncated` set is strictly more useful than an
        // error that tells them nothing about the response they got. The
        // buffer is sized to the *cap*, never to the declared length — sizing
        // it to the declaration is how a hostile `Content-Length` turns into
        // an allocation the process cannot satisfy.
        let want: usize = headers
            .get("content-length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let limit = want.min(MAX_BODY_BYTES);
        let mut body = rest.to_vec();
        body.truncate(limit);
        let mut extra = vec![0u8; limit.saturating_sub(body.len())];
        let mut got = 0;
        while got < extra.len() {
            match r.read(&mut extra[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error("reading the response body", &e)),
            }
        }
        extra.truncate(got);
        body.extend_from_slice(&extra);
        if want <= MAX_BODY_BYTES && body.len() < want {
            return Err(Error::runtime(format!(
                "http: response promised {want} body bytes but the connection closed after {}",
                body.len()
            )));
        }
        (body, want > MAX_BODY_BYTES)
    };

    Ok(Response {
        status: code,
        reason,
        headers,
        body,
        truncated,
    })
}

/// The index just past the header block's terminating blank line, or `None`.
///
/// Returns the **end** of the terminator, not its start: the caller slices
/// `buf[..at]` for the header text and `buf[at..]` for the body, so returning
/// the start would leave the terminator glued to the front of the body — a
/// body that begins `\r\n` rather than its first byte.
///
/// `\r\n\r\n` is matched first and a bare `\n\n` accepted second, because a
/// hand-written test server is a first-class case here and `nc -l`-style
/// scripts rarely emit CRLF.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b + 2)),
        (Some(a), None) => Some(a + 4),
        (None, Some(b)) => Some(b + 2),
        (None, None) => None,
    }
}

fn read_chunked<R: Read>(mut r: R, mut pending: Vec<u8>) -> Result<(Vec<u8>, bool)> {
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    // A pending byte buffer, drained in order, so a chunk header split across
    // two reads still parses.
    let mut cur: Vec<u8> = std::mem::take(&mut pending);
    loop {
        // Read one line of the chunk header, out of `cur` plus new reads.
        let line = loop {
            if let Some((content_end, next)) = find_line_end(&cur) {
                let line = String::from_utf8_lossy(&cur[..content_end])
                    .trim()
                    .to_string();
                cur.drain(..next);
                break line;
            }
            let mut chunk = [0u8; 1024];
            match r.read(&mut chunk) {
                Ok(0) => {
                    return Err(Error::runtime(
                        "http: connection closed in the middle of a chunked body",
                    ))
                }
                Ok(n) => cur.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error("reading a chunked body", &e)),
            }
            if cur.len() > MAX_HEADER_BYTES {
                return Err(Error::runtime(
                    "http: chunked body exceeded the header limit",
                ));
            }
        };
        // A chunk header may carry `;ext=value` extensions; AINL ignores them.
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| Error::runtime(format!("http: bad chunk size '{size_text}'")))?;
        if size == 0 {
            // The terminating zero-size chunk. Trailer headers, if any, run to
            // the next blank line; AINL discards them (documented).
            break;
        }
        if out.len().saturating_add(size) > MAX_BODY_BYTES {
            // Stop *before* reading more than the cap. The response is still
            // returned, flagged truncated — a caller that wants a 20 MB
            // download should not have its process OOM-killed first.
            truncated = true;
            break;
        }
        // Read exactly `size` bytes, then the trailing CRLF.
        let mut want = size;
        while want > 0 {
            if cur.is_empty() {
                let mut chunk = [0u8; 8192];
                let n = r
                    .read(&mut chunk)
                    .map_err(|e| io_error("reading a chunked body", &e))?;
                if n == 0 {
                    return Err(Error::runtime(
                        "http: connection closed in the middle of a chunked body",
                    ));
                }
                cur.extend_from_slice(&chunk[..n]);
            }
            let take = want.min(cur.len());
            out.extend_from_slice(&cur[..take]);
            cur.drain(..take);
            want -= take;
        }
        // The CRLF after the chunk data. It is always exactly two bytes, so
        // the guard below bounds how long this can wait for them.
        while !cur.is_empty() {
            if let Some((_, next)) = find_line_end(&cur) {
                cur.drain(..next);
                break;
            }
            if cur.len() > 8 {
                return Err(Error::runtime("http: missing CRLF after a chunk"));
            }
            let mut chunk = [0u8; 8];
            let n = r
                .read(&mut chunk)
                .map_err(|e| io_error("reading a chunked body", &e))?;
            if n == 0 {
                return Err(Error::runtime("http: connection closed after a chunk"));
            }
            cur.extend_from_slice(&chunk[..n]);
        }
    }
    Ok((out, truncated))
}

/// The end of a line's *content* and the index just past its terminator.
///
/// Both are needed and they differ: for `\r\n` the content ends at the `\r`
/// and the next line starts two bytes later, so a caller that drains only
/// `content_end + 1` leaves a stray `\n` behind — which the chunked reader
/// then reads as an empty line and reports as `bad chunk size ''`.
fn find_line_end(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
        return Some((i, i + 2));
    }
    buf.iter().position(|&b| b == b'\n').map(|i| (i, i + 1))
}

fn io_error(what: &str, e: &std::io::Error) -> Error {
    Error::runtime(format!("http: {what} failed: {e}"))
}

/// The parsed response, before it becomes an AINL value.
#[derive(Debug)]
struct Response {
    status: i64,
    reason: String,
    headers: Headers,
    body: Vec<u8>,
    truncated: bool,
}

impl Response {
    /// The response as an AINL map. A body that is not valid UTF-8 has no
    /// string form in AINL (`read-file` takes the same position for the same
    /// reason — see docs/SYNTAX.md §3), so it is an error rather than a lossy
    /// replacement character.
    fn into_value(self) -> Result<Value> {
        let body = String::from_utf8(self.body)
            .map_err(|_| Error::runtime("http: the response body is not valid utf-8"))?;
        let mut pairs: Vec<(Value, Value)> = vec![
            (Value::str("status"), Value::Int(self.status)),
            (
                Value::str("ok"),
                Value::Bool((200..300).contains(&self.status)),
            ),
            (Value::str("body"), Value::str(body)),
        ];
        if self.truncated {
            // Present only when true, so the common case does not carry a key
            // every caller has to remember to check for absence.
            pairs.push((Value::str("truncated"), Value::Bool(true)));
        }
        // `reason` is the status text. Kept because a caller showing a status
        // to a human needs it, and HTTP/1.1 keeps it meaningful.
        pairs.push((Value::str("reason"), Value::str(self.reason)));
        pairs.push((Value::str("headers"), self.headers.into_value()));
        Ok(Value::Map(std::rc::Rc::new(pairs)))
    }
}

// ---- the builtins ---------------------------------------------------------

/// The one place a URL becomes a socket: resolve, connect with a timeout, send
/// the request, read the response.
/// The request head, built and **fully validated**, with no socket involved.
///
/// Splitting this out is what makes "refused before anything is sent" a
/// structural property rather than a promise. Every check that can reject the
/// call — a CRLF in a header, a CRLF in the body — runs here, so a bad request
/// costs a `format!` and never a DNS lookup, a connect, or a byte on the wire.
/// That matters for more than tidiness: a caller that got a connection error
/// for a request that was never going to be sent has been told the wrong
/// reason.
fn build_request(
    method: &str,
    url: &Url,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<String> {
    let mut req = String::with_capacity(256);
    req.push_str(&format!("{method} {} HTTP/1.1\r\n", url.target));
    // Host is mandatory in HTTP/1.1, and carries the port when it is not the
    // scheme default — otherwise a 1.x client can reach the wrong listener.
    if url.port == 80 {
        req.push_str(&format!("Host: {}\r\n", url.host));
    } else {
        req.push_str(&format!("Host: {}:{}\r\n", url.host, url.port));
    }
    for (name, value) in extra_headers {
        // A CR or LF in a header splits one request into two. This is the
        // oldest request-smuggling trick there is, and it is checked here,
        // before the socket exists.
        if is_crlf_injected(name) || is_crlf_injected(value) {
            return Err(Error::runtime(
                "http: a header name or value contains a CR or LF, which would split the request",
            ));
        }
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    // AINL computes the length itself. A caller cannot make the length lie
    // about its own body without also having to describe chunked encoding,
    // and a request whose length disagrees with its body is a desync the
    // server cannot detect. (A caller-supplied `Content-Length` is already
    // refused by `check_reserved_header`, so this is the only writer.)
    if let Some(b) = body {
        if is_crlf_injected(b) {
            return Err(Error::runtime(
                "http: the request body contains a CR or LF that would split the request",
            ));
        }
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    } else {
        req.push_str("Content-Length: 0\r\n");
    }
    // Every request closes its connection. AINL has no keep-alive pool, so
    // advertising otherwise would leave a server holding a socket open for a
    // client that is about to exit.
    req.push_str("Connection: close\r\n\r\n");
    Ok(req)
}

/// `request` split in two by design: [`build_request`] decides whether this
/// call is allowed to touch the network at all, and only then does this half
/// open a socket.
fn request(
    method: &str,
    url: &Url,
    extra_headers: &[(String, String)],
    body: Option<&str>,
) -> Result<Value> {
    // Reject first, connect second — see `build_request`.
    let req = build_request(method, url, extra_headers, body)?;

    // Resolve before the timeout applies to the socket, and keep the
    // resolution inside the connect budget: a DNS lookup that hangs is the
    // same failure as a connect that hangs, from the caller's side.
    //
    // Two things this must get right, both of which were bugs in the first
    // draft of this function:
    //   * `Url::host` is stored the way resolution wants it — a name or an IPv4
    //     literal bare, an IPv6 literal in brackets. Do NOT strip brackets
    //     here: `127.0.0.1` is not a bracketed literal, and round-tripping it
    //     through the bracket logic corrupts a working host.
    //   * resolve the `(host, port)` *pair*, not the bare host. `&str`'s
    //     `ToSocketAddrs` impl requires the text to carry a port, so
    //     `"127.0.0.1".to_socket_addrs()` fails with `invalid socket address`
    //     for a host that is perfectly valid.
    let addrs = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| Error::runtime(format!("http: cannot resolve host '{}': {e}", url.host)))?;
    let mut last_err: Option<std::io::Error> = None;
    let mut stream: Option<TcpStream> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some(mut stream) = stream else {
        return Err(io_error(
            &format!("cannot connect to {}:{}", url.host, url.port),
            &last_err.expect("at least one address was tried"),
        ));
    };
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));

    stream
        .write_all(req.as_bytes())
        .and_then(|()| match body {
            Some(b) => stream.write_all(b.as_bytes()),
            None => Ok(()),
        })
        .map_err(|e| io_error("sending the request", &e))?;
    let _ = stream.flush();
    read_response(&mut stream)?.into_value()
}

fn is_crlf_injected(s: &str) -> bool {
    s.contains('\r') || s.contains('\n')
}

/// `Host` and `Content-Length` are refused as caller-supplied headers.
///
/// They are not style rules: a caller-set `Host` names a *different* server to
/// the one it connected to, and a caller-set `Content-Length` can disagree
/// with the body AINL is about to write. Both are the classic request-smuggling
/// primitives, so AINL owns them.
fn check_reserved_header(name: &str) -> Result<()> {
    if name.trim().eq_ignore_ascii_case("host") {
        return Err(Error::runtime(
            "http: the Host header is set by AINL from the URL and cannot be overridden",
        ));
    }
    if name.trim().eq_ignore_ascii_case("content-length") {
        return Err(Error::runtime(
            "http: the Content-Length header is computed by AINL from the body \
             and cannot be overridden",
        ));
    }
    Ok(())
}

/// The headers argument: a map, or absent.
fn header_arg(v: &Value, who: &str) -> Result<Vec<(String, String)>> {
    let Value::Map(pairs) = v else {
        return Err(Error::runtime(format!(
            "{who} expects a hash of headers, got {}",
            v.type_name()
        )));
    };
    let mut out = Vec::with_capacity(pairs.len());
    for (k, val) in pairs.iter() {
        let Value::Str(name) = k else {
            return Err(Error::runtime(format!(
                "{who} header names must be str, got {}",
                k.type_name()
            )));
        };
        let Value::Str(value) = val else {
            return Err(Error::runtime(format!(
                "{who} header values must be str, got {}",
                val.type_name()
            )));
        };
        check_reserved_header(name)?;
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

/// `(http-get url)` → a response map.
///
/// `(http-get url headers)` is also accepted: a GET with headers is how you
/// set an `Authorization` or an `Accept`, which most real APIs need.
pub fn builtin_http_get(args: &[Value]) -> Result<Value> {
    let (url_text, headers) = match args {
        [u] => (as_url(u, HTTP_GET)?, Vec::new()),
        [u, h] => (as_url(u, HTTP_GET)?, header_arg(h, HTTP_GET)?),
        _ => {
            return Err(Error::runtime(format!(
                "{HTTP_GET} expects ({HTTP_GET} url [headers])"
            )))
        }
    };
    let url = parse_url(url_text)?;
    request("GET", &url, &headers, None)
}

/// `(http-post url body)` → a response map.
/// `(http-post url body [headers])` → the same, with extra request headers.
pub fn builtin_http_post(args: &[Value]) -> Result<Value> {
    let (url_text, body, headers) = match args {
        [u, b] => (as_url(u, HTTP_POST)?, as_body(b, HTTP_POST)?, Vec::new()),
        [u, b, h] => (
            as_url(u, HTTP_POST)?,
            as_body(b, HTTP_POST)?,
            header_arg(h, HTTP_POST)?,
        ),
        _ => {
            return Err(Error::runtime(format!(
                "{HTTP_POST} expects ({HTTP_POST} url body [headers])"
            )))
        }
    };
    let url = parse_url(url_text)?;
    request("POST", &url, &headers, Some(body))
}

fn as_url<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str URL, got {}",
            other.type_name()
        ))),
    }
}

fn as_body<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str body, got {}",
            other.type_name()
        ))),
    }
}

// ---- tests ----------------------------------------------------------------
//
// The unit tests below cover the pure halves (URL parsing, header folding,
// body framing) with no socket at all. The socket half is covered by
// crates/ainl-core/tests/http.rs, which stands up a real listener on a loopback
// port and drives the builtins end to end.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_url_gets_the_default_port() {
        let u = parse_url("http://example.com/x").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 80);
        assert_eq!(u.target, "/x");
    }

    #[test]
    fn a_bare_host_gets_a_root_target() {
        let u = parse_url("http://example.com").unwrap();
        assert_eq!(u.target, "/");
        assert_eq!(u.port, 80);
    }

    #[test]
    fn a_query_string_stays_in_the_target() {
        let u = parse_url("http://h/p?a=1&b=2").unwrap();
        assert_eq!(u.target, "/p?a=1&b=2");
    }

    #[test]
    fn an_explicit_port_is_used() {
        assert_eq!(parse_url("http://h:8080/x").unwrap().port, 8080);
        assert_eq!(parse_url("http://h:8080").unwrap().port, 8080);
    }

    #[test]
    fn https_is_refused_with_the_fix_in_the_message() {
        let e = parse_url("https://example.com/x").unwrap_err().to_string();
        assert!(e.contains("not supported"), "got: {e}");
        assert!(e.contains("http://example.com/x"), "got: {e}");
    }

    #[test]
    fn a_missing_scheme_is_refused() {
        let e = parse_url("example.com/x").unwrap_err().to_string();
        assert!(e.contains("http://"), "got: {e}");
    }

    #[test]
    fn a_bad_port_is_refused_by_name() {
        assert!(parse_url("http://h:abc/x")
            .unwrap_err()
            .to_string()
            .contains("non-numeric"));
        assert!(parse_url("http://h:/x")
            .unwrap_err()
            .to_string()
            .contains("empty port"));
        assert!(parse_url("http://h:99999/x")
            .unwrap_err()
            .to_string()
            .contains("1-65535"));
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        let u = parse_url("http://[::1]:9000/health").unwrap();
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.port, 9000);
    }

    /// A regression pin for a bug this suite actually hit: `request()` used to
    /// strip brackets from `Url::host` and re-add them, so `127.0.0.1` came
    /// out as `[127.0.0.1` + ... and every IPv4 loopback URL died with
    /// `invalid socket address`. The pure URL tests could not catch it (they
    /// never resolve), which is exactly why the end-to-end suite in
    /// `tests/http.rs` exists.
    #[test]
    fn an_ipv4_host_needs_no_bracket_handling() {
        let u = parse_url("http://127.0.0.1:8080/x").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8080);
        // And it must actually resolve, which is where the bug bit. The
        // `(host, port)` pair is what `request()` resolves; a bare host string
        // is not, because `&str`'s `ToSocketAddrs` requires a port in the text.
        assert!(
            (u.host.as_str(), u.port).to_socket_addrs().is_ok(),
            "127.0.0.1:8080 must resolve"
        );
    }

    #[test]
    fn credentials_in_the_url_are_refused() {
        let e = parse_url("http://user:pw@h/x").unwrap_err().to_string();
        assert!(e.contains("credentials"), "got: {e}");
    }

    #[test]
    fn header_names_fold_to_lowercase_and_trim() {
        let mut h = Headers::new();
        h.push("Content-Type", " text/plain ");
        assert_eq!(h.get("content-type"), Some("text/plain"));
        assert_eq!(h.get("CONTENT-TYPE"), Some("text/plain"));
    }

    #[test]
    fn a_repeated_header_keeps_the_first_value() {
        let mut h = Headers::new();
        h.push("X-A", "1");
        h.push("x-a", "2");
        assert_eq!(h.get("x-a"), Some("1"));
        assert_eq!(h.0.len(), 1);
    }

    #[test]
    fn a_cont_fold_continues_the_previous_value() {
        // The header block must end with a blank line like any other; the
        // folded continuation is the only unusual part.
        let r = read_response(
            &b"HTTP/1.1 200 OK\r\nX-Long: one\r\n  two\r\nContent-Length: 0\r\n\r\n"[..],
        )
        .unwrap();
        assert_eq!(r.headers.get("x-long"), Some("one two"));
    }

    #[test]
    fn a_content_length_body_is_read_exactly() {
        let r = read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"[..]).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let r = read_response(&raw[..]).unwrap();
        assert_eq!(r.body, b"hello world");
        assert!(!r.truncated);
    }

    #[test]
    fn a_chunk_extension_is_ignored() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;a=b\r\nhello\r\n0\r\n\r\n";
        let r = read_response(&raw[..]).unwrap();
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn a_204_has_no_body_even_with_a_length() {
        let r =
            read_response(&b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"[..]).unwrap();
        assert_eq!(r.status, 204);
        assert!(r.body.is_empty());
    }

    #[test]
    fn an_unframed_body_is_refused() {
        let e = read_response(&b"HTTP/1.1 200 OK\r\n\r\nbody"[..])
            .unwrap_err()
            .to_string();
        assert!(e.contains("Content-Length"), "got: {e}");
    }

    #[test]
    fn a_truncated_body_against_its_length_is_an_error() {
        let e = read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort"[..])
            .unwrap_err()
            .to_string();
        assert!(e.contains("closed after 5"), "got: {e}");
    }

    #[test]
    fn a_header_larger_than_the_cap_is_refused() {
        let big = format!(
            "HTTP/1.1 200 OK\r\nX: {}\r\n\r\n",
            "y".repeat(MAX_HEADER_BYTES + 16)
        );
        let e = read_response(big.as_bytes()).unwrap_err().to_string();
        assert!(e.contains("over the 65536 byte limit"), "got: {e}");
    }

    /// A `Content-Length` over the cap must not be allocated, must be
    /// truncated, and must say so. All three matter, and the middle one is the
    /// reason: a response declaring a huge length is the cheap way to make a
    /// client allocate whatever it likes, so the buffer is sized to the cap and
    /// never to the declaration.
    #[test]
    fn a_declared_length_over_the_cap_is_truncated_not_allocated() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES * 4
        );
        // A header plus only 4 KiB of body: enough to prove AINL stops at the
        // cap instead of trying to read the declared 32 MiB that never arrive.
        let mut raw = head.into_bytes();
        raw.extend(std::iter::repeat_n(b'z', 4096));
        let r = read_response(&raw[..]).expect("over-cap body is not fatal");
        assert_eq!(r.status, 200);
        assert!(r.truncated, "an over-cap body must be flagged truncated");
        assert!(r.body.len() <= MAX_BODY_BYTES);
    }

    #[test]
    fn a_body_at_exactly_the_cap_is_not_truncated() {
        // The boundary, because `<=` and `<` are exactly the kind of thing a
        // refactor gets wrong: a body of precisely MAX_BODY_BYTES is complete
        // and must NOT carry the flag.
        let body = vec![b'x'; MAX_BODY_BYTES];
        let mut raw =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        raw.extend_from_slice(&body);
        let r = read_response(&raw[..]).expect("a body at the cap is complete");
        assert_eq!(r.body.len(), MAX_BODY_BYTES);
        assert!(!r.truncated, "a body at exactly the cap is not truncated");
    }

    #[test]
    fn a_non_utf8_body_is_an_error() {
        let r =
            read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n\xff\xfe"[..]).unwrap();
        assert!(r.into_value().unwrap_err().to_string().contains("utf-8"));
    }

    #[test]
    fn the_response_value_carries_status_ok_body_and_headers() {
        let r = read_response(
            &b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"[..],
        )
        .unwrap();
        let v = r.into_value().unwrap();
        let get = |k: &str| match &v {
            Value::Map(p) => p
                .iter()
                .find(|(a, _)| matches!(a, Value::Str(s) if s.as_str() == k))
                .map(|(_, b)| b.clone()),
            _ => panic!("not a map"),
        };
        assert_eq!(get("status"), Some(Value::Int(201)));
        assert_eq!(get("ok"), Some(Value::Bool(true)));
        assert_eq!(get("body"), Some(Value::str("{}")));
        assert_eq!(get("reason"), Some(Value::str("Created")));
        // Headers are their own nested map, reachable with the ordinary `get`.
        assert_eq!(
            get("headers").map(|h| match h {
                Value::Map(p) => p
                    .iter()
                    .find(|(a, _)| matches!(a, Value::Str(s) if s.as_str() == "content-type"))
                    .map(|(_, b)| b.clone()),
                _ => None,
            }),
            Some(Some(Value::str("application/json")))
        );
    }

    #[test]
    fn a_non_2xx_status_is_a_normal_response_not_an_error() {
        // A 404 is a fact about the server, not a failure of the fetch. The
        // builtin returns it; the caller decides.
        let r = read_response(&b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"[..]).unwrap();
        let v = r.into_value().unwrap();
        match &v {
            Value::Map(p) => {
                let ok = p
                    .iter()
                    .find(|(a, _)| matches!(a, Value::Str(s) if s.as_str() == "ok"))
                    .map(|(_, b)| b);
                assert_eq!(ok, Some(&Value::Bool(false)));
            }
            _ => panic!("not a map"),
        }
    }

    #[test]
    fn a_reserved_header_cannot_be_overridden() {
        assert!(check_reserved_header("Host").is_err());
        assert!(check_reserved_header("content-length").is_err());
        assert!(check_reserved_header("Accept").is_ok());
    }
}
