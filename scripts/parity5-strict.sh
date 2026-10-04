#!/usr/bin/env bash
# Strict 5-backend parity: for a program, capture stdout and stderr SEPARATELY
# on each of the five backends (interpreter, AOT C, JS, Python, Ruby) and diff
# every pair byte-for-byte. Exits non-zero on ANY difference (stdout or stderr).
#
#   bash scripts/parity5-strict.sh prog.ainl
#
# This is the integration-gate instrument for numeric card 5/6: the first place
# all five backends must agree out of range, on both streams.
set -uo pipefail
cd "$(dirname "$0")/.."

SRC=${1:?usage: parity5-strict.sh prog.ainl}

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

# run <name> <cmd...> : capture stdout -> $dir/<name>.out, stderr -> $dir/<name>.err
run() {
  local name=$1; shift
  "$@" >"$dir/$name.out" 2>"$dir/$name.err"
}

# -- interpreter (bytecode VM) --
run interp "$BIN" run "$SRC"

# -- AOT binary --
if "$BIN" compile "$SRC" -o "$dir/p.bin" >"$dir/aot_compile.log" 2>&1 && [ -x "$dir/p.bin" ]; then
  run aot "$dir/p.bin"
else
  printf '(aot compile failed)\n' > "$dir/aot.out"
  cat "$dir/aot_compile.log" > "$dir/aot.err"
fi

# -- transpiler targets --
for target in js python ruby; do
  case "$target" in
    js) runner=node ;;
    python) runner=python3 ;;
    ruby) runner=ruby ;;
  esac
  if ! command -v "$runner" >/dev/null 2>&1; then
    printf '(skip: %s not installed)\n' "$runner" > "$dir/$target.out"
    : > "$dir/$target.err"
    continue
  fi
  if ! "$BIN" transpile "$SRC" --to "$target" > "$dir/$target.src" 2>"$dir/$target.transpile.err"; then
    printf '(transpile failed)\n' > "$dir/$target.out"
    cat "$dir/$target.transpile.err" > "$dir/$target.err"
    continue
  fi
  run "$target" "$runner" "$dir/$target.src"
done

names=(interp aot js python ruby)

echo "== per-backend stdout =="
for n in "${names[@]}"; do
  printf '%s\n' "---- $n ----"
  cat "$dir/$n.out"
done

echo
echo "== per-backend stderr =="
for n in "${names[@]}"; do
  printf '%s\n' "---- $n ----"
  cat "$dir/$n.err"
done

echo
echo "== byte-for-byte diff (stdout) =="
fail=0
base=interp
for n in "${names[@]}"; do
  if [ "$n" = "$base" ]; then continue; fi
  if diff -u "$dir/$base.out" "$dir/$n.out" > "$dir/diff-$n.out" 2>&1; then
    printf 'ok   %s stdout == %s stdout\n' "$n" "$base"
  else
    printf 'DIFF %s stdout != %s stdout:\n' "$n" "$base"
    cat "$dir/diff-$n.out"
    fail=1
  fi
done

echo
echo "== byte-for-byte diff (stderr) =="
for n in "${names[@]}"; do
  if [ "$n" = "$base" ]; then continue; fi
  if diff -u "$dir/$base.err" "$dir/$n.err" > "$dir/diff-$n.err" 2>&1; then
    printf 'ok   %s stderr == %s stderr\n' "$n" "$base"
  else
    printf 'DIFF %s stderr != %s stderr:\n' "$n" "$base"
    cat "$dir/diff-$n.err"
    fail=1
  fi
done

# A vacuous pass is the trap here. If the interpreter wrote NOTHING to either
# stream, the five empty files are trivially identical and the diffs above all
# pass — reporting "PASS" for a run in which no backend actually computed
# anything (a typo'd source path, or a comment-only program). Exit 2 marks that
# as UNVERIFIED rather than passing it off as agreement.
if [ ! -s "$dir/$base.out" ] && [ ! -s "$dir/$base.err" ]; then
  echo
  echo "INDETERMINATE: $base produced no output on stdout AND no output on"
  echo "stderr, so every backend compared equal only because they were all"
  echo "empty. Nothing was verified — check the source path and the binary."
  exit 2
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "FAIL: backends DIVERGE"
  exit "$fail"
fi

# Known, pre-existing, out of this card's scope: a runtime error surfaces through
# each host's UNCAUGHT-exception handler, so JS/Python/Ruby append a native
# traceback (with absolute temp paths) that the interpreter never prints. They
# agree on the AINL message itself, but not byte-for-byte on stderr. That is a
# separate, pre-existing gap — report it instead of calling it agreement.
if [ -s "$dir/$base.err" ]; then
  ainl_line=$(head -1 "$dir/$base.err")
  echo "STDOUT: byte-identical on all five backends."
  echo "STDERR: NOT byte-identical (exit 1, divergence above)."
  echo "        interp says: $ainl_line"
  echo "        this is the known host-traceback gap: the transpiled targets raise"
  echo "        through the host's uncaught-exception handler and append a native"
  echo "        traceback with absolute temp paths. They agree on the AINL message"
  echo "        but not byte-for-byte. Out of scope for the numeric card."
  exit 1
fi

echo "PASS: all five backends byte-identical on stdout AND stderr"
exit 0
