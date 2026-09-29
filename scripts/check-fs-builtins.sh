#!/usr/bin/env bash
# Byte-for-byte parity check for the Tier 3 file-system builtins (mkdir /
# rename / copy / is-dir / file-size) across every backend: interpreter, AOT C,
# and the three transpiler targets.
#
# One program, five runners, one expected output. Any divergence in stdout or
# stderr is a failure. The interpreter is the reference — it is the normative
# implementation — so every other backend's bytes are diffed against it.
#
# Each runner gets its OWN fresh copy of the scratch tree, because the program
# creates directories, moves files and deletes as it goes. Sharing one tree
# would make the second runner fail on the first runner's leftovers and the
# diff would be noise, not a parity finding.
set -uo pipefail
cd "$(dirname "$0")/.."
# Absolute, because every runner below executes with its cwd set to a private
# scratch directory (see run_in) and would not resolve a relative path from
# there.
REPO=$PWD

BIN=target/release/ainl
# The release binary is only trusted when it is at least as new as the sources
# it was built from; otherwise a stale release build silently becomes the
# *reference* output and every backend looks wrong.
if [ -x "$BIN" ] && [ target/debug/ainl -nt "$BIN" ]; then
  BIN=target/debug/ainl
fi
[ -x "$BIN" ] || { echo "building ainl..."; cargo build -q || exit 1; BIN=target/debug/ainl; }
BIN=$REPO/$BIN

# The parity program is a *print* program, not a test file, so it lives in
# fixtures/ rather than tests/ — `ainl test tests` sweeps that directory and
# would count its output as suite noise. (The strings gate says the same thing
# about its own fixture; this is a learned lesson, not a preference.)
PROG=${1:-fixtures/fs_builtins_parity.ainl}
[ -f "$PROG" ] || { echo "no such program: $PROG" >&2; exit 1; }
# Absolute, for the same reason $BIN is: the runners below execute with their
# own cwd, so a relative program path would not resolve from there.
PROG=$REPO/$PROG

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# run_in <name> <runner...> — run the program under <runner> in a private cwd and
# leave its combined stdout+stderr in $tmp/<name>.out. The cwd is what makes the
# program's relative paths land in a directory of its own.
run_in() {
  local name=$1
  shift
  local dir="$tmp/$name"
  mkdir -p "$dir"
  # $BIN and the transpiled sources are reached through $tmp, which is
  # absolute, because the runner executes with cwd=$dir — a relative path like
  # target/release/ainl would not resolve from there. (Learned the hard way:
  # the first run of this gate "passed" the interpreter only because it failed
  # to launch at all, and every backend was then compared against a
  # "No such file or directory" reference.)
  ( cd "$dir" && "$@" ) > "$tmp/$name.out" 2>&1
  echo $? > "$tmp/$name.rc"
}

fail=0

# ---- interpreter (the reference) ----
run_in interp "$BIN" run "$PROG"
expected=$(cat "$tmp/interp.out")
expected_rc=$(cat "$tmp/interp.rc")
echo "--- interpreter (rc=$expected_rc)"
printf '%s\n' "$expected"

report() { # name, rc
  local name=$1 rc=$2
  local out
  out=$(cat "$tmp/$name.out")
  if [ "$out" == "$expected" ] && [ "$rc" == "$expected_rc" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name (rc=$rc, expected rc=$expected_rc)"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$out") | sed 's/^/    /'
    fail=1
  fi
}

# ---- AOT C ----
if command -v cc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
  ccbin=$(command -v cc >/dev/null 2>&1 && echo cc || echo clang)
  "$BIN" compile "$PROG" -o "$tmp/aot-prog" --keep-c "$tmp/aot-prog.c" >/dev/null || {
    echo "FAIL aot: compile"
    fail=1
  }
  if [ -f "$tmp/aot-prog.c" ]; then
    # -lm: fmod (float formatting) and sqrt/floor live in libm, not libc, so a
    # Linux link needs it and macOS tolerates it. Matches the flag set in
    # crates/ainl-cc/tests/aot_stdlib.rs and the musl job in ci.yml.
    #
    # No -std flag, deliberately: the runtime uses strdup/lstat/nanosleep, which
    # are POSIX rather than C11, and -std=c11 (which implies -std=__STRICT_ANSI__)
    # hides their declarations — so the implicit-int return breaks the link on
    # Linux while macOS's default keeps it working. This file now also uses
    # mkdir/rename/lstat/off_t, which are the same story. This matches the flag
    # set scripts/check-aot.sh and crates/ainl-cc/tests/aot_stdlib.rs already use.
    "$ccbin" -O2 -o "$tmp/aot.bin" "$tmp/aot-prog.c" -lm 2>"$tmp/cc.log" || {
      echo "FAIL aot: cc"
      sed 's/^/    /' "$tmp/cc.log"
      fail=1
    }
    if [ -x "$tmp/aot.bin" ]; then
      run_in aot "$tmp/aot.bin"
      report aot "$(cat "$tmp/aot.rc")"
    fi
  fi
else
  echo "skip aot (no C compiler)"
fi

# ---- transpilers ----
for t in python:python3 js:node ruby:ruby; do
  target=${t%%:*}
  runner=${t##*:}
  command -v "$runner" >/dev/null 2>&1 || {
    echo "skip $target ($runner not installed)"
    continue
  }
  "$BIN" transpile "$PROG" --to "$target" > "$tmp/p.$target"
  run_in "$target" "$runner" "$tmp/p.$target"
  report "$target" "$(cat "$tmp/$target.rc")"
done

[ "$fail" -eq 0 ] && echo "ALL BACKENDS BYTE-EQUAL" || echo "PARITY FAILED"
exit "$fail"
