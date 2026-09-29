#!/usr/bin/env bash
# Byte-for-byte parity check for the Tier 3 byte-string primitives across every
# backend: interpreter, AOT C, and the three transpiler targets.
#
# One program, five runners, one expected output. Any divergence in stdout or
# stderr is a failure. The interpreter is the reference — it is the normative
# implementation — so every other backend's bytes are diffed against it.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
# The release binary is only trusted when it is at least as new as the sources
# it was built from; otherwise a stale release build silently becomes the
# *reference* output and every backend looks wrong.
if [ -x "$BIN" ] && [ target/debug/ainl -nt "$BIN" ]; then
  BIN=target/debug/ainl
fi
[ -x "$BIN" ] || { echo "building ainl..."; cargo build -q || exit 1; BIN=target/debug/ainl; }

# The parity program is a *print* program, not a test file, so it lives in
# fixtures/ rather than tests/ — `ainl test tests` sweeps that directory and
# would count its output as suite noise.
PROG=${1:-fixtures/byte_strings_parity.ainl}
[ -f "$PROG" ] || { echo "no such program: $PROG" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The interpreter is the reference: it is the normative implementation.
expected=$("$BIN" run "$PROG" 2>&1)
expected_rc=$?
echo "--- interpreter (rc=$expected_rc)"
printf '%s\n' "$expected"

fail=0
report() { # name, output, rc
  if [ "$2" == "$expected" ] && [ "$3" == "$expected_rc" ]; then
    echo "ok   $1"
  else
    echo "FAIL $1 (rc=$3, expected rc=$expected_rc)"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$2") | sed 's/^/    /'
    fail=1
  fi
}

# ---- AOT C ----
if command -v cc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
  ccbin=$(command -v cc >/dev/null 2>&1 && echo cc || echo clang)
  "$BIN" compile "$PROG" -o "$tmp/prog" --keep-c "$tmp/prog.c" >/dev/null || { echo "FAIL aot: compile"; fail=1; }
  if [ -f "$tmp/prog.c" ]; then
    # -lm: fmod (float formatting) and sqrt/floor live in libm, not libc, so a
    # Linux link needs it and macOS tolerates it. Matches the flag set in
    # crates/ainl-cc/tests/aot_stdlib.rs and the musl job in ci.yml.
    #
    # No -std flag, deliberately: the runtime uses strdup/lstat/nanosleep, which
    # are POSIX rather than C11, and -std=c11 (which implies -std=__STRICT_ANSI__)
    # hides their declarations — so the implicit-int return breaks the link on
    # Linux while macOS's default keeps it working. This matches the flag set
    # scripts/check-aot.sh and crates/ainl-cc/tests/aot_stdlib.rs already use.
    "$ccbin" -O2 -o "$tmp/prog.bin" "$tmp/prog.c" -lm 2>"$tmp/cc.log" || {
      echo "FAIL aot: cc"; sed 's/^/    /' "$tmp/cc.log"; fail=1; }
    if [ -x "$tmp/prog.bin" ]; then
      out=$("$tmp/prog.bin" 2>&1); rc=$?
      report "aot" "$out" "$rc"
    fi
  fi
else
  echo "skip aot (no C compiler)"
fi

# ---- transpilers ----
for t in python:python3 js:node ruby:ruby; do
  target=${t%%:*}; runner=${t##*:}
  command -v "$runner" >/dev/null 2>&1 || { echo "skip $target ($runner not installed)"; continue; }
  "$BIN" transpile "$PROG" --to "$target" > "$tmp/p.$target"
  out=$("$runner" "$tmp/p.$target" 2>&1); rc=$?
  report "$target" "$out" "$rc"
done

[ "$fail" -eq 0 ] && echo "ALL BACKENDS BYTE-EQUAL" || echo "PARITY FAILED"
exit "$fail"
