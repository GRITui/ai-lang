#!/usr/bin/env python3
"""AINL constrained-decoding proof harness (card C2).

Runs a small local model through a REAL GBNF-constrained decoder
(llama.cpp `llama-cli --grammar-file`) using the grammar exported by
`ainl grammar`, on a set of task prompts. For every prompt it records, in
TWO modes:

  * constrained   — the AINL GBNF is applied (the thesis under test)
  * unconstrained — no grammar (baseline: the model asked to emit AINL)

and for each generation checks, with a SOUND detector:

  (a) GBNF membership  — is the output in the language of the exported GBNF?
      (scripts/gen-harness/gbnf_fast.py, a fast recursive-descent parser that
      accepts exactly the GBNF language; cross-validated against the reference
      Earley in scripts/gbnf-conformance.py). This is the decisive test: if
      the constraint is applied, the decoder can only emit GBNF-accepted
      strings, so constrained output must be a GBNF member while
      unconstrained output (free Python with ':' / '#') is not.

  (b) `ainl ast`       — does it parse as AINL? (supplementary; the parser is
      a superset of the GBNF, so this is a weaker check than (a))
  (c) `ainl run`       — does it run without error? (supplementary)
  (d) "does what was asked" — NOT auto-judged; the raw output is saved so a
      human can inspect it.

The headline number is: constrained GBNF-membership-rate vs
unconstrained GBNF-membership-rate. An honest negative result (e.g. the model
is too weak to produce semantically correct AINL) is still the proof — the
point is that the *decoder* can only emit what the grammar allows.

Dependencies (documented in scripts/gen-harness/README.md):
  * llama.cpp `llama-cli` on PATH (0.5.0+; needs `-st`/`--single-turn`)
  * a small GGUF model (Qwen2.5-0.5B-Instruct Q4_K_M used here)
  * a built `ainl` binary (cargo build --release)
  * Python 3 (stdlib only)

Usage:
  python3 scripts/gen-harness/run_generation.py \
      --model /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
      [--ainl target/release/ainl] \
      [--prompts scripts/gen-harness/prompts.txt] \
      [--out results] [--max-tokens 256] [--threads 8] [--limit N]

Output:
  <out>/constrained/<i>.ainl    raw constrained generation
  <out>/unconstrained/<i>.ainl  raw unconstrained generation
  <out>/results.json            machine-readable per-prompt results
  <out>/results.csv             flat table for the doc
  stdout                        human-readable summary + the headline numbers
"""
import argparse
import csv
import json
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
from gbnf_fast import ainl_gbnf_accepts  # noqa: E402


def export_gbnf(ainl: Path) -> str:
    out = subprocess.run([str(ainl), "grammar", "--gbnf"],
                         capture_output=True, text=True, check=True)
    return out.stdout


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
    """The generated text is everything AFTER the prompt-echo line
    ('> <prompt>') and BEFORE the timing line ('[ Prompt: ... ]').

    llama-cli 0.5.0 prints a banner (Loading model, ASCII art, build info,
    command list) to stdout before the prompt echo, so we drop everything up
    to and including the echo line — not just the echo line itself.
    """
    end = txt.find("[ Prompt:")
    body = txt[:end] if end >= 0 else txt
    lines = body.splitlines(keepends=True)
    for idx, line in enumerate(lines):
        if line.startswith("> ") and prompt in line:
            return "".join(lines[idx + 1:])
    # Fallback: no echo line found (shouldn't happen) — return whole body.
    return body


def ainl_check(ainl: Path, text: str):
    """Returns (parse_ok, parse_err, run_ok, run_exit, run_out, run_err)."""
    with tempfile.NamedTemporaryFile("w", suffix=".ainl", delete=False) as f:
        f.write(text)
        f.close()
        p = f.name
    ast = subprocess.run([str(ainl), "ast", p], capture_output=True, text=True)
    run = subprocess.run([str(ainl), "run", p], capture_output=True, text=True)
    return (ast.returncode == 0, ast.stderr.strip(),
            run.returncode == 0, run.returncode,
            run.stdout.strip(), run.stderr.strip())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True, help="path to the GGUF model")
    ap.add_argument("--ainl", default=str(ROOT / "target" / "release" / "ainl"),
                    help="path to the ainl binary")
    ap.add_argument("--prompts", default=str(HERE / "prompts.txt"))
    ap.add_argument("--out", default=str(HERE / "results"))
    ap.add_argument("--max-tokens", type=int, default=256)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--limit", type=int, default=0,
                    help="only run the first N prompts (0 = all)")
    args = ap.parse_args()

    ainl = Path(args.ainl)
    model = Path(args.model)
    if not model.exists():
        raise SystemExit(f"FATAL: model not found: {model}")
    if not ainl.exists():
        raise SystemExit(f"FATAL: ainl binary not found: {ainl} "
                         f"(run cargo build --release)")

    prompts = load_prompts(Path(args.prompts))
    if args.limit:
        prompts = prompts[:args.limit]

    out = Path(args.out)
    (out / "constrained").mkdir(parents=True, exist_ok=True)
    (out / "unconstrained").mkdir(parents=True, exist_ok=True)

    gbnf = export_gbnf(ainl)
    with tempfile.NamedTemporaryFile("w", suffix=".gbnf", delete=False) as g:
        g.write(gbnf)
        g.close()
        gbnf_path = g.name

    results = []
    for i, prompt in enumerate(prompts):
        row = {"index": i, "prompt": prompt}
        for mode, grammar in (("constrained", gbnf_path),
                              ("unconstrained", None)):
            gen = run_llama(model, prompt, grammar, args.max_tokens,
                            args.threads)
            (out / mode / f"{i}.ainl").write_text(gen)
            gbnf_ok = ainl_gbnf_accepts(gen)
            p_ok, p_err, r_ok, r_exit, r_out, r_err = ainl_check(ainl, gen)
            row[mode] = {
                "generated": gen,
                "gbnf_member": gbnf_ok,   # SOUND detector (headline)
                "parse_ok": p_ok, "parse_err": p_err,
                "run_ok": r_ok, "run_exit": r_exit,
                "run_out": r_out, "run_err": r_err,
            }
            tag = "C" if mode == "constrained" else "U"
            print(f"[{tag} {i:2}] gbnf={'Y' if gbnf_ok else 'N'} "
                  f"parse={'Y' if p_ok else 'N'} run={'Y' if r_ok else 'N'}"
                  f"  {prompt[:40]!r}", flush=True)
        results.append(row)

    (out / "results.json").write_text(json.dumps(results, indent=2))
    with open(out / "results.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["index", "prompt",
                    "constrained_gbnf", "constrained_parse", "constrained_run",
                    "unconstrained_gbnf", "unconstrained_parse",
                    "unconstrained_run"])
        for r in results:
            w.writerow([r["index"], r["prompt"],
                        r["constrained"]["gbnf_member"],
                        r["constrained"]["parse_ok"], r["constrained"]["run_ok"],
                        r["unconstrained"]["gbnf_member"],
                        r["unconstrained"]["parse_ok"],
                        r["unconstrained"]["run_ok"]])

    n = len(results)

    def rate(mode, key):
        return sum(1 for r in results if r[mode][key])

    cg, cp, cr = (rate("constrained", "gbnf_member"),
                  rate("constrained", "parse_ok"),
                  rate("constrained", "run_ok"))
    ug, up, ur = (rate("unconstrained", "gbnf_member"),
                  rate("unconstrained", "parse_ok"),
                  rate("unconstrained", "run_ok"))
    print("\n" + "=" * 64)
    print(f"RESULTS  (n={n} prompts, model={model.name})")
    print("=" * 64)
    print(f"  constrained   : gbnf {cg}/{n} ({100*cg/n:.0f}%)   "
          f"parse {cp}/{n} ({100*cp/n:.0f}%)   run {cr}/{n} ({100*cr/n:.0f}%)")
    print(f"  unconstrained : gbnf {ug}/{n} ({100*ug/n:.0f}%)   "
          f"parse {up}/{n} ({100*up/n:.0f}%)   run {ur}/{n} ({100*ur/n:.0f}%)")
    print("=" * 64)
    print(f"HEADLINE: constrained GBNF-membership {100*cg/n:.0f}% vs "
          f"unconstrained {100*ug/n:.0f}%")
    print(f"          constrained parse-rate      {100*cp/n:.0f}% vs "
          f"unconstrained {100*up/n:.0f}%")
    print(f"          constrained run-rate        {100*cr/n:.0f}% vs "
          f"unconstrained {100*ur/n:.0f}%")


if __name__ == "__main__":
    main()
