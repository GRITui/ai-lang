#!/usr/bin/env python3
"""AINL larger-model constrained run (card Stage 3.4).

Extends the C2 harness with a SEMANTICS metric. C2 proved a small model,
through a real GBNF-constrained decoder, emits only *syntactically valid*
AINL — but the 0.5B model produced 0/20 *semantically* useful programs. This
harness asks the next question: does a LARGER model (Llama-3.2-1B / 3B), under
the same constraint, produce programs that actually RUN and DO WHAT WAS ASKED?

For each prompt in a *checkable* suite (one where the expected stdout is known
in advance), it runs TWO modes:

  * constrained   — the AINL GBNF is applied (llama-cli --grammar-file)
  * unconstrained — no grammar (baseline)

and scores each generation on THREE metrics:

  1. **Syntax**  — GBNF membership (scripts/gen-harness/gbnf_fast.py, the sound
     O(n) detector, cross-validated against the reference Earley).
  2. **Runs**    — `ainl run` exits 0 (no runtime error).
  3. **Correct** — it runs AND its stdout (stripped) exactly equals the
     expected value recorded in the suite.

Headline: per mode, % valid (GBNF) / % runs / % correct.

Dependencies:
  * llama.cpp `llama-cli` on PATH (0.5.0+; needs `-st`/`--single-turn`)
  * a GGUF model (Llama-3.2-1B-Instruct Q4_K_M used here; any via --model)
  * a built `ainl` binary (cargo build --release)
  * Python 3 (stdlib only)

Usage:
  python3 scripts/gen-harness/run_checkable.py \
      --model /path/to/Llama-3.2-1B-Instruct-Q4_K_M.gguf \
      --model-tag llama-3.2-1b \
      [--ainl target/release/ainl] \
      [--suite scripts/gen-harness/suite_checkable.json] \
      [--out results-llama-3.2-1b] [--max-tokens 512] [--threads 8] [--limit N]

Output:
  <out>/constrained/<id>.ainl    raw constrained generation
  <out>/unconstrained/<id>.ainl  raw unconstrained generation
  <out>/results.json             machine-readable per-prompt results
  <out>/results.csv              flat table for the doc
  stdout                         human-readable summary + the 3-metric headline
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


def load_suite(path: Path) -> list:
    return json.loads(path.read_text())


def run_llama(model: Path, prompt: str, grammar_file, max_tokens: int,
              threads: int) -> str:
    """One llama-cli single-turn completion. Returns the generated text.

    Uses `--output` to capture ONLY the generated text (no banner/prompt
    echo), which is robust across chat templates. Falls back to the
    banner/echo parsing if the output file is empty.
    """
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as pp, \
         tempfile.NamedTemporaryFile("w", suffix=".out", delete=False) as op, \
         tempfile.NamedTemporaryFile("w", suffix=".gen", delete=False) as gp:
        pp.write(prompt + "\n")
        pp.close()
        cmd = ["llama-cli", "-m", str(model), "-f", pp.name,
               "-n", str(max_tokens), "--temp", "0",
               "--no-display-prompt", "--no-warmup",
               "--threads", str(threads), "-st",
               "-o", gp.name]
        if grammar_file:
            cmd += ["--grammar-file", str(grammar_file)]
        subprocess.run(cmd, stdout=open(op.name, "w"),
                       stderr=subprocess.DEVNULL, timeout=600)
        gen = open(gp.name).read()
        txt = open(op.name).read()
    if gen.strip():
        # `-o` writes the FULL rendered conversation:
        #   "User:\n{prompt}\n\nAssistant:\n{generation}"
        # Strip the prefix; the generation is everything after the last
        # "Assistant:\n" (rsplit is robust to the exact prompt echo).
        if "Assistant:\n" in gen:
            return gen.rsplit("Assistant:\n", 1)[-1]
        return gen
    # Fallback: parse the banner + prompt-echo from stdout.
    return extract_generated(txt, prompt)


def extract_generated(txt: str, prompt: str) -> str:
    end = txt.find("[ Prompt:")
    body = txt[:end] if end >= 0 else txt
    lines = body.splitlines(keepends=True)
    for idx, line in enumerate(lines):
        if line.startswith("> ") and prompt in line:
            return "".join(lines[idx + 1:])
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
    ap.add_argument("--model-tag", required=True,
                    help="short tag for the model, e.g. llama-3.2-1b")
    ap.add_argument("--ainl", default=str(ROOT / "target" / "release" / "ainl"),
                    help="path to the ainl binary")
    ap.add_argument("--suite", default=str(HERE / "suite_checkable.json"))
    ap.add_argument("--out", default=None,
                    help="output dir (default: results-<model-tag>)")
    ap.add_argument("--max-tokens", type=int, default=512)
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

    suite = load_suite(Path(args.suite))
    if args.limit:
        suite = suite[:args.limit]

    out = Path(args.out) if args.out else HERE / f"results-{args.model_tag}"
    (out / "constrained").mkdir(parents=True, exist_ok=True)
    (out / "unconstrained").mkdir(parents=True, exist_ok=True)

    gbnf = export_gbnf(ainl)
    with tempfile.NamedTemporaryFile("w", suffix=".gbnf", delete=False) as g:
        g.write(gbnf)
        g.close()
        gbnf_path = g.name

    results = []
    for i, item in enumerate(suite):
        prompt = item["prompt"]
        expected = item["expected"]
        pid = item.get("id", str(i))
        row = {"index": i, "id": pid, "prompt": prompt, "expected": expected}
        for mode, grammar in (("constrained", gbnf_path),
                              ("unconstrained", None)):
            gen = run_llama(model, prompt, grammar, args.max_tokens,
                            args.threads)
            (out / mode / f"{pid}.ainl").write_text(gen)
            gbnf_ok = ainl_gbnf_accepts(gen)
            p_ok, p_err, r_ok, r_exit, r_out, r_err = ainl_check(ainl, gen)
            correct = r_ok and (r_out == expected)
            row[mode] = {
                "generated": gen,
                "gbnf_member": gbnf_ok,   # SOUND detector (syntax)
                "parse_ok": p_ok, "parse_err": p_err,
                "run_ok": r_ok, "run_exit": r_exit,
                "run_out": r_out, "run_err": r_err,
                "correct": correct,        # runs AND stdout == expected
            }
            tag = "C" if mode == "constrained" else "U"
            print(f"[{tag} {pid:12}] gbnf={'Y' if gbnf_ok else 'N'} "
                  f"run={'Y' if r_ok else 'N'} "
                  f"correct={'Y' if correct else 'N'}  {pid}", flush=True)
        results.append(row)

    (out / "results.json").write_text(json.dumps(results, indent=2))
    with open(out / "results.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["index", "id", "prompt", "expected",
                    "constrained_gbnf", "constrained_run", "constrained_correct",
                    "unconstrained_gbnf", "unconstrained_run",
                    "unconstrained_correct"])
        for r in results:
            w.writerow([r["index"], r["id"], r["prompt"], r["expected"],
                        r["constrained"]["gbnf_member"],
                        r["constrained"]["run_ok"],
                        r["constrained"]["correct"],
                        r["unconstrained"]["gbnf_member"],
                        r["unconstrained"]["run_ok"],
                        r["unconstrained"]["correct"]])

    n = len(results)

    def rate(mode, key):
        return sum(1 for r in results if r[mode][key])

    def uniq(mode):
        return len({r[mode]["generated"] for r in results})

    cg, cr, cc = (rate("constrained", "gbnf_member"),
                  rate("constrained", "run_ok"),
                  rate("constrained", "correct"))
    ug, ur, uc = (rate("unconstrained", "gbnf_member"),
                  rate("unconstrained", "run_ok"),
                  rate("unconstrained", "correct"))
    print("\n" + "=" * 64)
    print(f"RESULTS  (n={n} prompts, model={model.name})")
    print("=" * 64)
    print(f"  constrained   : valid {cg}/{n} ({100*cg/n:.0f}%)   "
          f"runs {cr}/{n} ({100*cr/n:.0f}%)   "
          f"correct {cc}/{n} ({100*cc/n:.0f}%)   "
          f"distinct-outputs {uniq('constrained')}/{n}")
    print(f"  unconstrained : valid {ug}/{n} ({100*ug/n:.0f}%)   "
          f"runs {ur}/{n} ({100*ur/n:.0f}%)   "
          f"correct {uc}/{n} ({100*uc/n:.0f}%)   "
          f"distinct-outputs {uniq('unconstrained')}/{n}")
    print("=" * 64)
    print(f"HEADLINE: constrained  valid {100*cg/n:.0f}% / runs {100*cr/n:.0f}% "
          f"/ correct {100*cc/n:.0f}%")
    print(f"           unconstrained valid {100*ug/n:.0f}% / runs {100*ur/n:.0f}% "
          f"/ correct {100*uc/n:.0f}%")


if __name__ == "__main__":
    main()
