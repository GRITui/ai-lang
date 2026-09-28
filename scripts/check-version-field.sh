#!/usr/bin/env bash
# Verify the build-target field extraction in check-install-clean-env.sh against
# every version string the binary can actually produce. Run standalone; the
# packaging suite calls it indirectly.
set -uo pipefail

fail=0
check() {
  # $1 = a full `ainl --version` line, $2 = expected outcome (ok|no-field|bad)
  _v=$1; _want=$2
  _f=$(printf '%s' "$_v" | sed -n 's/^ainl [^ ]* \([^ ]*\).*/\1/p')
  if [ -z "$_f" ]; then _got=no-field
  elif printf '%s' "$_f" | grep -Eq '^[a-z0-9_]+(-[a-z0-9_]+)+$'; then _got=ok
  else _got=bad
  fi
  if [ "$_got" = "$_want" ]; then
    echo "ok   [$_got] $_v"
  else
    echo "FAIL want=$_want got=$_got  $_v"; fail=1
  fi
}

# The three real shapes: a macOS release build, a CI Linux build (note: the
# plain gnu triple, since the musl artifact belongs to the release job), and
# the degraded fallback when there is no git and no target information.
check "ainl 0.2.0 aarch64-apple-darwin (dirty tree) (d7735eaa003d)"            ok
check "ainl 0.2.0 x86_64-unknown-linux-gnu (tree state unknown) (d6ff430a1892)" ok
check "ainl 0.3.0 aarch64-unknown-linux-musl (8f3a1c9d2b7e)"                 ok
# A tree with no git at all: the target is still known, so this stays ok.
check "ainl 0.2.0 x86_64-unknown-linux-gnu (unknown) (unknown)"               ok
# A genuinely absent target must not be mistaken for a triple.
check "ainl 0.2.0 unknown (unknown) (unknown)"                                bad
# A malformed line with no target field at all.
check "ainl 0.2.0"                                                           no-field

echo
[ "$fail" -eq 0 ] && echo "version-field parsing: ALL PASSED" || echo "version-field parsing: FAILURES"
exit "$fail"
