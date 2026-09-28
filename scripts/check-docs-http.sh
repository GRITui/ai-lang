#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3c against the real binary
# and a real local server.
#
# The same reasoning as check-docs-import.sh: doc claims written from memory are
# the ones that rot, and on the first pass several of these were wrong. This
# turns the section from prose into a test — the TLS refusal, the response
# shape, the 404-is-a-value rule, the reserved headers, and the backend
# refusals.
#
# No external network: the server is a stdlib Python one bound to loopback on a
# port the OS picks. Exits non-zero on any failure.
set -uo pipefail
cd "$(dirname "$0")/.."

B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
[ -f examples/http/server.py ] || { echo "FAIL: examples/http/server.py is missing"; exit 1; }

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
BASE="http://127.0.0.1:$PORT"
D=$(mktemp -d)
python3 examples/http/server.py "$PORT" >/dev/null 2>"$D/server.log" &
SRV=$!
# Wait for the listener to actually accept, rather than sleeping a guessed
# interval: a fixed sleep is a flaky gate, and this one runs in CI.
for _ in $(seq 1 100); do
  if python3 -c "
import socket,sys
s=socket.socket()
s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$PORT))==0 else 1)
" 2>/dev/null; then break; fi
done
trap 'kill $SRV 2>/dev/null; rm -rf "$D"' EXIT

fail=0
# want <label> <expected> <expr> — evaluates an AINL expression, expects its value.
want() {
  local label="$1" expected="$2" expr="$3"
  local got
  got=$("$B" eval "$expr" 2>&1)
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "       want: $expected"
    echo "       got:  $got"
    fail=1
  fi
}
# rejects <label> <needle> <expr> — expects a runtime error containing needle.
rejects() {
  local label="$1" needle="$2" expr="$3"
  local got
  got=$("$B" eval "$expr" 2>&1)
  if printf '%s' "$got" | grep -q "$needle"; then
    echo "ok   $label"
  else
    echo "FAIL $label (wanted /$needle/)"
    echo "       got: $got"
    fail=1
  fi
}

echo "== the response is an ordinary map =="
want "status is an int"      "200" "(get (http-get \"$BASE/health\") \"status\")"
want "ok is true for 200"    "true" "(get (http-get \"$BASE/health\") \"ok\")"
want "body is the payload"   "ok" "(get (http-get \"$BASE/health\") \"body\")"
want "headers is a nested map, reachable with get" "text/plain; charset=utf-8" \
  "(get (get (http-get \"$BASE/health\") \"headers\") \"content-type\")"
want "reason is the status text" "OK" "(get (http-get \"$BASE/health\") \"reason\")"
want "the whole response is a map (keys works)" "true" \
  "(= (first (keys (http-get \"$BASE/health\"))) \"status\")"

echo
echo "== header names are lowercased =="
want "an uppercase request header round-trips in lowercase" "application/json" \
  "(get (get (http-get \"$BASE/items\" (hash \"Accept\" \"application/json\")) \"headers\") \"content-type\")"

echo
echo "== a non-2xx status is a value, not an error =="
want "a 404 returns, with ok false" "false" "(get (http-get \"$BASE/nope\") \"ok\")"
want "a 404's status is the code"  "404"   "(get (http-get \"$BASE/nope\") \"status\")"
want "and it branches like any value" "missing" \
  "(if (get (http-get \"$BASE/nope\") \"ok\") \"found\" \"missing\")"

echo
echo "== http-post sends a body and a caller header =="
want "POST returns 201" "201" \
  "(get (http-post \"$BASE/items\" \"{\\\"name\\\": \\\"widget\\\"}\" (hash \"Content-Type\" \"application/json\")) \"status\")"
want "the server received the body" "true" \
  "(contains (get (http-post \"$BASE/items\" \"{\\\"name\\\": \\\"widget\\\"}\") \"body\") \"widget\")"

echo
echo "== chunked transfer-encoding is reassembled =="
want "a chunked body arrives whole" "one two three" \
  "(get (http-get \"$BASE/chunked\") \"body\")"

echo
echo "== TLS: https is refused by name, before any connection =="
rejects "https is not supported" "not supported" \
  "(http-get \"https://example.com/\")"
rejects "the message names the http:// fix" "http://example.com/" \
  "(http-get \"https://example.com/\")"
rejects "https is refused for POST too" "not supported" \
  "(http-post \"https://no-such-host.invalid/\" \"b\")"
rejects "and it is refused BEFORE resolving the host (no DNS error)" "not supported" \
  "(http-get \"https://this-host-does-not-exist.invalid/x\")"
rejects "a missing scheme is refused" "http://" "(http-get \"example.com/\")"

echo
echo "== the reserved headers and smuggling vectors are refused =="
rejects "Host cannot be overridden"       "Host" \
  "(http-get \"$BASE/health\" (hash \"Host\" \"evil.example\"))"
rejects "Content-Length cannot be overridden" "Content-Length" \
  "(http-post \"$BASE/items\" \"b\" (hash \"Content-Length\" \"9999\"))"
rejects "a CRLF in a header value is refused" "CR or LF" \
  "(http-get \"$BASE/health\" (hash \"X\" \"a\r\nX-Evil: 1\"))"
rejects "credentials in a URL are refused" "credentials" \
  "(http-get \"http://user:pw@example.com/\")"

echo
echo "== wrong arguments are refused without touching the network =="
rejects "a non-str URL" "expects a str URL" "(http-get 42)"
rejects "http-get arity" "http-get expects" "(http-get \"$BASE/x\" \"a\" \"b\")"
rejects "http-post arity" "http-post expects" "(http-post \"$BASE/x\")"
rejects "headers must be a map" "hash of headers" \
  "(http-get \"$BASE/x\" \"not a map\")"

echo
echo "== a body that is not valid utf-8 cannot be a str =="
# The stdlib server will happily send bytes that are not utf-8 if asked for a
# path that does this; the demo server does not, so this is asserted through
# the Rust suite (tests/http.rs) rather than here. Placeholder kept explicit so
# a future reader does not assume it was forgotten:
echo "ok   (covered by crates/ainl-core/tests/http.rs: a_non_utf8_body_is_refused)"

echo
echo "== the AOT backend and all three transpilers REFUSE http =="
printf '(print (get (http-get "%s/health") "status"))\n' "$BASE" > "$D/h.ainl"
if out=$("$B" compile "$D/h.ainl" -o "$D/h.aot" 2>&1); then
  echo "FAIL: ainl compile accepted a program with http-get"
  fail=1
elif ! printf '%s' "$out" | grep -q 'interpreter-only'; then
  echo "FAIL: AOT refused, but not for the right reason:"
  printf '%s\n' "$out" | sed 's/^/    /'
  fail=1
else
  echo "ok   AOT refused: $(printf '%s' "$out" | head -1 | cut -c1-90)…"
fi
for target in python js ruby; do
  if out=$("$B" transpile "$D/h.ainl" --to "$target" 2>&1); then
    echo "FAIL: transpile --to $target accepted a program with http-get"
    fail=1
  elif ! printf '%s' "$out" | grep -q 'interpreter-only'; then
    echo "FAIL: --to $target refused, but not for the right reason:"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok   $target refused"
  fi
done
# `print` returns nil, and `ainl eval` prints the snippet's final value after
# the program's own output — so the expected text is two lines, not one.
want "a QUOTED http-get is data, not a directive" \
  "$(printf '(http-get "%s/x")\nnil' "$BASE")" \
  "(print (quote (http-get \"$BASE/x\")))"

echo
echo "== a program with NO http is unaffected on every backend =="
printf '(print (+ 1 2))\n' > "$D/plain.ainl"
if got=$("$B" run "$D/plain.ainl" 2>&1) && [ "$got" == "3" ]; then
  echo "ok   interpreter still fine"
else
  echo "FAIL: interpreter broke on a plain program (got: $got)"
  fail=1
fi
if "$B" transpile "$D/plain.ainl" --to python > "$D/plain.py" 2>"$D/err"; then
  echo "ok   transpiler still fine"
else
  echo "FAIL: transpiler broke on a plain program:"
  sed 's/^/    /' "$D/err"
  fail=1
fi

echo
echo "== the example runs against the real server =="
if out=$(AINL_DEMO_PORT="$PORT" "$B" run examples/http/http-demo.ainl 2>&1); then
  # Assert on shape, not exact text: the demo prints seven lines and the point
  # is that it completes and reports the statuses it fetched.
  lines=$(printf '%s\n' "$out" | grep -c '[^[:space:]]')
  if [ "$lines" -lt 6 ]; then
    echo "FAIL: expected the demo to print its results, got $lines lines:"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok   example ran, $lines lines"
  fi
else
  echo "FAIL: examples/http/http-demo.ainl did not run:"
  printf '%s\n' "$out" | sed 's/^/    /'
  fail=1
fi

echo
[ "$fail" -eq 0 ] && echo "SYNTAX 3c: every doc claim verified" || echo "SYNTAX 3c: SOME DOC CLAIMS ARE WRONG"
exit "$fail"
