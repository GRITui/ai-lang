#!/usr/bin/env python3
"""Constraint-decoding smoke test for the DENSE-AINL GBNF (spike t_4bb53833).

Mirrors scripts/gen-harness/run_generation.py's method, but for the dense
grammar: run a small local model through a REAL GBNF-constrained decoder
(llama.cpp `llama-cli --grammar-file`) using the hand-authored dense GBNF, in
TWO modes (constrained / unconstrained), and for each generation check GBNF
membership with the SOUND fast detector (spike/dense_fast.py — cross-validated
against the reference Earley).

The headline number: constrained GBNF-membership-rate vs unconstrained. If the
constraint is applied, the decoder can only emit GBNF-accepted strings, so
constrained output must be a dense-GBNF member while unconstrained output
(free-form) generally is not. The 0.5B model is too weak for semantics, but the
SYNTAX check is exactly what tells us whether the denser grammar is still
constrainable in a real decoder.

Usage:
    python3 spike/dense_smoke.py \
        --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
        [--prompts spike/dense-prompts.txt] [--out spike/results-dense] \
        [--max-tokens 256] [--threads 8] [--limit N]
"""
import argparse
import csv
import json
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from dense_fast import dense_gbnf_accepts  # noqa: E402

GBNF_PATH = HERE / "dense-ainl" / "dense.ainl.gbnf"


def load_prompts(path: Path) -> list:
    lines = [l.rstrip("\n") for l in path.read_text().splitlines()]
    return [l for l in lines if l.strip()]


def run_llama(model: Path, prompt: str, grammar_file, max_tokens: int,
              threads: int) -> str:
    """One llama-cli single-turn completion. Returns the generated text."""
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as pp, \
         tempfile.NamedTemporaryFile("w", suffix=".out", delete=False) as op:
        pp.write(prompt + "\n")
        pp.close()
        cmd = ["llama-cli", "-m", str(model), "-f", pp.name,
               "--chat-template", "qwen", "-n", str(max_tokens),
               "--temp", "0", "--no-display-prompt", "--no-warmup",
               "--threads", str(threads), "-st"]
        if grammar_file:
            cmd += ["--grammar-file", str(grammar_file)]
        subprocess.run(cmd, stdout=open(op.name, "w"),
                       stderr=subprocess.DEVNULL, timeout=300)
        txt = open(op.name).read()
    return extract_generated(txt, prompt)


def extract_generated(txt: str, prompt: str) -> str:
    """Generated text = everything after the prompt-echo line ('> <prompt>')
    and before the timing line ('[ Prompt: ... ]'). llama-cli 0.5.0 prints a
    banner before the echo, so we drop everything up to and including it."""
    end = txt.find("[ Prompt:")
    body = txt[:end] if end >= 0 else txt
    lines = body.splitlines(keepends=True)
    for idx, line in enumerate(lines):
        if line.startswith("> ") and prompt in line:
            return "".join(lines[idx + 1:])
    return body


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--prompts", default=str(HERE / "dense-prompts.txt"))
    ap.add_argument("--out", default=str(HERE / "results-dense"))
    ap.add_argument("--max-tokens", type=int, default=256)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--limit", type=int, default=0)
    args = ap.parse_args()

    model = Path(args.model)
    if not model.exists():
        raise SystemExit(f"FATAL: model not found: {model}")
    if not GBNF_PATH.exists():
        raise SystemExit(f"FATAL: dense GBNF not found: {GBNF_PATH}")
    gbnf_path = GBNF_PATH

    prompts = load_prompts(Path(args.prompts))
    if args.limit:
        prompts = prompts[:args.limit]

    out = Path(args.out)
    (out / "constrained").mkdir(parents=True, exist_ok=True)
    (out / "unconstrained").mkdir(parents=True, exist_ok=True)

    results = []
    for i, prompt in enumerate(prompts):
        row = {"index": i, "prompt": prompt}
        for mode, grammar in (("constrained", gbnf_path),
                              ("unconstrained", None)):
            gen = run_llama(model, prompt, grammar, args.max_tokens,
                            args.threads)
            (out / mode / f"{i}.ainl").write_text(gen)
            gbnf_ok = dense_gbnf_accepts(gen)
            row[mode] = {"generated": gen, "gbnf_member": gbnf_ok}
            tag = "C" if mode == "constrained" else "U"
            print(f"[{tag} {i:2}] gbnf={'Y' if gbnf_ok else 'N'}  "
                  f"{prompt[:40]!r}", flush=True)
        results.append(row)

    (out / "results.json").write_text(json.dumps(results, indent=2))
    with open(out / "results.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["index", "prompt", "constrained_gbnf",
                    "unconstrained_gbnf"])
        for r in results:
            w.writerow([r["index"], r["prompt"],
                        r["constrained"]["gbnf_member"],
                        r["unconstrained"]["gbnf_member"]])

    n = len(results)

    def rate(mode):
        return sum(1 for r in results if r[mode]["gbnf_member"])

    cg, ug = rate("constrained"), rate("unconstrained")
    print("\n" + "=" * 64)
    print(f"DENSE SMOKE  (n={n} prompts, model={model.name})")
    print("=" * 64)
    print(f"  constrained   : gbnf {cg}/{n} ({100*cg/n:.0f}%)")
    print(f"  unconstrained : gbnf {ug}/{n} ({100*ug/n:.0f}%)")
    print("=" * 64)
    print(f"HEADLINE: constrained GBNF-membership {100*cg/n:.0f}% vs "
          f"unconstrained {100*ug/n:.0f}%")


if __name__ == "__main__":
    main()
