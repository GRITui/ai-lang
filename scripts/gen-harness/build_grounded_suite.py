#!/usr/bin/env python3
"""Build a syntax-grounded prompt suite for the gateway run.

Why this exists
---------------
The first gateway smoke test produced `PRINT 2 + 3` and `PRINT 42` — valid
S-expressions that are NOT AINL. The cause is not the model's weakness, it is
that it was asked to write AINL with zero description of what AINL is: it
guessed Lisp-with-capitals and the grammar happily accepted both forms,
because the GBNF only constrains *shape*, not *vocabulary*.

`docs/SYNTAX.md` is explicitly "written for model ingestion" (its own opening
line). Priming the model with the real reference — the special forms, the
builtins, and two worked examples — turns this from a test of "can a big model
guess a language it has never seen" into the experiment the card actually
specified: constrained vs unconstrained decoding of a *known* language.

Grounding is applied identically to BOTH arms, so it cannot bias the
constrained/unconstrained comparison. That comparison is the whole point; the
absolute number only means something if the model had a fair chance.

This is standard few-shot practice and is recorded as such in the results.
"""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent

# Compact reference: the forms a v0.1 program actually needs, plus the
# builtins. Kept short on purpose — a long preamble eats the context budget
# and, empirically, does not help the model more than the two examples do.
REFERENCE = """AINL is an S-expression language. Every expression is an atom or a
list (head arg...). There is no infix syntax and no operator precedence.
Comments start with ';' and run to end of line.

Special forms:
  (def name value)            bind a name
  (fn (p1 p2) body...)        function; (& rest) collects extra args
  (if cond then [else])       branch on truthiness; only nil/false are false
  (do form...)                run in order, return the last
  (let ((n v)...) body...)    bind locals in a new scope
  (while cond body...)        loop while cond is true
  (and a b ...) / (or a b ...)

Builtins: print, + - * /, = != < > <= >=, len, first, rest, last, list,
str, int, not, and nil/true/false literals.

Example program:
  (def sq (fn (x) (* x x)))
  (print (sq 12))

Example program:
  (def total (fn (& xs) (do (def s 0) (def go (fn (l a) (if (= (len l) 0) a (go (rest l) (+ a (first l)))))) (go xs 0))))
  (print (total 1 2 3 4 5))
"""


def build(prompt: str) -> str:
    return (REFERENCE
            + "\nTask: " + prompt
            + "\nWrite ONLY the AINL program. No explanation, no markdown, "
              "no code fences.\nProgram:\n")


def main():
    suite = json.loads((HERE / "suite_checkable.json").read_text())
    out = []
    for item in suite:
        out.append({
            "id": item["id"],
            "prompt": build(item["prompt"]),
            "expected": item["expected"],
            "raw_prompt": item["prompt"],
        })
    dest = HERE / "suite_checkable_grounded.json"
    dest.write_text(json.dumps(out, indent=2))
    print("wrote %s  (%d prompts, %d bytes)"
          % (dest, len(out), dest.stat().st_size))
    print("--- sample prompt ---")
    print(out[0]["prompt"])


if __name__ == "__main__":
    main()
