#!/usr/bin/env python3
"""A tiny local HTTP server for the AINL HTTP demo. No dependencies.

    python3 examples/http/server.py 8080 &
    ainl run examples/http/http-demo.ainl

Why this file is Python and not AINL: AINL's HTTP client is the thing being
demonstrated, so the server has to be something AINL did not produce. AINL has
no server-side listening support either — `http-get`/`http-post` are clients,
and adding a listener is a separate feature (see docs/BACKLOG.md).

Every response declares its own Content-Length from the body it is sending.
Hand-typed lengths are the classic bug in a hand-written HTTP fixture: too
short and the client's leftover bytes look like the start of a second response,
too long and the client waits forever for bytes that never come.
"""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    # Requests are logged to stderr, never stdout, so a test harness can
    # redirect stdout and parse only the AINL program's own output.
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
            self._send(200, '[{"id": 1}]', "application/json")
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
            self._send(404, "not here")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length).decode("utf-8")
        self._send(201, "created: " + body, "application/json")

    def log_message(self, format, *args):
        # The base class names this parameter `format`; keeping the name
        # matters (Pyright flags an incompatible override, and Python would
        # bind positionally either way).
        sys.stderr.write("server: " + (format % args) + "\n")


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    # Loopback only: this is a demo fixture, and a server that binds 0.0.0.0
    # would be listening on every interface of whatever machine ran it.
    server = HTTPServer(("127.0.0.1", port), Handler)
    sys.stderr.write(f"server: listening on http://127.0.0.1:{port}\n")
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
