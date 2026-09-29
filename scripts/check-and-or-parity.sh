#!/usr/bin/env bash
# `and`/`or` on all five runners: interpreter/VM, AOT C, python3, node, ruby.
#
# This job exists because `and`/`or` return an OPERAND, not a boolean, and
# nothing pinned that. The AOT emitter seeded its accumulator with
# `v_bool(1)`/`v_bool(0)` and overwrote it only on the short-circuit path, so
# whenever every operand was truthy the seed survived: `(and 1 2 3)` compiled
# to a program that printed `true` while all four other backends printed `3`.
#
# The collections gate could not see this because neither the parity program
# nor its doc claims ran `and`/`or` with non-boolean operands — every use was
# a bare boolean guard, where "returns a boolean" and "returns the operand"
# are indistinguishable. Every program below therefore prints the RESULT of
# `and`/`or` directly, with non-boolean operands, so the return value itself is
# the thing under test.
#
# Note the truthiness model while reading the expectations: only `nil` and
# `false` are falsey, so `0` and `""` are TRUTHY (SYNTAX.md 1). `(and 0 1)` is
# therefore `(and 1 1)` -> `1`, and `(or 0 1)` is `(or 1 1)` -> `0`, the first
# truthy operand. A parity gate that assumed C-like truthiness would "fix" the
# AOT backend into agreeing with a program that is wrong.
#
# The gate has caught two different backends disagreeing with the interpreter
# about the SAME pair of forms, which is why it asserts against the interpreter
# rather than against a table of expected strings:
#
#   * the AOT emitter seeded its accumulator with `v_bool(1)` and overwrote it
#     only on the short-circuit path, so an all-truthy `and` returned the seed
#     (`true`) where every other backend returned the last operand;
#   * the three transpilers folded `or` to a host conditional seeded with the
#     LAST OPERAND, so an all-falsy `or` returned that operand where the
#     interpreter and the C emitter return `false` (SYNTAX.md 2). The same
#     fold emitted each operand's text twice — once in the `_truthy` test, once
#     as the value it yields — so an operand ran twice.
#
# A gate that only checked values would have caught the first and missed the
# third, so the programs below print side effects as well as results.
#
# "It ran" is not the claim; "the bytes matched the interpreter" is. The Rust
# suite covers the VM/tree-walk half in a debug build; this script covers the
# three non-bytecode backends plus the compiled C, which need a release binary
# and their hosts. cc is preinstalled on ubuntu runners.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

# Every backend the 4-backend rule names. The interpreter and the VM are the
# same runner (`ainl run` is the VM), so five runners = VM, AOT C, python3,
# node, ruby.
check_all_five() {
  local label="$1" src="$2"
  printf '%s\n' "$src" > "$D/t.ainl"

  local vm_out vm_rc
  vm_out=$("$B" run "$D/t.ainl" 2>&1); vm_rc=$?

  local aot_out aot_rc="skipped"
  if command -v cc >/dev/null 2>&1; then
    if "$B" compile "$D/t.ainl" -o "$D/t_aot" >"$D/cc.log" 2>&1; then
      aot_out=$("$D/t_aot" 2>&1); aot_rc=$?
    else
      echo "FAIL $label: aot compile"; cat "$D/cc.log"; fail=1; return
    fi
  fi

  # Transpile to a REAL FILE, then run it. Piping the program in (e.g.
  # `node <(...)`) works on macOS, where /dev/fd resolves, but on an ubuntu
  # runner node opens the path as a regular file and fails with
  # ENOENT: '/proc/PID/fd/pipe:[...]'.
  local py_out="" js_out="" rb_out=""
  "$B" transpile "$D/t.ainl" --to python > "$D/t.py" 2>/dev/null
  py_out=$(python3 "$D/t.py" 2>&1); py_rc=$?
  "$B" transpile "$D/t.ainl" --to js     > "$D/t.js" 2>/dev/null
  js_out=$(node     "$D/t.js" 2>&1); js_rc=$?
  "$B" transpile "$D/t.ainl" --to ruby   > "$D/t.rb" 2>/dev/null
  rb_out=$(ruby     "$D/t.rb" 2>&1); rb_rc=$?

  if [ "$vm_rc" -ne 0 ]; then
    echo "FAIL $label: the VM errored (exit $vm_rc)"
    echo "       $vm_out"
    fail=1
    return
  fi

  local b
  for b in aot python js ruby; do
    local out rc
    case $b in
      aot)    out=$aot_out; rc=$aot_rc ;;
      python) out=$py_out;   rc=$py_rc ;;
      js)     out=$js_out;   rc=$js_rc ;;
      ruby)   out=$rb_out;   rc=$rb_rc ;;
    esac
    if [ "$rc" = "skipped" ]; then
      echo "SKIP $label/$b: no cc"; continue
    fi
    if [ "$rc" -ne 0 ]; then
      echo "FAIL $label/$b: exit $rc"; echo "       $out"; fail=1; continue
    fi
    if [ "$out" == "$vm_out" ]; then
      echo "ok   $label/$b matches the interpreter byte-for-byte"
    else
      echo "FAIL $label/$b diverged from the interpreter"
      diff <(printf '%s\n' "$vm_out") <(printf '%s\n' "$out") | sed 's/^/       /'
      fail=1
    fi
  done
}

# Also assert the interpreter's own answer, so a change in what `and`/`or`
# MEAN cannot pass by moving all five backends together.
want() {
  local label="$1" expected="$2" src="$3"
  printf '%s\n' "$src" > "$D/t.ainl"
  local got
  got=$("$B" run "$D/t.ainl" 2>&1)
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "       want: $expected"
    echo "       got:  $got"
    fail=1
  fi
}

# ---- the card's repro, verbatim ------------------------------------------
# `(and 0 1)`/`(and 1 2)` returned `true` on AOT C. `0` and `""` are TRUTHY in
# AINL, so these are all-truthy chains and the answer is the LAST operand.
want "and 0 1 returns the last operand, not a boolean" \
  '1' \
  '(print (and 0 1))'

want "and 1 2 returns the last operand" \
  '2' \
  '(print (and 1 2))'

want 'and 0 "" returns the last operand (a blank line)' \
  '' \
  '(print (and 0 ""))'

want "or 0 1 returns the FIRST truthy operand, which is 0" \
  '0' \
  '(print (or 0 1))'

# The all-truthy shape, the exact case the old AOT seed broke.
want "and returns its last operand when every operand is truthy" \
  '3' \
  '(print (and 1 2 3))'

# Non-boolean operand types: the return value must survive intact, not be
# coerced. A boolean return would print `true` here.
want "and returns a string operand" \
  'b' \
  '(print (and "a" "b"))'

want "and returns a list operand" \
  '(1 2)' \
  '(print (and 0 (list 1 2)))'

want "or returns a list operand" \
  '(1 2)' \
  '(print (or nil (list 1 2)))'

# The falsey operands themselves, which are the short-circuit results.
# `(or nil 0)` returns `0`, NOT nil: `0` is truthy in AINL (SYNTAX.md 1), so
# `or` stops there. A parity gate that read it as "0 is falsey" would pin the
# backends to the wrong answer.
want "and returns nil"                 'nil'   '(print (and nil 1))'
want "and returns false"               'false' '(print (and false 1))'
want "and returns the FIRST falsey of several" 'nil' '(print (and 1 nil 2 3))'
want "or returns 0 because 0 is TRUTHY" '0'   '(print (or nil 0))'
want "or returns the first truthy of several"  '2'  '(print (or nil 2 3))'
want "or returns false when every operand is falsey" 'false' '(print (or nil false))'

# The identities. `(and)` is true and `(or)` is false, and these are the only
# cases where a boolean is the right answer.
want "(and) is true"  'true'  '(print (and))'
want "(or) is false"  'false' '(print (or))'
want "(and x) is x"   '7'     '(print (and 7))'
want "(or x) is x"    '7'     '(print (or 7))'

# ---- every case on every backend -----------------------------------------
# One program carrying the whole truthiness table through both forms, so a
# backend that gets ONE cell wrong fails here rather than in the field.
check_all_five "and/or return operands across all types" '
(print (and 0 1))
(print (and 1 2))
(print (and 0 ""))
(print (and 1 nil))
(print (and nil 1))
(print (and false 1))
(print (and 1 false))
(print (and 1 2 3))
(print (and 1 2 nil))
(print (and nil nil))
(print (and false false))
(print (and))
(print (and "a" "b"))
(print (and 0 (list 1 2)))
(print (or 0 1))
(print (or nil 1))
(print (or nil false))
(print (or nil 0))
(print (or 1 2 3))
(print (or nil nil 5))
(print (or "a" "b"))
(print (or nil (list 1 2)))
(print (or nil nil))
(print (or false nil))
(print (or false false))
(print (or nil false nil))
'

# A lone operand, both forms. `or` is NOT the same as "hand the operand back":
# with one operand the chain has no test to run, so `(or nil)` answers the
# `false` identity while `(or 0)` answers `0` — 0 is TRUTHY (SYNTAX.md 1). The
# operand's truthiness is a runtime property, so this is the case where a
# transpiler has to emit an actual test rather than pick a branch at compile
# time. It belongs on all five backends for the same reason the all-falsy chains
# do: a target that answers `nil` here is wrong on a real expression.
check_all_five "a single operand comes back unchanged" '
(print (and 42))
(print (or 42))
(print (and "solo"))
(print (or (list 1)))
(print (and nil))
(print (and false))
(print (or nil))
(print (or false))
(print (or 0))
(print (or ""))
'

# ---- short-circuiting must survive the rewrite ---------------------------
# `and`/`or` are lazy. The AOT emitter has to test the accumulator BEFORE
# emitting the next operand's code, or a `goto` that is correct on the value
# still evaluates every operand on every run. A `print` side effect in the
# dead branch is the observable difference, and an error there (`(/ 1 0)`)
# proves the operand was never evaluated at all.
check_all_five "and stops at the first falsey operand" '
(print (and false (print "DEAD")))
(print (and nil (print "DEAD")))
(print (and 1 nil (print "DEAD")))
(print (and false (/ 1 0)))
'

check_all_five "or stops at the first truthy operand" '
(print (or true (print "DEAD")))
(print (or 1 (print "DEAD")))
(print (or nil 1 (print "DEAD")))
(print (or 1 (/ 1 0)))
'

# Each operand must be evaluated exactly ONCE on the run that does evaluate
# it — a double evaluation shows up as the side effect printing twice. The
# counted value is a `fn` call rather than an inline `(do ...)`, because a
# multi-statement `do` in expression position is a DOCUMENTED refusal on all
# three transpilers (an inline two-statement lambda cannot be expressed), so
# an inline `do` here would be testing the refusal, not the evaluation count.
#
# The `(or …)` rows below used to be kept out of this program: an all-falsy `or`
# was the one shape where the transpilers disagreed with the interpreter, so
# the gate asserted the interpreter's answer alone (the `want` block) and left
# these three backends uncovered. With `or` returning its `false` identity they
# belong here, and they are also the sharpest test of the count: the transpilers
# emit each operand's text twice — once inside the `_truthy` test, once as the
# value it yields — so a fold that repeats the text prints "A" twice where the
# interpreter prints it once. A value-only assertion cannot see that; this can.
check_all_five "an evaluated operand runs exactly once" '
(def note (fn (s) (print s) s))
(print (and 1 (note "ONCE")))
(print (or nil (note "ONCE")))
(print (and 1 1 (note "ONCE")))
(print (and false (note "DEAD")))
(print (or true (note "DEAD")))
(print (or (note "A") 1))
(print (or nil (note "B")))
(print (and (note "C") (or nil (note "D"))))
(print (or nil (note "E") nil))
'

# The result must be a value the program can go on USING, not just print: a
# string/struct return that lost its ownership would print correctly here and
# corrupt the next form.
check_all_five "the returned operand is usable afterwards" '
(def v (and 1 "kept"))
(print v)
(print (str "x" (and 1 "kept")))
(def w (or nil (list 1 2)))
(print (len w))
(print (and 1 "kept"))
'

# Nested: the inner form is itself a value-producing special form.
check_all_five "nested and/or" '
(print (and 1 (or nil "inner")))
(print (or nil (and 1 "inner")))
(print (and 1 (and 2 "deep")))
(print (or nil (or false "deep")))
'

# In a closure body, where the value is stored and re-read rather than printed
# directly. The `def`s are named `pick`/`fallback` rather than `first`/`second`
# because those shadow builtins, and a shadowed builtin resolves differently on
# the transpilers (their `_first` is a list helper, not the user's closure).
check_all_five "and/or inside a fn body" '
(def pick (fn (x) (and x "default")))
(print (pick 0))
(print (pick nil))
(def fallback (fn (x) (or x "fb")))
(print (fallback nil))
(print (fallback 5))
'

echo ""
if [ $fail -eq 0 ]; then
  echo "SYNTAX 2: every backend agrees on and/or returning operands"
else
  echo "FAILED"
fi
exit $fail
