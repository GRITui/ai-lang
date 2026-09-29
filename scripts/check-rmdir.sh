#!/usr/bin/env bash
# Byte-for-byte parity check for `rmdir` — the inverse of `mkdir` — across every
# backend: the interpreter, the AOT C runtime, and the three transpiler targets.
#
# This is check-fs-builtins.sh's sibling rather than an extension of it, for one
# reason: this program needs a **symlink that exists before the run**. AINL has
# no symlink builtin, so the link is made by the shell here and the program only
# checks that `rmdir ":recursive"` unlinks it instead of following it. That is
# the single case the four hosts each get differently (shutil.rmtree refuses a
# symlinked *top* directory but not a link inside the tree, fs.rmSync follows by
# default, FileUtils.rm_rf does not), and it is the case a delegated recursive
# delete would get wrong.
#
# One program, five runners, one expected output. Any divergence in stdout, in
# stderr or in the return code is a failure. The interpreter is the reference —
# it is the normative implementation — so every other backend's bytes are
# diffed against it.
#
# Each runner gets its OWN fresh copy of the scratch tree: the program creates
# and deletes directories, so it is not idempotent and a shared tree would make
# the second runner fail on the first one's leftovers.
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
[ -x "$BIN" ] || { echo "building ainl (release)..."; cargo build --release --quiet || exit 1; BIN=target/release/ainl; }
BIN=$REPO/$BIN

# The parity program is a *print* program, not a test file, so it lives in
# fixtures/ rather than tests/ — `ainl test tests` sweeps that directory and
# would count its output as suite noise.
PROG=${1:-fixtures/rmdir_parity.ainl}
[ -f "$PROG" ] || { echo "no such program: $PROG" >&2; exit 1; }
# Absolute, for the same reason $BIN is: the runners below execute with their
# own cwd, so a relative program path would not resolve from there.
PROG=$REPO/$PROG

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# seed <dir> — build the one thing the program cannot build for itself: symlinks
# from inside the tree it will delete to a target OUTSIDE it. The program then
# removes a tree containing a link and checks the target survived, so a backend
# that followed the link would delete keep.txt and fail the diff.
#
# Two links, for two different rules: `tree-link` is moved into a directory that
# is then removed recursively (a link *inside* a deleted tree), and `solo-link`
# is passed to a plain `rmdir` (a link that is not a directory at all, per the
# lstat rule). `shutil.rmtree` refuses a symlinked *top* directory but not a
# link inside the tree, `fs.rmSync` follows by default and `FileUtils.rm_rf`
# does not — so neither delegation is safe and both are pinned here.
#
# `|| { skip_symlink=1; }` rather than a hard failure: a filesystem without
# symlink support (or a CI sandbox without the permission) would otherwise take
# the whole gate down over a case that has its own unit test in
# crates/ainl-cc/tests/aot_stdlib.rs and crates/ainl-core/tests/fs_builtins.rs.
skip_symlink=0
seed() {
  mkdir -p "$1/ainl-rmdir-parity/precious"
  printf 'keep' > "$1/ainl-rmdir-parity/precious/keep.txt"
  ln -s precious "$1/ainl-rmdir-parity/tree-link" || skip_symlink=1
  ln -s precious "$1/ainl-rmdir-parity/solo-link" || skip_symlink=1
}

# run_in <name> <runner...> — run the program under <runner> in a private cwd and
# leave its combined stdout+stderr in $tmp/<name>.out. The cwd is what makes the
# program's relative paths land in a directory of its own.
run_in() {
  local name=$1
  shift
  local dir="$tmp/$name"
  mkdir -p "$dir"
  seed "$dir"
  # $BIN and the transpiled sources are reached through $tmp, which is
  # absolute, because the runner executes with cwd=$dir — a relative path like
  # target/release/ainl would not resolve from there.
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

report() { # name
  local name=$1
  local rc out
  rc=$(cat "$tmp/$name.rc")
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
    # No -std flag, deliberately: the runtime uses strdup/lstat/nanosleep/
    # opendir/rmdir, which are POSIX rather than C11, and -std=c11 (which
    # implies -std=__STRICT_ANSI__) hides their declarations — so the implicit-int
    # return breaks the link on Linux while macOS's default keeps it working.
    # This matches the flag set scripts/check-aot.sh and
    # crates/ainl-cc/tests/aot_stdlib.rs already use.
    "$ccbin" -O2 -o "$tmp/aot.bin" "$tmp/aot-prog.c" -lm 2>"$tmp/cc.log" || {
      echo "FAIL aot: cc"
      sed 's/^/    /' "$tmp/cc.log"
      fail=1
    }
    if [ -x "$tmp/aot.bin" ]; then
      run_in aot "$tmp/aot.bin"
      report aot
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
  report "$target"
done

# The symlink case is the reason this script exists, so a run where the link
# could not be created is reported rather than quietly passing: the four
# remaining runners would still compare, but the case they were here for did
# not happen.
if [ "$skip_symlink" -ne 0 ]; then
  echo "WARN could not create the symlink — the 'tree-link' case did not run"
  fail=1
fi

[ "$fail" -eq 0 ] && echo "ALL BACKENDS BYTE-EQUAL" || echo "PARITY FAILED"
exit "$fail"
