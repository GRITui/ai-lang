#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3b against the real binary.
# Doc claims written from memory are the ones that rot; this pins them.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/lib" "$D/sub"

fail=0
# want <label> <expected> <program-text>: run a PROGRAM FILE, not `eval`.
# Programs are written into $D/sub so that a specifier like "./local" resolves
# against the *importer's* directory — which is the whole point of the
# path-resolution cases. Writing them into $D would test the working-directory
# candidate instead and quietly assert the wrong thing.
#
# `ainl run` emits only what `print` produces, so every program here ends in an
# explicit `(print ...)`. A bare trailing value is evaluated and discarded,
# which reads as empty output and looks like a resolution failure.
want() {
  local label="$1" expected="$2" src="$3"
  printf '%s\n' "$src" > "$D/sub/t.ainl"
  local got
  got=$("$B" run "$D/sub/t.ainl" 2>&1)
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "       want: $expected"
    echo "       got:  $got"
    fail=1
  fi
}
# rejects <label> <needle> <program-text>
rejects() {
  local label="$1" needle="$2" src="$3"
  printf '%s\n' "$src" > "$D/sub/t.ainl"
  local got
  got=$("$B" run "$D/sub/t.ainl" 2>&1)
  if printf '%s' "$got" | grep -q "$needle"; then
    echo "ok   $label"
  else
    echo "FAIL $label (wanted /$needle/)"
    echo "       got: $got"
    fail=1
  fi
}

printf '(def square (fn (n) (* n n)))\n(def cube (fn (n) (* n n n)))\n' > "$D/lib/math.ainl"
printf '(import "../lib/math")\n(def area (fn (s) (* s s)))\n' > "$D/sub/shapes.ainl"
printf '(def v 42)\n' > "$D/sub/local.ainl"
printf '(def v "from txt")\n' > "$D/sub/data.txt"

echo "== the namespaced form is a plain map =="
want "call through (get m \"square\")" "49" \
  '(import "../lib/math.ainl" as m)
(print ((get m "square") 7))'
want "(has m \"square\") / (has m \"nope\")" "(true false)" \
  '(import "../lib/math.ainl" as m)
(print (list (has m "square") (has m "nope")))'
want "(keys m) is insertion order" "(\"square\" \"cube\")" \
  '(import "../lib/math.ainl" as m)
(print (keys m))'
want "(get m \"square\") prints as <fn>" "<fn>" \
  '(import "../lib/math.ainl" as m)
(print (get m "square"))'

echo
echo "== exports are the module's own top-level defs =="
rejects "a module does NOT re-export what it imported" \
  "unbound symbol 'square'" \
  '(import "shapes")
square'
want "the module's own def IS exported" "9" \
  '(import "shapes")
(print (area 3))'

echo
echo "== a module cannot see the importer's names =="
printf '(def uses-importer hidden)\n' > "$D/sub/peek.ainl"
rejects "module body cannot see the importer's scope" \
  "unbound symbol 'hidden'" \
  '(def hidden 5)
(import "peek")'

echo
echo "== path resolution =="
want "path-like: importer's dir first" "42" \
  '(import "./local")
(print v)'
want "bare name falls back to importer's dir" "42" \
  '(import "local")
(print v)'
want "explicit extension is respected" "from txt" \
  '(import "data.txt")
(print v)'
want "extensionless == with .ainl" "42" \
  '(import "./local.ainl")
(print v)'
rejects "not-found lists every candidate tried" "Tried:" \
  '(import "zzz-nope")'

echo
echo "== collisions =="
printf '(def len (fn () 1))\n' > "$D/sub/shadow.ainl"
rejects "a module cannot shadow a builtin" "already defined (by the prelude)" \
  '(import "shadow")'
rejects "the error names the conflict" "'len' is already defined" \
  '(import "shadow")'
rejects "the error points at the as escape" "as <alias>" \
  '(import "shadow")'
rejects "import vs this file's own def" "already defined" \
  '(import "../lib/math.ainl")
(def square 1)'
rejects "a def BEFORE the import collides too" "already defined" \
  '(def square 1)
(import "../lib/math.ainl")'
want "the as form resolves a genuine collision" "120" \
  '(import "../lib/math.ainl" as helper)
(def square 20)
(print (+ square ((get helper "square") 10)))'

echo
echo "== re-importing the same file is a no-op =="
printf '(def v 1)\n(append-file "'"$D"'/n.txt" "x")\n' > "$D/sub/once.ainl"
printf '(import "once")\n(import "once")\n(import "./once")\nv\n' > "$D/sub/twice.ainl"
"$B" run "$D/sub/twice.ainl" >/dev/null 2>&1
n=$(wc -c < "$D/n.txt" 2>/dev/null | tr -d ' ')
if [ "$n" == "1" ]; then
  echo "ok   three spellings of one path evaluate the module once"
else
  echo "FAIL: module evaluated $n times, want 1"
  fail=1
fi
want "re-importing a flat import is not a collision" "1" \
  '(import "once")
(import "once")
(print v)'

echo
echo "== circular import names the cycle =="
printf '(import "c2")\n' > "$D/sub/c1.ainl"
printf '(import "c1")\n' > "$D/sub/c2.ainl"
rejects "a cycle is reported" "circular import" '(import "c1")'
printf '(import "c5")\n' > "$D/sub/c5.ainl"
rejects "a self-import is a cycle" "circular import" '(import "c5")'

echo
echo "== nested import is an error =="
rejects "inside fn"  "only allowed at the top level" '(def f (fn () (import "../lib/math.ainl")))'
rejects "inside do"  "only allowed at the top level" '(do (import "../lib/math.ainl"))'
rejects "inside let" "only allowed at the top level" '(let () (import "../lib/math.ainl"))'
want "under quote is data, not a directive" "(import \"m.ainl\")" \
  '(print (quote (import "m.ainl")))'

echo
[ "$fail" -eq 0 ] && echo "SYNTAX 3b: every doc claim verified" || echo "SYNTAX 3b: SOME DOC CLAIMS ARE WRONG"
exit "$fail"
