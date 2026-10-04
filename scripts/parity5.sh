#!/usr/bin/env bash
# Run one AINL program on all five backends and print each backend's stdout
# (and stderr) so they can be diffed byte-for-byte. Used as the measurement
# instrument for the numeric model (see docs/NUMERIC_MODEL.md).
#
#   bash scripts/parity5.sh path/to/prog.ainl
#
# Prints one section per backend, then a verdict line.
set -uo pipefail
cd "$(dirname "$0")/.."

SRC=${1:?usage: parity5.sh prog.ainl}

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

dir=$(mktemp -d)
out=$dir/out
declare -a names=() bodies=()

emit() { # name body
  names+=("$1"); bodies+=("$2")
}

# -- interpreter (bytecode VM) --
o=$("$BIN" run "$SRC" 2>&1); emit interp "$o"

# -- AOT binary --
if "$BIN" compile "$SRC" -o "$dir/p.bin" >/dev/null 2>&1 && [ -x "$dir/p.bin" ]; then
  o=$("$dir/p.bin" 2>&1); emit aot "$o"
else
  emit aot "(aot compile failed)"
fi

# -- transpiler targets --
for target in js python ruby; do
  case "$target" in
    js) runner=node ;;
    python) runner=python3 ;;
    ruby) runner=ruby ;;
  esac
  if ! command -v "$runner" >/dev/null 2>&1; then
    emit "$target" "(skip: $runner not installed)"
    continue
  fi
  if ! "$BIN" transpile "$SRC" --to "$target" > "$out.$target" 2>"$dir/trans.$target"; then
    emit "$target" "(transpile failed: $(cat "$dir/trans.$target"))"
    continue
  fi
  o=$("$runner" "$out.$target" 2>&1); emit "$target" "$o"
done

for i in "${!names[@]}"; do
  printf '== %s ==\n%s\n' "${names[$i]}" "${bodies[$i]}"
done

# Verdict: compare each against the interpreter.
fail=0
base=${bodies[0]}
for i in "${!names[@]}"; do
  if [ "${bodies[$i]}" == "$base" ]; then
    printf 'ok   %s\n' "${names[$i]}"
  else
    printf 'DIFF %s\n' "${names[$i]}"
    fail=1
  fi
done

rm -rf "$dir"
if [ "$fail" -eq 0 ]; then
  echo "all five backends byte-identical"
else
  echo "backends DIVERGE"
fi
exit "$fail"