#!/usr/bin/env python3
"""Score generations already on disk, without calling the gateway.

The first full-suite run wrote its raw .ainl files but crashed on a gateway 524
before results.json existed (the JSONL ledger was added after that run). Rather
than spend 15 more minutes of a very slow gateway re-generating identical
output, this re-scores the saved artifacts: GBNF membership, ainl ast, ainl run,
stdout correctness, and the truncation/valid-prefix classification.

The .ainl files are the primary record; this reproduces the harness's verdict
from them so the two paths cannot disagree about what was produced.
"""
import csv
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
from gbnf_fast import ainl_gbnf_accepts  # noqa: E402
from gbnf_prefix import prefix_report  # noqa: E402

AINL = ROOT / "target" / "release" / "ainl"
OUT = HERE / "results-gateway-qwen3.8-27b"
MODES = ("constrained", "unconstrained")


def eval_one(text):
    p = Path("/tmp/_score_tmp.ainl")
    p.write_text(text)
    try:
        ast = subprocess.run([str(AINL), "ast", str(p)],
                             capture_output=True, text=True)
        run = subprocess.run([str(AINL), "run", str(p)],
                             capture_output=True, text=True)
    finally:
        p.unlink(missing_ok=True)
    return ast.returncode == 0, run.returncode == 0, run.stdout.strip(), run.stderr.strip()


def main():
    suite = {i["id"]: i for i in
             json.loads((HERE / "suite_checkable_grounded.json").read_text())}
    rows = []
    for pid, item in suite.items():
        row = {"id": pid, "expected": item["expected"], "raw_prompt":
               item.get("raw_prompt", "")}
        ok = True
        for mode in MODES:
            f = OUT / mode / (pid + ".ainl")
            if not f.exists():
                ok = False
                break
            gen = f.read_text()
            truncated = not ainl_gbnf_accepts(gen)
            pre = prefix_report(gen, truncated)
            judged = gen if ainl_gbnf_accepts(gen) else (
                pre["prefix_text"] if pre["kind"] == "valid_prefix" else "")
            p_ok, r_ok, r_out, r_err = (eval_one(gen) if gen else (False, False, "", ""))
            if judged and judged != gen:
                _, jr, jo, je = eval_one(judged)
            else:
                jr, jo, je = r_ok, r_out, r_err
            row[mode] = {
                "generated": gen, "gbnf_member": ainl_gbnf_accepts(gen),
                "parse_ok": p_ok, "run_ok": r_ok, "run_out": r_out, "run_err": r_err,
                "correct": bool(jr and jo == item["expected"]),
                "prefix": {k: v for k, v in pre.items() if k != "prefix_text"},
            }
        if ok:
            rows.append(row)

    (OUT / "results.json").write_text(json.dumps(rows, indent=2))
    with open(OUT / "results.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["id", "expected",
                    "c_gbnf", "c_run", "c_correct", "c_out", "c_prefix",
                    "u_gbnf", "u_run", "u_correct", "u_out", "u_prefix"])
        for r in rows:
            c, u = r["constrained"], r["unconstrained"]
            w.writerow([r["id"], r["expected"],
                        c["gbnf_member"], c["run_ok"], c["correct"], c["run_out"],
                        c["prefix"]["kind"],
                        u["gbnf_member"], u["run_ok"], u["correct"], u["run_out"],
                        u["prefix"]["kind"]])

    n = len(rows)
    print("=" * 96)
    print("%-16s %-8s | %-28s | %-28s |" % ("id", "expected",
                                            "CONSTRAINED", "UNCONSTRAINED"))
    print("%-16s %-8s | %-6s %-5s %-6s %-8s | %-6s %-5s %-6s %-8s |"
          % ("", "", "gbnf", "run", "ok", "out", "gbnf", "run", "ok", "out"))
    print("-" * 96)
    for r in rows:
        c, u = r["constrained"], r["unconstrained"]
        print("%-16s %-8s | %-6s %-5s %-6s %-8s | %-6s %-5s %-6s %-8s |"
              % (r["id"], r["expected"],
                 "Y" if c["gbnf_member"] else "N", "Y" if c["run_ok"] else "N",
                 "Y" if c["correct"] else "N", (c["run_out"] or "-")[:8],
                 "Y" if u["gbnf_member"] else "N", "Y" if u["run_ok"] else "N",
                 "Y" if u["correct"] else "N", (u["run_out"] or "-")[:8]))

    print("-" * 96)
    for mode in MODES:
        g = sum(1 for r in rows if r[mode]["gbnf_member"])
        ru = sum(1 for r in rows if r[mode]["run_ok"])
        co = sum(1 for r in rows if r[mode]["correct"])
        uq = len({r[mode]["generated"] for r in rows})
        print("%-14s valid %2d/%d (%3.0f%%)   runs %2d/%d (%3.0f%%)   "
              "correct %2d/%d (%3.0f%%)   distinct %d"
              % (mode, g, n, 100 * g / n, ru, n, 100 * ru / n,
                 co, n, 100 * co / n, uq))
    print("=" * 96)
    print("note: n=%d of %d suite prompts have artifacts on disk; "
          "map-ada and last-1-2-3-4 never completed (gateway 524 storm)."
          % (n, len(suite)))


if __name__ == "__main__":
    main()
