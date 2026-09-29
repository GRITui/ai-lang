#!/usr/bin/env bash
# The transpilers must refuse every db-* name, and the AOT backend must not.
# A blanket "storage is too hard" rule that swept in the AOT backend would break
# the docs' whole argument while every refusal test kept passing, so the
# negative case is asserted in both directions.
set -uo pipefail
cd "$(dirname "$0")/.."
# Absolute, and resolved *before* the cd into the scratch directory below — a
# relative `./target/release/ainl` stops existing the moment the script changes
# directory, and every assertion then fails on "no such file" instead of on the
# thing it is checking.
A="$PWD/target/release/ainl"
[ -x "$A" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
cd "$D"
fail=0

# name, program
#
# All ten, including `db-get-raw`. That name is the one most likely to be
# missing from a list like this: it was added in card 2 by *renaming* §3k's
# reader rather than by adding a symbol, so nothing in the refusal path
# announces it, and a list written from the original five would silently skip
# the only builtin whose name is not mentioned in either section's heading.
CASES=(
  "db-set|(db-set 1 \"k\" 2)"
  "db-get|(db-get 1 \"k\")"
  "db-get-raw|(db-get-raw 1 \"k\")"
  "db-del|(db-del 1 \"k\")"
  "db-keys|(db-keys 1)"
  "db-count|(db-count 1)"
  "db-open|(db-open \"d.ainl-db\")"
  "db-put|(db-put 1 \"k\" \"v\")"
  "db-flush|(db-flush 1)"
  "db-close|(db-close 1)"
)

for c in "${CASES[@]}"; do
  name=${c%%|*}
  prog=${c#*|}
  printf '%s\n' "$prog" > t.ainl
  for tgt in python js ruby; do
    got=$($A transpile --to "$tgt" t.ainl 2>&1)
    case "$got" in
      *transpiler-only*) : ;;
      *) echo "FAIL transpile --to $tgt must refuse $name"; echo "     got: $got"; fail=1 ;;
    esac
    case "$got" in
      *"$name"*) : ;;
      *) echo "FAIL the $tgt refusal for $name must name the symbol"; echo "     got: $got"; fail=1 ;;
    esac
  done
  # AOT must accept every one of them.
  if $A compile t.ainl -o t >/dev/null 2>&1; then
    :
  else
    echo "FAIL ainl compile must accept $name"
    $A compile t.ainl -o t 2>&1 | head -3
    fail=1
  fi
done

# The wording must not regress to "interpreter-only": ainl compile runs these.
printf '(db-set 1 "k" 2)\n' > t.ainl
got=$($A transpile --to python t.ainl 2>&1)
case "$got" in
  *interpreter-only*)
    echo "FAIL the refusal must not say interpreter-only (ainl compile runs db-set)"
    fail=1 ;;
  *) : ;;
esac

echo
[ "$fail" -eq 0 ] \
  && echo "all ${#CASES[@]} db-* names: 3 transpilers refuse, AOT accepts" \
  || echo "REFUSAL MATRIX FAILED"
exit "$fail"
