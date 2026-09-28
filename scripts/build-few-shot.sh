#!/usr/bin/env bash
# Build examples/few-shot.txt from examples/corpus/*.ainl.
#
# WHY A GENERATED FILE RATHER THAN A HAND-WRITTEN ONE. This corpus exists to
# be shown to a model that has never seen AINL, so a stale few-shot prompt is
# worse than none: it teaches syntax that no longer parses, and a model shown
# a broken example learns the broken shape. Hand-maintained prompt text drifts
# from the code the moment a builtin is added or a program is fixed, and
# nothing notices until a generation quietly starts failing.
#
# So the corpus is EXTRACTED from the examples, and CI regenerates it and
# fails if the committed file differs. The examples are the single source of
# truth; this file is a projection of them.
#
# WHAT IS INCLUDED, and why:
#
#   * only `portable` examples, by default. A model shown `http-get` or
#     `import` will use them, and both are interpreter-only — a generated
#     program that reaches for either is refused by three of the four
#     backends. `--scope all` overrides this for a caller that knows better.
#   * a per-example header naming what it demonstrates, so a reader — human
#     or model — can select a relevant subset rather than reading all of it.
#     That is what makes the file usable as a FEW-SHOT source: a prompt about
#     file I/O does not need the HTTP example in it.
#   * the programs verbatim, COMMENTS INCLUDED. The comments are the point:
#     they carry the rules a model would otherwise get wrong (`while` does not
#     open a scope, `print` space-joins, `push` appends to its first
#     argument, there is no `list?`).
#
# Usage:
#   scripts/build-few-shot.sh              # write examples/few-shot.txt
#   scripts/build-few-shot.sh --check      # fail if it is out of date
#   scripts/build-few-shot.sh --scope all  # include every example
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=examples/few-shot.txt
SCOPE=portable
CHECK=0

while [ $# -gt 0 ]; do
  case "$1" in
    --check)
      CHECK=1
      shift
      ;;
    --scope)
      SCOPE="${2:-portable}"
      shift 2
      ;;
    --scope=*)
      SCOPE="${1#--scope=}"
      shift
      ;;
    -h|--help)
      sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "build-few-shot: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

# Read one @key from an example's header. Blank if absent.
field() {
  sed -n "s/^; @$2[ ]\{1,\}\(.*\)\$/\1/p" "$1" | head -1
}

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

{
  echo "# AINL few-shot corpus — GENERATED, do not edit."
  echo "#"
  echo "# Built by scripts/build-few-shot.sh from examples/corpus/*.ainl."
  echo "# Re-run that script after changing any example; CI checks this file"
  echo "# is current, so a hand edit here is reverted rather than believed."
  echo "#"
  echo "# This is the prompt-injection source for a model that has never seen"
  echo "# AINL. Inject SELECTIVELY: every example below is preceded by a header"
  echo "# naming what it teaches and what it demonstrates, so a request about"
  echo "# file I/O can be served the file-I/O example without the HTTP or"
  echo "# modules ones. 'ainl gen --examples <n>' takes the first n; for"
  echo "# targeted selection, grep for the header line."
  echo "#"
  if [ "$SCOPE" = "all" ]; then
    echo "# Scope: ALL examples, including interpreter-only ones. A model shown"
    echo "# http-get or import will use them, and three of the four backends"
    echo "# refuse a program that does — so this mode is for a caller that"
    echo "# knows it only ever runs the interpreter."
  else
    echo "# Scope: portable examples only. An interpreter-only example is"
    echo "# excluded because showing it teaches a shape that three of the four"
    echo "# backends reject outright."
  fi
} > "$tmp"

included=0
for ex in examples/corpus/*.ainl; do
  [ -f "$ex" ] || continue
  scope=$(field "$ex" scope)
  if [ "$SCOPE" != "all" ] && [ "$scope" != "portable" ]; then
    continue
  fi
  name=$(basename "$ex")
  {
    echo ""
    echo "# ====================================================================="
    echo "# examples/$name"
    echo "#   teaches:     $(field "$ex" teaches)"
    echo "#   demonstrates: $(field "$ex" summary)"
    echo "# ====================================================================="
    cat "$ex"
  } >> "$tmp"
  included=$((included + 1))
done

if [ "$included" -eq 0 ]; then
  echo "build-few-shot: no examples matched scope '$SCOPE' — refusing to write an empty corpus" >&2
  exit 1
fi

if [ "$CHECK" -eq 1 ]; then
  if [ ! -f "$OUT" ]; then
    echo "FAIL $OUT does not exist — run scripts/build-few-shot.sh"
    exit 1
  fi
  if ! diff -u "$OUT" "$tmp" > /dev/null 2>&1; then
    echo "FAIL $OUT is out of date with examples/corpus/. Run:"
    echo "       scripts/build-few-shot.sh"
    echo "     and commit the result. A stale few-shot prompt teaches syntax"
    echo "     that no longer parses, which is the one failure this corpus"
    echo "     cannot have."
    echo ""
    diff -u "$OUT" "$tmp" | head -30
    exit 1
  fi
  echo "ok   few-shot.txt is current ($included examples, scope=$SCOPE)"
  exit 0
fi

cp "$tmp" "$OUT"
echo "wrote $OUT ($included examples, scope=$SCOPE)"
