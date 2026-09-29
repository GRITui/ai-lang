#!/usr/bin/env bash
# `if` on all five runners: interpreter/VM, AOT C, python3, node, ruby.
#
# This job exists because of a bug the in-crate suite could not catch on its
# own. `sf_if`'s 2-arg arm compiled a trailing `Nil` with no `Jump` over it,
# so a TRUE condition pushed BOTH the then-value and the `Nil`; in a `fn`/
# `let` body the surplus shifted the next form's operand and the program died
# with "cannot call a nil". A `while` masked it (its per-iteration pop ate the
# surplus), which is why a 2-arg `if` in a loop passed for the language's
# whole life.
#
# "It ran" is not the claim; "the bytes matched the interpreter" is — and a
# leaked operand is invisible in a single call, so every program here makes a
# SECOND call after the `if`. The AOT and transpiler backends were always
# right (a host `if`/`else` assigns one temporary; there is no stack to
# over-push), and this gate is what turns that into a checked claim.
#
# The Rust suite covers the VM/tree-walk half in a debug build; this script
# covers the three non-bytecode backends, which need a release binary and
# their hosts. cc is preinstalled on ubuntu runners.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

# Every backend the 4-backend rule names, in the order they are reported.
# The interpreter and the VM are the same runner (`ainl run` is the VM), so
# five runners = VM, AOT C, python3, node, ruby.
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

  local py_out="" js_out="" rb_out=""
  py_out=$(python3 <("$B" transpile "$D/t.ainl" --to python) 2>&1); py_rc=$?
  js_out=$(node     <("$B" transpile "$D/t.ainl" --to js)     2>&1); js_rc=$?
  rb_out=$(ruby     <("$B" transpile "$D/t.ainl" --to ruby)   2>&1); rb_rc=$?

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

# ---- the card's minimal repro ------------------------------------------
# TWO calls: the 2-arg `if` is a non-last form in the closure body, and a
# leaked operand only corrupts the NEXT call. A one-call version of this
# program passes with the bug present.
check_all_five "two-arg if in a fn body" '
(def probe (fn (x)
             (if (= x 0) (print "hit"))
             (print "done")))
(print (probe 0))
(print (probe 5))
'

# The `if` as the LAST form of a body, and the false branch: the trailing
# `Nil` here is the form's own value, so a mis-patched jump shows as a wrong
# answer rather than a crash.
check_all_five "two-arg if as the last form" '
(def probe (fn (x) (if (= x 0) "hit")))
(print (probe 0))
(print (probe 5))
'

# The `let`-body shape `05_organize`'s do-move used, where the leak was first
# seen in the field (through map over a closure).
check_all_five "two-arg if in a let body" '
(def probe (fn (x)
             (let ((v (if (= x 0) (print "hit"))))
               (str "done " (str x)))))
(print (probe 0))
(print (probe 5))
'

# An anonymous closure inside a higher-order builtin: the real-world shape.
# The closure is bound with `def` rather than inlined, because a
# multi-statement body is a DOCUMENTED refusal on all three transpilers (an
# inline lambda/arrow cannot hold two statements) — so an inline closure here
# would be testing the refusal message, not the `if`. `def` is what real
# programs do anyway, and it is the shape `05_organize` used.
check_all_five "two-arg if in a closure over map" '
(def xs (list "a" "b"))
(def tag (fn (s)
            (if (= s "a") (print "A"))
            (str s "!")))
(print (map tag xs))
'

# Nested 2-arg ifs in both arms: each compiles its own jump pair, so a
# mis-patched target would send control into the wrong arm.
check_all_five "nested two-arg ifs" '
(def probe (fn (x)
             (if (= x 0)
                 (if (= x 0) "inner-true" "inner-false")
                 (if (= x 1) "else-true" "else-false"))))
(print (probe 0))
(print (probe 1))
(print (probe 2))
'

# The combination the bug was masked in: a 2-arg if in a LOOP body with a
# following form. The loop's per-iteration pop used to eat the surplus.
check_all_five "two-arg if in a loop body" '
(def probe (fn (n)
             (let ((i 0) (acc (list)))
               (while (< i n)
                 (if (= i 1) (print "one"))
                 (def acc (push acc i))
                 (def i (+ i 1)))
               acc)))
(print (probe 3))
'

# ---- the 3-arg form must be untouched by the fix -----------------------
check_all_five "three-arg if takes its else" '
(print (if true "t" "e"))
(print (if false "t" "e"))
'

# The 3-arg-with-nil-else idiom the e2e corpus used as its WORKAROUND. It
# must keep working (the workaround is only being retired, not invalidated)
# and must now be redundant.
check_all_five "three-arg if with an explicit nil else" '
(def probe (fn (x) (if (= x 0) "hit" nil) (print "done")))
(print (probe 0))
(print (probe 5))
'

echo ""
if [ $fail -eq 0 ]; then
  echo "SYNTAX: every backend agrees on if"
else
  echo "FAILED"
fi
exit $fail
