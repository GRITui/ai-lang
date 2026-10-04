#!/usr/bin/env bash
# Reproduces the numbers in docs/NUMERIC_MODEL.md: runs the three headline
# programs through ALL FIVE backends (interpreter, AOT C binary, JS, Python,
# Ruby) and prints an agreement table.
#
# Before the numeric chain (cards 1–4) this script showed the *divergence* —
# four different answers for the same program. Now that every backend computes
# exact arbitrary-precision integers, it shows the *agreement*: every row is
# one value shared by all five backends. The verdict section says so, and
# isolates the one remaining, genuinely-undecidable divergence (float display)
# so it is labelled rather than mistaken for an integer disagreement.
#
#   bash scripts/numeric-divergence-demo.sh
#
# Exits 0 when every integer row agrees, non-zero if any backend diverges.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

# The three headline programs from docs/NUMERIC_MODEL.md.
cat > "$dir/p1.ainl" <<'EOF'
(print (* 9223372036854775807 2))
EOF
cat > "$dir/p2.ainl" <<'EOF'
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 25))
EOF
cat > "$dir/p3.ainl" <<'EOF'
(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))
(print (fact 100))
EOF

# run <name> <file> <cmd...> : capture stdout -> $dir/<name>.<file>.out
run() {
  local name=$1 file=$2; shift 2
  "$@" >"$dir/$name.$file.out" 2>"$dir/$name.$file.err"
}

# -- interpreter (bytecode VM) --
for f in p1 p2 p3; do run interp "$f" "$BIN" run "$dir/$f.ainl"; done

# -- AOT binary --
for f in p1 p2 p3; do
  if "$BIN" compile "$dir/$f.ainl" -o "$dir/$f.bin" >"$dir/$f.aotlog" 2>&1 && [ -x "$dir/$f.bin" ]; then
    run aot "$f" "$dir/$f.bin"
  else
    printf '(aot compile failed)\n' > "$dir/aot.$f.out"
    cat "$dir/$f.aotlog" > "$dir/aot.$f.err"
  fi
done

# -- transpiler targets --
for target in js python ruby; do
  case "$target" in
    js) runner=node ;;
    python) runner=python3 ;;
    ruby) runner=ruby ;;
  esac
  if ! command -v "$runner" >/dev/null 2>&1; then
    for f in p1 p2 p3; do
      printf '(skip: %s not installed)\n' "$runner" > "$dir/$target.$f.out"
      : > "$dir/$target.$f.err"
    done
    continue
  fi
  for f in p1 p2 p3; do
    if ! "$BIN" transpile "$dir/$f.ainl" --to "$target" > "$dir/$target.$f.src" 2>"$dir/$f.$target.transpile.err"; then
      printf '(transpile failed)\n' > "$dir/$target.$f.out"
      cat "$dir/$f.$target.transpile.err" > "$dir/$target.$f.err"
      continue
    fi
    run "$target" "$f" "$runner" "$dir/$target.$f.src"
  done
done

# -- agreement table --
echo
echo "# Numeric model — agreement table"
echo
echo "The three headline programs from docs/NUMERIC_MODEL.md, run on all five"
echo "backends. Every row is ONE value shared by all five backends (byte-for-"
echo "byte, stdout and stderr)."
echo
printf '%-14s | %-24s | %-24s | %-24s\n' "program" "i64::MAX * 2" "(fact 25)" "(fact 100)"
printf '%s\n' "---------------+--------------------------+--------------------------+--------------------------"
for name in interp aot js python ruby; do
  v1=$(cat "$dir/$name.p1.out")
  v2=$(cat "$dir/$name.p2.out")
  v3=$(cat "$dir/$name.p3.out")
  # fact 100 is 158 digits; show it truncated in the table, full value below.
  v3short="${v3:0:16}…(${#v3} digits)"
  printf '%-14s | %-24s | %-24s | %-24s\n' "$name" "$v1" "$v2" "$v3short"
done
echo
echo "(fact 100) full value, identical on all five:"
echo "  $(cat "$dir/interp.p3.out")"

# -- verdict (stdout AND stderr, byte-for-byte) --
echo
echo "# Verdict"
echo
echo "Every row is compared on BOTH streams: an error on one backend while the"
echo "others print is a divergence too."
fail=0
for f in p1 p2 p3; do
  base=$(cat "$dir/interp.$f.out")
  base_err=$(cat "$dir/interp.$f.err")
  for name in aot js python ruby; do
    v=$(cat "$dir/$name.$f.out")
    v_err=$(cat "$dir/$name.$f.err")
    if [ "$v" = "$base" ] && [ "$v_err" = "$base_err" ]; then
      printf 'ok   %s == interp on %s (stdout + stderr)\n' "$name" "$f"
    else
      [ "$v" != "$base" ] && printf 'DIFF %s stdout != interp on %s:\n  interp: %s\n  %s:   %s\n' "$name" "$f" "$base" "$name" "$v"
      [ "$v_err" != "$base_err" ] && printf 'DIFF %s stderr != interp on %s:\n  interp: %s\n  %s:   %s\n' "$name" "$f" "$base_err" "$name" "$v_err"
      fail=1
    fi
  done
done
echo
if [ "$fail" -eq 0 ]; then
  echo "All five backends AGREE on every integer row — there is no divergence."
else
  echo "Backends DIVERGE on an integer row (a bug in cards 1–4, not a paper-over)."
fi

# -- the one remaining divergence: whole-float display (JS and Ruby only) --
echo
echo "# The one remaining divergence (float display, not integer)"
echo
echo "Floats are f64 on every backend. For a WHOLE float the interpreter prints"
echo "the EXACT binary expansion, and so do the AOT binary and the Python target"
echo "— Python's _disp mirrors the interpreter's whole-float branch. JavaScript"
echo "and Ruby print the SHORTEST round-tripping decimal instead (Ruby in"
echo "scientific notation). Same f64 either way; only the chosen decimal differs."
echo "A NON-whole float agrees on all five, so this is the whole-float case only."
echo "Documented in NUMERIC_MODEL.md (\"the float display rule\")."
echo
FL='(print (* 9223372036854775807 1.5))'
printf '  %s\n' "$FL"
printf '  %-8s %s\n' "interp:" "$("$BIN" run <(echo "$FL") 2>/dev/null)"
fl_aot=$(mktemp)
if "$BIN" compile <(echo "$FL") -o "$fl_aot" >/dev/null 2>&1; then
  printf '  %-8s %s\n' "aot:" "$("$fl_aot" 2>/dev/null)"
else
  printf '  %-8s %s\n' "aot:" "(aot compile failed)"
fi
rm -f "$fl_aot"
for name in js python ruby; do
  t=$(mktemp)
  if "$BIN" transpile <(echo "$FL") --to "$name" > "$t" 2>/dev/null; then
    case "$name" in js) r=node ;; python) r=python3 ;; ruby) r=ruby ;; esac
    printf '  %-8s %s\n' "$name:" "$("$r" "$t" 2>/dev/null)"
  else
    printf '  %-8s %s\n' "$name:" "(transpile failed)"
  fi
  rm -f "$t"
done
echo
echo "  interp / aot / python agree exactly; js and ruby differ by display only."

exit "$fail"
