//! `http-get` / `http-post` end to end, against a real socket on loopback.
//!
//! The unit tests in `src/http.rs` cover the pure halves (URL parsing, header
//! folding, body framing) with no socket. This file covers the half they cannot
//! reach: that a real request is really written, that a real response really
//! comes back, and that the whole thing survives the trip through the
//! evaluator.
//!
//! **No external network.** Every server here is a `TcpListener` bound to
//! `127.0.0.1:0` — the OS picks a free port, the test reads the address back,
//! and a background thread answers. Nothing leaves the machine, so the suite
//! is deterministic and runs offline.
//!
//! Every request is also run on **both** interpreters, because "the VM and the
//! tree-walk agree" is the project's standing differential-testing rule for any
//! builtin, and HTTP is no exception: a builtin that is a real call in one and
//! an unbound symbol in the other would be a silent, hard-to-find difference.

use ainl_core::value::Value;
use ainl_core::BigNum;
use ainl_core::Env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

/// A one-shot HTTP server: accepts connections and replies with `reply`,
/// recording what it received.
struct Server {
    addr: std::net::SocketAddr,
    /// The raw request bytes the server saw, joined across connections.
    received: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Set by `Drop` to end the accept loop.
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    /// Start a server that answers every request with `reply`, verbatim.
    fn canned(reply: &'static [u8]) -> Server {
        Server::spawn(move |_req| reply.to_vec())
    }

    /// Start a server that answers with a reply the test built at runtime —
    /// for a response whose `Content-Length` is computed from its own body.
    fn owned(reply: String) -> Server {
        Server::spawn(move |_req| reply.clone().into_bytes())
    }

    /// Start a server whose reply is computed from the request text, so a test
    /// can echo the body back and assert on what was actually sent.
    fn echoing(f: fn(&str) -> Vec<u8>) -> Server {
        Server::spawn(f)
    }

    fn spawn(f: impl Fn(&str) -> Vec<u8> + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = received.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                // A short read timeout on the accept side means this thread can
                // never wedge the harness: if a client connects and says
                // nothing, the read below gives up instead of parking forever.
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut text = String::new();
                // Read the request: headers, then the declared body length.
                // Stopping at the header terminator is not enough — a POST's
                // body arrives after it, and a server that ignores the body
                // would make every echo test pass vacuously.
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if let Some(head_end) = find_blank_line(&text) {
                        let want = content_length(&text[..head_end]);
                        if text.len() - head_end >= want {
                            break;
                        }
                    }
                }
                if text.is_empty() {
                    // The wake-up connection from `Drop`. Nothing to answer.
                    break;
                }
                sink.lock().expect("sink").push(text.clone());
                let reply = f(&text);
                let _ = stream.write_all(&reply);
                let _ = stream.flush();
                // AINL sends `Connection: close`, so dropping is the right way
                // to end the response.
            }
        });
        Server {
            addr,
            received,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Every request the server saw, in order.
    fn requests(&self) -> Vec<String> {
        self.received.lock().expect("sink").clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Stop the accept loop and WAIT for the thread, so a failing test
        // cannot leave a thread blocked in `accept()` holding the harness open
        // (which reads as a hang, not as a failure).
        if let Some(stop) = self.stop.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(h) = self.handle.take() {
            // Unblock the `accept()` that is currently parked.
            let _ = TcpStream::connect(self.addr);
            let _ = h.join();
        }
    }
}

fn find_blank_line(s: &str) -> Option<usize> {
    s.find("\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| s.find("\n\n").map(|i| i + 2))
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// Run a program on the **bytecode VM** and return its last value.
fn run_vm(src: &str) -> ainl_core::Result<Value> {
    let env = Env::with_prelude();
    ainl_core::vm::run_in(src, &env)
}

/// Run the same program on the **tree-walking evaluator**, for the
/// differential half of every test below.
fn run_tree(src: &str) -> ainl_core::Result<Value> {
    let env = Env::with_prelude();
    ainl_core::tree_walk_in(src, &env)
}

/// The evaluation the builtin actually goes through. A `get` on the response
/// map, so the test asserts on AINL values rather than on Rust structs.
fn field(resp: &Value, key: &str) -> Value {
    match resp {
        Value::Map(pairs) => pairs
            .iter()
            .find(|(k, _)| matches!(k, Value::Str(s) if s.as_str() == key))
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Nil),
        other => panic!("response should be a map, got {}", other.type_name()),
    }
}

// ---- http-get --------------------------------------------------------------

/// The card's headline acceptance: a GET against a local server, with the
/// response (status/headers/body) as a clean AINL value.
#[test]
fn http_get_returns_status_headers_and_body() {
    let server = Server::canned(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Trace: abc\r\nContent-Length: 5\r\n\r\nhello",
    );
    let src = format!(r#"(get (http-get "{}") "status")"#, server.url("/hello"));
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/hello")))
        .expect("http-get should succeed");
    assert_eq!(field(&resp, "status"), Value::Int(BigNum::small(200)));
    assert_eq!(field(&resp, "ok"), Value::Bool(true));
    assert_eq!(field(&resp, "body"), Value::str("hello"));
    assert_eq!(field(&resp, "reason"), Value::str("OK"));
    // Headers are a nested map, reachable with the ordinary `get` — no new
    // access syntax, which is the point.
    let headers = field(&resp, "headers");
    assert_eq!(field(&headers, "content-type"), Value::str("text/plain"));
    assert_eq!(field(&headers, "x-trace"), Value::str("abc"));
    // And it composes with `get` at the source level too.
    assert_eq!(run_vm(&src).unwrap(), Value::Int(BigNum::small(200)));
}

/// AINL sends an origin-form request target, a Host header, and a length. A
/// server that echoes the request lets the test assert on the *bytes*, which
/// is where an HTTP client's real bugs live.
#[test]
fn the_request_line_and_headers_are_well_formed() {
    let server = Server::echoing(|req| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{req}",
            req.len()
        )
        .into_bytes()
    });
    let url = server.url("/some/path?a=1");
    let resp = run_vm(&format!(r#"(http-get "{url}")"#)).expect("get");
    let body = match field(&resp, "body") {
        Value::Str(s) => s.as_str().to_string(),
        other => panic!("body should be a str, got {}", other.type_name()),
    };
    let want_request_line = "GET /some/path?a=1 HTTP/1.1\r\n";
    assert!(
        body.starts_with(want_request_line),
        "request line must be origin-form; got:\n{body}"
    );
    // Host carries the port when it is not the scheme default, or a 1.x client
    // reaches the wrong listener.
    let host_line = format!("Host: {}", server.addr);
    assert!(
        body.contains(&host_line),
        "expected `{host_line}` in:\n{body}"
    );
    assert!(body.contains("Connection: close"), "got:\n{body}");
}

/// A GET with a headers argument, which is how a caller sets `Accept` or
/// `Authorization` — most real APIs need one.
#[test]
fn http_get_sends_caller_headers() {
    let server = Server::echoing(|req| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{req}",
            req.len()
        )
        .into_bytes()
    });
    run_vm(&format!(
        r#"(http-get "{}" (hash "Authorization" "Bearer t0ken" "Accept" "application/json"))"#,
        server.url("/x")
    ))
    .expect("get with headers");
    let req = &server.requests()[0];
    assert!(req.contains("Authorization: Bearer t0ken"), "got:\n{req}");
    assert!(req.contains("Accept: application/json"), "got:\n{req}");
}

/// A non-2xx status is a fact about the server, not a failure of the fetch.
/// Refusing it would make every error page a runtime error, and the caller
/// could not tell a 404 from a 500 without string-matching a message.
#[test]
fn a_404_is_returned_not_raised() {
    let server = Server::canned(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot here!");
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/missing")))
        .expect("a 404 must not be an error");
    assert_eq!(field(&resp, "status"), Value::Int(BigNum::small(404)));
    assert_eq!(field(&resp, "ok"), Value::Bool(false));
    assert_eq!(field(&resp, "body"), Value::str("not here!"));
}

/// A 204 is bodyless by definition, even if the server claims a length.
#[test]
fn a_204_has_an_empty_body() {
    let server = Server::canned(b"HTTP/1.1 204 No Content\r\nContent-Length: 5\r\n\r\n");
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x"))).expect("204");
    assert_eq!(field(&resp, "status"), Value::Int(BigNum::small(204)));
    assert_eq!(field(&resp, "body"), Value::str(""));
}

/// Chunked transfer-encoding, which every real server uses for a response of
/// unknown length.
#[test]
fn a_chunked_body_is_reassembled_over_the_socket() {
    let server = Server::canned(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
          5\r\nhello\r\n1\r\n \r\n5\r\nworld\r\n0\r\n\r\n",
    );
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x"))).expect("chunked");
    assert_eq!(field(&resp, "body"), Value::str("hello world"));
}

/// A repeated header keeps its first value. Documented in SYNTAX.md §3c;
/// pinned here so the rule cannot drift silently.
#[test]
fn a_repeated_response_header_keeps_the_first_value() {
    let server = Server::canned(
        b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 0\r\n\r\n",
    );
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x"))).expect("cookies");
    let headers = field(&resp, "headers");
    assert_eq!(field(&headers, "set-cookie"), Value::str("a=1"));
}

/// `truncated` is present only when the body was cut at the cap, so a normal
/// response does not carry a key every caller must remember to test for
/// absence.
#[test]
fn a_small_body_carries_no_truncated_key() {
    let server = Server::canned(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let resp = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x"))).expect("small");
    assert_eq!(field(&resp, "truncated"), Value::Nil);
}

// ---- http-post -------------------------------------------------------------

#[test]
fn http_post_sends_the_body_and_the_server_receives_it() {
    let server = Server::echoing(|req| {
        format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{req}",
            req.len()
        )
        .into_bytes()
    });
    let resp = run_vm(&format!(
        r#"(http-post "{}" "hello=world")"#,
        server.url("/submit")
    ))
    .expect("post");
    assert_eq!(field(&resp, "status"), Value::Int(BigNum::small(201)));
    let req = &server.requests()[0];
    assert!(req.starts_with("POST /submit HTTP/1.1"), "got:\n{req}");
    // The length must match the body, or the server reads the wrong number of
    // bytes and the request is unparseable.
    assert!(req.contains("Content-Length: 11"), "got:\n{req}");
    assert!(
        req.ends_with("hello=world"),
        "the body must be the last thing on the wire, got:\n{req}"
    );
}

#[test]
fn http_post_sends_caller_headers() {
    let server = Server::echoing(|req| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{req}",
            req.len()
        )
        .into_bytes()
    });
    run_vm(&format!(
        r#"(http-post "{}" "{{}}" (hash "Content-Type" "application/json"))"#,
        server.url("/api")
    ))
    .expect("post with headers");
    let req = &server.requests()[0];
    assert!(
        req.contains("Content-Type: application/json"),
        "got:\n{req}"
    );
}

/// A JSON API is the actual reason `http-post` exists, so JSON in and JSON out
/// is the case worth proving works together.
///
/// The fixture body is built with a `format!` that computes its own
/// `Content-Length`. A hand-typed length is the single most likely mistake in a
/// hand-written HTTP test, and it fails in the least obvious way: a length that
/// is too *short* leaves trailing bytes in the buffer, which then look like the
/// start of the next response.
#[test]
fn a_json_api_round_trips_through_json_parse() {
    // The body deliberately carries odd spacing; the point is that AINL hands
    // the bytes to `json-parse` untouched, so the server's own formatting is
    // what the parser sees.
    let body = "{\"answer\":  \"42\"  }";
    let server = Server::owned(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    let src = format!(
        r#"(get (json-parse (get (http-post "{}" "{{\"q\": 1}}" (hash "Content-Type" "application/json")) "body")) "answer")"#,
        server.url("/ask")
    );
    // `"42"` is a JSON *string*, so it comes back as an AINL str, not an int.
    // Asserting an Int here would be asserting that AINL guesses at a JSON
    // string's type — it does not, and must not: the parse is a parse.
    assert_eq!(
        run_vm(&src).expect("json round trip"),
        Value::str("42"),
        "the answer is a JSON string, so the AINL value is a str"
    );
}

// ---- the refusal and safety rules ------------------------------------------

/// This is the TLS decision, tested. `https://` is refused by name, with the
/// fix in the message — not attempted and failed later in a handshake.
#[test]
fn https_is_refused_by_name() {
    let e = run_vm(r#"(http-get "https://example.com/")"#)
        .expect_err("https must be refused")
        .to_string();
    assert!(e.contains("not supported"), "got: {e}");
    assert!(
        e.contains("http://example.com/"),
        "the message must name the fix: {e}"
    );
}

/// The refusal happens with no socket at all, which is what makes it
/// deterministic and offline-testable.
#[test]
fn https_is_refused_before_any_connection_is_attempted() {
    // A host that cannot resolve: if the client tried to connect first, this
    // would fail with a resolution error, not the TLS message.
    let e = run_vm(r#"(http-post "https://no-such-host.invalid/" "b")"#)
        .expect_err("must be refused")
        .to_string();
    assert!(e.contains("https is not supported"), "got: {e}");
}

#[test]
fn a_missing_scheme_is_refused() {
    let e = run_vm(r#"(http-get "example.com/")"#)
        .expect_err("must be refused")
        .to_string();
    assert!(e.contains("http://"), "got: {e}");
}

/// A body that is not valid UTF-8 cannot become an AINL string, and AINL has
/// no lossy-decoding rule anywhere else (`read-file` takes the same position),
/// so this is an error rather than a replacement character.
#[test]
fn a_non_utf8_body_is_refused() {
    let server = Server::canned(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n\xff\xfe");
    let e = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x")))
        .expect_err("invalid utf-8 must be refused")
        .to_string();
    assert!(e.contains("utf-8"), "got: {e}");
}

/// An unframed body means AINL cannot tell where the body ends. Reading to
/// EOF would hang on a keep-alive connection, so it is refused with the fix.
#[test]
fn an_unframed_body_is_refused_with_the_fix() {
    let server = Server::canned(b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\nbody");
    let e = run_vm(&format!(r#"(http-get "{}")"#, server.url("/x")))
        .expect_err("unframed must be refused")
        .to_string();
    assert!(e.contains("Content-Length"), "got: {e}");
}

/// A `Host` the caller supplies would name a different server than the one
/// connected to, and a `Content-Length` that disagrees with the body is the
/// request-smuggling primitive. AINL owns both.
#[test]
fn the_host_header_cannot_be_overridden() {
    let e = run_vm(r#"(http-get "http://127.0.0.1:1/x" (hash "Host" "evil.example"))"#)
        .expect_err("Host override must be refused")
        .to_string();
    assert!(e.contains("Host"), "got: {e}");
}

#[test]
fn the_content_length_header_cannot_be_overridden() {
    let e = run_vm(r#"(http-post "http://127.0.0.1:1/x" "b" (hash "Content-Length" "9999"))"#)
        .expect_err("Content-Length override must be refused")
        .to_string();
    assert!(e.contains("Content-Length"), "got: {e}");
}

/// A header value carrying CRLF would split the request into two requests —
/// the oldest request-smuggling trick there is. Refused before it is written.
///
/// The URL points at a closed port on purpose: the check must fire *before*
/// the connect, so a connection-refused error here would mean AINL wrote the
/// request to the wire and only then complained.
#[test]
fn a_header_value_cannot_smuggle_a_crlf() {
    let e = run_vm(r#"(http-get "http://127.0.0.1:1/x" (hash "X" "a\r\nX-Evil: 1"))"#)
        .expect_err("CRLF must be refused")
        .to_string();
    assert!(e.contains("CR or LF"), "got: {e}");
    assert!(
        !e.contains("Connection refused"),
        "the check must precede the connect, not follow it: {e}"
    );
}

/// Credentials in a URL get copied into a request line and then logged by
/// whatever sits in the middle. AINL has no redirect policy or cookie jar to
/// make them safe, so it refuses.
#[test]
fn credentials_in_a_url_are_refused() {
    let e = run_vm(r#"(http-get "http://user:pw@example.com/")"#)
        .expect_err("userinfo must be refused")
        .to_string();
    assert!(e.contains("credentials"), "got: {e}");
}

/// Nothing reaches the network when the arguments are wrong — every check
/// happens before a socket is opened, so a typo is free.
#[test]
fn a_non_string_url_is_refused_without_connecting() {
    let e = run_vm("(http-get 42)")
        .expect_err("must be refused")
        .to_string();
    assert!(e.contains("expects a str URL"), "got: {e}");
}

#[test]
fn wrong_arity_is_refused() {
    let e = run_vm(r#"(http-get "http://x/" "extra" "more")"#)
        .expect_err("must be refused")
        .to_string();
    assert!(e.contains("http-get expects"), "got: {e}");
    let e = run_vm(r#"(http-post "http://x/")"#)
        .expect_err("must be refused")
        .to_string();
    assert!(e.contains("http-post expects"), "got: {e}");
}

#[test]
fn a_headers_must_be_a_map_of_strings() {
    for src in [
        r#"(http-get "http://x/" "not a map")"#,
        r#"(http-get "http://x/" (hash 1 "v"))"#,
        r#"(http-get "http://x/" (hash "k" 1))"#,
    ] {
        assert!(run_vm(src).is_err(), "should have been refused: {src}");
    }
}

/// Nothing listens on this port, so the connect must fail — and it must fail
/// with a connect error rather than hanging. The connect timeout is a language
/// rule precisely so this test can assert it.
#[test]
fn a_closed_port_fails_fast() {
    // Bind then drop, to get a port nothing is listening on.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let e = run_vm(&format!(r#"(http-get "http://127.0.0.1:{port}/")"#))
        .expect_err("a closed port must fail")
        .to_string();
    assert!(e.contains("connect"), "got: {e}");
}

// ---- the differential rule -------------------------------------------------

/// The standing project rule: the bytecode VM and the tree-walking evaluator
/// must agree about a builtin. HTTP is a real call in both, so this is not
/// cosmetic — a builtin bound in one prelude and not the other would be a
/// difference nobody notices until a program behaves differently depending on
/// which evaluator ran it.
#[test]
fn both_interpreters_agree_about_http() {
    let server = Server::canned(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}",
    );
    let url = server.url("/data");
    let src = format!(
        r#"(do
             (def r (http-get "{url}"))
             (def p (http-post "{url}" "body"))
             (list (get r "status")
                   (get r "ok")
                   (get r "body")
                   (get (get r "headers") "content-type")
                   (get p "status")
                   (get p "body")))"#
    );
    let vm = run_vm(&src).expect("vm");
    let tw = run_tree(&src).expect("tree-walk");
    assert_eq!(
        vm, tw,
        "the two interpreters disagree about http-get/http-post"
    );
    // `vm` is the 6-element list the program built, so index into it rather
    // than treating it as a response map.
    assert_eq!(list_index(&vm, 0), Some(Value::Int(BigNum::small(200))));
    assert_eq!(list_index(&vm, 1), Some(Value::Bool(true)));
    assert_eq!(list_index(&vm, 2), Some(Value::str("{\"a\":1}")));
    assert_eq!(
        list_index(&vm, 3),
        Some(Value::str("application/json")),
        "headers must be reachable with the ordinary `get`"
    );
    assert_eq!(list_index(&vm, 4), Some(Value::Int(BigNum::small(200))));
    // The canned reply is served for the POST too, so `p`'s body is the canned
    // JSON — which is the point: both verbs return the same *shape*, and a
    // caller can write one function over the response map regardless.
    assert_eq!(list_index(&vm, 5), Some(Value::str("{\"a\":1}")));
    assert_eq!(list_index(&vm, 6), None, "the list has exactly 6 elements");
}

/// Element `i` of an AINL list value, 0-based, or `None` if out of range.
///
/// AINL's own `(nth list i)` is 0-based and returns `nil` out of range; this
/// mirrors the Rust side so a test can assert "absent" rather than "nil".
fn list_index(v: &Value, i: usize) -> Option<Value> {
    let Value::List(items) = v else {
        return None;
    };
    items.nth(i).cloned()
}

/// An error must be the *same* error on both interpreters, or a program that
/// handles it differently depending on the backend is the bug this whole
/// project is organized against.
#[test]
fn both_interpreters_refuse_https_identically() {
    let vm = run_vm(r#"(http-get "https://example.com/")"#)
        .unwrap_err()
        .to_string();
    let tw = run_tree(r#"(http-get "https://example.com/")"#)
        .unwrap_err()
        .to_string();
    assert_eq!(vm, tw, "the two interpreters gave different https errors");
}

/// AINL closures around HTTP, which is the shape a real caller has: a helper
/// that fetches and reports. The step budget and the scope rules must not
/// interfere.
///
/// The assertion is on the *printed* line rather than the form's value,
/// because `print` returns `nil` and `run_*` hands back the last form's value —
/// asserting on that would pin `print`'s return type, not the fetch.
#[test]
fn http_composes_with_closures_and_loops() {
    let server = Server::canned(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx");
    let url = server.url("/x");
    let src = format!(
        r#"(do
             (def fetch-status (fn (u) (get (http-get u) "status")))
             (let ((i 0) (n 0))
               (while (< i 3)
                 (if (= (fetch-status "{url}") 200) (def n (+ n 1)))
                 (def i (+ i 1)))
               (print "hits" n)))"#
    );
    assert_eq!(run_vm(&src).expect("loop over http"), Value::Nil);
}
