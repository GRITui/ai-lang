#!/usr/bin/env python3
"""Loopback HTTP fixture for examples/corpus/http-get-json.ainl. No deps.

    python3 examples/corpus/http-fixture/server.py 8080 &
    AINL_PORT=8080 ainl run examples/corpus/http-get-json.ainl

Why this file is Python and not AINL: the thing being demonstrated is AINL's
HTTP CLIENT, so the server has to be something AINL did not produce. AINL has
no server-side listening support either — `http-get`/`http-post` are clients,
and a listener is a separate feature (see docs/BACKLOG.md).

Why it is a FIXTURE and not a mock: the AINL client is hand-written HTTP/1.1,
and the things most likely to be wrong in a hand-written client are the parts
a real server exercises that a mock would paper over. So this sends a real
Content-Length, a real chunked body, and a real 404. Every response declares
its own Content-Length from the body it is sending — a hand-typed length is
the classic fixture bug: too short and the client's leftover bytes look like
the start of a second response, too long and the client waits forever.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

ITEMS = [{"id": 1, "name": "widget", "qty": 2}, {"id": 2, "name": "gadget", "qty": 5}]


class Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 keep-alive, so the client must handle a connection that stays
    # open after the response — a real difference from HTTP/1.0.
    protocol_version = "HTTP/1.1"

    def _send(self, code, body, ctype="text/plain; charset=utf-8"):
        raw = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path.startswith("/health"):
            self._send(200, "ok")
        elif self.path.startswith("/items"):
            # Echo back the request's own X-Client header, so the example can
            # assert that a caller-supplied header survived the round trip
            # rather than only asserting that the fetch worked.
            seen = self.headers.get("X-Client", "")
            payload = json.dumps({"data": ITEMS, "client": seen})
            self._send(200, payload, "application/json")
        elif self.path.startswith("/chunked"):
            # Chunked, because a real server uses it for a body of unknown
            # length and AINL reassembles it.
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for part in (b"one ", b"two ", b"three"):
                self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
            self.wfile.write(b"0\r\n\r\n")
        else:
            # A real 404, so the "a non-2xx status is a value, not an error"
            # rule in the example is demonstrated rather than asserted.
            self._send(404, "not here")

    def log_message(self, format, *args):
        # The base class names this parameter `format`; keeping the name
        # matters (Pyright flags an incompatible override, and Python would
        # bind positionally either way).
        # stderr, never stdout, so a harness can parse only the AINL program's
        # own output.
        sys.stderr.write("fixture: " + (format % args) + "\n")


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    # Loopback only: a fixture that binds 0.0.0.0 would be listening on every
    # interface of whatever machine ran it.
    server = HTTPServer(("127.0.0.1", port), Handler)
    sys.stderr.write(f"fixture: listening on http://127.0.0.1:{port}\n")
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
