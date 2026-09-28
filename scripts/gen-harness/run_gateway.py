#!/usr/bin/env python3
"""AINL constrained-decoding run against a REMOTE OpenAI-compatible gateway
(card Stage 3.4 — the zOnya gateway / Qwen3.8-27B-FP8).

This is the cloud counterpart of run_checkable.py. Same three metrics, same
sound detector, same valid-prefix artifact rule — but the model runs on a
remote vLLM server behind an OpenAI-compatible API instead of local
llama-cli.

Three facts about this gateway were established empirically and are encoded
here, because each one silently corrupts results if missed:

  1. **User-Agent is mandatory.** The default urllib UA (`Python-urllib/…`)
     is rejected at the Cloudflare edge with Error 1010 *before* auth runs.
     That looks exactly like a bad key but is not one. We always send a
     browser UA.

  2. **Constrained decoding is `structured_outputs.grammar`, not
     `guided_grammar`.** On this vLLM build `guided_grammar` returns HTTP 200
     and is SILENTLY IGNORED — worse than an error, because the run looks
     successful while measuring nothing. Verified with an impossible grammar
     (`root ::= "Z"`): a working constraint returns exactly "Z" even when
     asked for an essay; an ignored one returns prose.

  3. **The model is a reasoning model.** It burns the first tokens of the
     budget on `reasoning_content` and returns `content: null` with
     finish_reason=length when the budget is too small. We disable thinking and
     keep enough budget to see the real failure mode rather than a budget
     artifact.

Usage:
  python3 scripts/gen-harness/run_gateway.py \
      [--model qwen3.8-27b-fp8] \
      [--suite scripts/gen-harness/suite_checkable.json] \
      [--out results-gateway-qwen3.8-27b] \
      [--ainl target/release/ainl] \
      [--limit N] [--max-tokens 1200] [--timeout 300] [--retries 3]

The key is read from $HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY and is never
printed, logged, or written to any output file.
"""
import argparse
import csv
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
from gbnf_fast import ainl_gbnf_accepts  # noqa: E402
from gbnf_prefix import prefix_report  # noqa: E402

BASE = "https://gateway.9arm.co"
UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")


def key():
    k = os.environ.get("HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY", "")
    if not k:
        raise SystemExit("FATAL: $HERMES_CUSTOM_GATEWAY_9ARM_CO_API_KEY is not "
                         "set. Do not paste a key into this file; export it.")
    return k


def post(payload, timeout, retries):
    """POST a chat completion. Retries transient failures only (429/5xx/timeouts).
    An auth failure is NOT retried — it needs a human."""
    last = None
    for attempt in range(1, retries + 1):
        req = urllib.request.Request(
            BASE + "/v1/chat/completions",
            data=json.dumps(payload).encode(), method="POST")
        req.add_header("Authorization", "Bearer " + key())
        req.add_header("Content-Type", "application/json")
        req.add_header("User-Agent", UA)  # see note 1
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, json.loads(r.read().decode("utf-8", "replace"))
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", "replace")
            if e.code in (401, 403):
                raise SystemExit("FATAL: gateway rejected the key (%d): %s"
                                 % (e.code, body[:200]))
            last = "HTTP %d: %s" % (e.code, body[:200])
        except Exception as e:  # timeout / connection reset / DNS
            last = "%s: %s" % (type(e).__name__, e)
        if attempt < retries:
            back = min(2 ** attempt, 20)
            print("      retry %d/%d after %ds (%s)"
                  % (attempt, retries, back, last), flush=True)
            time.sleep(back)
    raise RuntimeError("gateway failed after %d attempts: %s" % (retries, last))


def generate(prompt, grammar, model, max_tokens, timeout, retries):
    """One generation. Returns (content, finish_reason, reasoning_len, usage)."""
    body = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "max_tokens": max_tokens,
        "chat_template_kwargs": {"enable_thinking": False},
    }
    if grammar:
        # see note 2 — this is the parameter that actually constrains
        body["structured_outputs"] = {"grammar": grammar}
    st, r = post(body, timeout, retries)
    if st != 200 or not isinstance(r, dict):
        return None, "error", 0, {}
    ch = (r.get("choices") or [{}])[0]
    msg = ch.get("message") or {}
    return (msg.get("content"), ch.get("finish_reason"),
            len(msg.get("reasoning_content") or ""), r.get("usage", {}))


def ainl_eval(ainl: Path, text: str):
    """(parse_ok, run_ok, run_out, run_err) against the real ainl binary."""
    p = ainl.parent / "_eval_tmp.ainl"
    p.write_text(text)
    try:
        ast = subprocess.run([str(ainl), "ast", str(p)],
                             capture_output=True, text=True)
        run = subprocess.run([str(ainl), "run", str(p)],
                             capture_output=True, text=True)
    finally:
        p.unlink(missing_ok=True)
    return (ast.returncode == 0, run.returncode == 0,
            run.stdout.strip(), run.stderr.strip())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="qwen3.8-27b-fp8")
    ap.add_argument("--base", default=BASE)
    ap.add_argument("--ainl", default=str(ROOT / "target" / "release" / "ainl"))
    ap.add_argument("--suite", default=str(HERE / "suite_checkable.json"))
    ap.add_argument("--out", default=None)
    ap.add_argument("--max-tokens", type=int, default=1200)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--retries", type=int, default=6)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--modes", default="constrained,unconstrained",
                    help="comma list; 'constrained' applies the AINL GBNF")
    ap.add_argument("--resume", action="store_true",
                    help="reuse generations already recorded in results.jsonl")
    ap.add_argument("--rescore-only", action="store_true",
                    help="re-score results.jsonl on disk without calling the API")
    args = ap.parse_args()

    ainl = Path(args.ainl)
    if not ainl.exists():
        raise SystemExit("FATAL: ainl binary not found: %s "
                         "(run cargo build --release)" % ainl)
    suite = json.loads(Path(args.suite).read_text())
    if args.limit:
        suite = suite[:args.limit]
    modes = [m.strip() for m in args.modes.split(",") if m.strip()]

    out = Path(args.out) if args.out else HERE / ("results-gateway-" + args.model)
    for m in modes:
        (out / m).mkdir(parents=True, exist_ok=True)
    # Append-only record, flushed after every prompt. A remote gateway WILL
    # time out eventually (observed: HTTP 524 mid-suite); without this a
    # 14-minute run vanishes because the last prompt was unlucky.
    ledger = out / "results.jsonl"

    gbnf = subprocess.run([str(ainl), "grammar", "--gbnf"],
                          capture_output=True, text=True, check=True).stdout
    print("model        :", args.model)
    print("grammar bytes:", len(gbnf))
    print("prompts      :", len(suite), " modes:", modes)
    print("out          :", out)
    print("ledger       :", ledger)
    print("=" * 74, flush=True)

    def record(row):
        with open(ledger, "a") as fh:
            fh.write(json.dumps(row) + "\n")

    done = {}
    if ledger.exists():
        for line in ledger.read_text().splitlines():
            if not line.strip():
                continue
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue  # truncated final line from a hard kill
            done[(r["id"], r["mode"])] = r
        if done:
            print("resuming: %d generation(s) already on disk"
                  % len(done), flush=True)

    if args.rescore_only:
        rows = [r for r in done.values() if r["mode"] == modes[-1]]
        results = finalize(suite, done, modes)
        write_outputs(out, results, modes, args.model)
        return

    results = []
    for i, item in enumerate(suite):
        prompt, expected = item["prompt"], item["expected"]
        pid = item.get("id", str(i))
        row = {"index": i, "id": pid, "prompt": prompt, "expected": expected}
        for mode in modes:
            grammar = gbnf if mode == "constrained" else None
            if args.resume and (pid, mode) in done:
                row[mode] = done[(pid, mode)][mode]
                print("[%-2s %-13s] cached" % (mode[0].upper(), pid), flush=True)
                continue
            gen, fin, rlen, usage = generate(prompt, grammar, args.model,
                                             args.max_tokens, args.timeout,
                                             args.retries)
            truncated = (fin == "length")
            (out / mode / (pid + ".ainl")).write_text(gen or "")
            gbnf_ok = ainl_gbnf_accepts(gen) if gen else False
            pre = prefix_report(gen or "", truncated)
            if gen:
                p_ok, r_ok, r_out, r_err = ainl_eval(ainl, gen)
            else:
                p_ok = r_ok = False
                r_out, r_err = "", "(no content returned)"
            # "correct" is judged on the whole output, but a truncated
            # generation is credited from its valid prefix (artifact rule).
            judged = gen or ""
            if not gbnf_ok and pre["kind"] == "valid_prefix":
                judged = pre["prefix_text"]
            j_ok = j_out = j_err = False
            if judged:
                _, jr_ok, j_out, j_err = ainl_eval(ainl, judged)
                j_ok = jr_ok
            row[mode] = {
                "generated": gen, "finish_reason": fin,
                "reasoning_len": rlen, "usage": usage,
                "gbnf_member": gbnf_ok, "parse_ok": p_ok, "run_ok": r_ok,
                "run_out": r_out, "run_err": r_err,
                "correct": bool(j_ok and j_out == expected),
                "judged_text": judged,
                "prefix": {k: v for k, v in pre.items() if k != "prefix_text"},
            }
            record({"id": pid, "index": i, "mode": mode, "expected": expected,
                    mode: row[mode]})
            print("[%-2s %-13s] gbnf=%s runs=%s correct=%-3s fin=%-7s "
                  "prefix=%-16s %s"
                  % (mode[0].upper(), pid, "Y" if gbnf_ok else "N",
                     "Y" if r_ok else "N", "Y" if row[mode]["correct"] else "N",
                     fin, pre["kind"], repr(gen)[:46]), flush=True)
        results.append(row)

    results = finalize(suite, done, modes, results)
    write_outputs(out, results, modes, args.model)


def finalize(suite, done, modes, results=None):
    """Merge the ledger with this run's rows, one row per prompt, in suite order."""
    merged = []
    for i, item in enumerate(suite):
        pid = item.get("id", str(i))
        if results:
            hit = next((r for r in results if r["id"] == pid), None)
            if hit:
                merged.append(hit)
                continue
        row = {"index": i, "id": pid, "prompt": item["prompt"],
               "expected": item["expected"]}
        for mode in modes:
            hit = done.get((pid, mode))
            if hit:
                row[mode] = hit[mode]
        if all(m in row for m in modes):
            merged.append(row)
    return merged


def write_outputs(out: Path, results, modes, model):
    (out / "results.json").write_text(json.dumps(results, indent=2))
    with open(out / "results.csv", "w", newline="") as f:
        cols = ["index", "id", "prompt", "expected"]
        for m in modes:
            cols += [m + "_" + k for k in
                     ("gbnf", "runs", "correct", "finish", "prefix_kind",
                      "coverage")]
        w = csv.writer(f)
        w.writerow(cols)
        for r in results:
            row = [r["index"], r["id"], r["prompt"], r["expected"]]
            for m in modes:
                d = r[m]
                row += [d["gbnf_member"], d["run_ok"], d["correct"],
                        d["finish_reason"], d["prefix"]["kind"],
                        round(d["prefix"]["coverage"], 3)]
            w.writerow(row)

    n = len(results) or 1
    print()
    print("=" * 74)
    print("RESULTS  n=%d  model=%s" % (len(results), model))
    print("=" * 74)
    for m in modes:
        g = sum(1 for r in results if r[m]["gbnf_member"])
        ru = sum(1 for r in results if r[m]["run_ok"])
        co = sum(1 for r in results if r[m]["correct"])
        uniq = len({r[m]["generated"] for r in results})
        vp = sum(1 for r in results if r[m]["prefix"]["kind"] == "valid_prefix")
        print("  %-14s valid %2d/%d (%3.0f%%)  runs %2d/%d (%3.0f%%)  "
              "correct %2d/%d (%3.0f%%)  distinct %2d  truncated %d"
              % (m, g, len(results), 100 * g / n, ru, len(results), 100 * ru / n,
                 co, len(results), 100 * co / n, uniq, vp))
    print("=" * 74)


if __name__ == "__main__":
    main()
