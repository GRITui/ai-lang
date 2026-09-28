#!/usr/bin/env python3
"""Check that `ainl gen`'s repair loop actually fires.

The real model is too reliable to fail on demand, so "the program ran" on the
first attempt proves nothing about the retry path. This check stands up a local
stub OpenAI-compatible endpoint that returns a program which is a GBNF member
but *fails when run*, and only returns a working one on the second call. The
command must then report the failure, feed model-readable text back, and succeed
on the retry.

It also checks the two honesty guarantees that a real backend cannot be asked
for on demand:

  * `--no-probe` must NOT claim the constraint was verified.
  * `--grammar-field none` must NOT claim a grammar was sent at all.

Run: python3 scripts/check-gen-repair.py
Needs no network and no API key.
"""
import http.server
import json
import os
import re
import socket
import subprocess
import sys
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "ainl"
if not BIN.exists():
    BIN = ROOT / "target" / "debug" / "ainl"
if not BIN.exists():
    sys.exit("FATAL: no ainl binary; run cargo build --release first")

# A GBNF member that dies at run time with a message the model can act on, and
# then the fixed program. Both are valid AINL; only the first one fails.
BAD = "(print (undefined-thing 1))\n"
GOOD = "(print (+ 2 3))\n"

STATE = {"calls": 0}
LOCK = threading.Lock()
FAILURES = []


class Stub(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        with LOCK:
            STATE["calls"] += 1
            n = STATE["calls"]
        body = BAD if n == 1 else GOOD
        out = json.dumps({
            "id": "stub-1",
            "object": "chat.completion",
            "model": "stub",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": body},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, format, *args):
        """Silence the default stderr access log."""


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run_gen(port, *extra):
    """Run `ainl gen` against the stub, with a throwaway key.

    The key is obviously fake and lives only in this subprocess's environment;
    it is never an argument, which is the same discipline the command itself
    documents.
    """
    env = dict(os.environ, AINL_GEN_STUB_CHECK_KEY="not-a-real-key-test-fixture")
    return subprocess.run(
        [str(BIN), "gen", "print the sum of 2 and 3",
         "--endpoint", f"http://127.0.0.1:{port}",
         "--model", "stub", "--api-key-env", "AINL_GEN_STUB_CHECK_KEY", *extra],
        capture_output=True, text=True, env=env, timeout=120,
    )


def check(name, cond, detail=""):
    mark = "ok  " if cond else "FAIL"
    print(f"{mark} {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        FAILURES.append(name)


def main():
    port = free_port()
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Stub)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        # --- the repair loop -------------------------------------------------
        STATE["calls"] = 0
        p = run_gen(
            port, "--grammar-field", "none", "--no-probe",
            "--run", "--attempts", "3", "--show-program",
        )
        out = p.stdout + p.stderr
        check("the repair loop recovers on the second attempt", p.returncode == 0, out[-400:])
        check("the stub was actually called twice", STATE["calls"] == 2, f"{STATE['calls']} calls")
        check("both attempts are shown", out.count("attempt") >= 2, out[-400:])
        check("the broken program is what was retried", BAD.strip() in out, out[-400:])
        check("the fixed program ran", re.search(r"^\s*5\s*$", out, re.M) is not None, out[-400:])

        # --- honesty: --no-probe must not claim verification -------------------
        STATE["calls"] = 0
        p2 = run_gen(port, "--no-probe", "--run", "--attempts", "2")
        out2 = p2.stdout + p2.stderr
        check("--no-probe does not claim the constraint was verified",
              "verified in force" not in out2, out2[-400:])
        check("--no-probe says the enforcement is unverified",
              "unverified" in out2, out2[-400:])

        # --- honesty: --grammar-field none must not claim a grammar ----------
        STATE["calls"] = 0
        p3 = run_gen(port, "--grammar-field", "none", "--run", "--attempts", "2")
        out3 = p3.stdout + p3.stderr
        check("--grammar-field none is not reported as constrained",
              "constrained yes" not in out3, out3[-400:])
        check("--grammar-field none says the run was unconstrained",
              "NOT grammar-constrained" in out3, out3[-400:])
    finally:
        srv.shutdown()

    print()
    if FAILURES:
        print(f"RESULT: FAIL — {len(FAILURES)} check(s) failed: {', '.join(FAILURES)}")
        return 1
    print("RESULT: PASS — the repair loop fires and the constraint report is honest")
    return 0


if __name__ == "__main__":
    sys.exit(main())
