#!/usr/bin/env bash
# String RENDERING (repr) on all five runners: a quote or a backslash inside a
# string must come back out escaped, on every backend, byte for byte.
#
# This job exists because the transpilers' `_repr` helper wrapped a string in
# double quotes and did nothing else. `(str (list "a\"b"))` printed
#
#     ("a\"b")     interpreter, AOT C  (correct: escaped, unambiguous)
#     ("a"b")      python, node, ruby  (bare quote: ambiguous)
#
# The interpreter's `Value::repr` is Rust's `{:?}` and the AOT C runtime's
# `value_repr` is a hand-port of the same rule, so both were right and agreed
# with each other; only the three transpilers were wrong. Nothing caught it
# because no example, fixture or corpus program ever put a quote or a backslash
# INSIDE a container — the escaped form is the only place the helper is
# reachable, and every existing case printed a bare string, which goes through
# `_disp` and never touches `_repr`.
#
# "It ran" is not the claim; "the bytes matched the interpreter" is. The Rust
# suite covers the emitted source; this covers the three non-Rust backends plus
# the compiled C, which need a release binary and their hosts. cc is
# preinstalled on ubuntu runners.
#
# SCOPE, stated because the escape table is not a judgement call. The reference
# escapes exactly five characters: `"` -> `\"`, `\` -> `\\`, newline -> `\n`,
# tab -> `\t`, CR -> `\r`. The transpiler helpers implement that same five and
# no more. The interpreter (Rust's `{:?}`) additionally escapes other
# non-printable characters as `\u{..}`, which the C runtime does not do — so
# those bytes are a pre-existing interpreter-vs-AOT divergence that lives in
# the reference itself. The card forbids touching either, and widening the
# transpilers past the C runtime would make five-runner parity WORSE, not
# better. AINL's lexer only admits \n \t \r \\ \" \/ as source escapes, so
# these five are the characters a string can carry anyway.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }

# The reference output is pinned literally as well as compared, so a change in
# what string rendering MEANS cannot pass by moving all five backends together.
# These are the bytes `crates/ainl-core/src/value.rs` (`{:?}`) and the C
# runtime's `value_repr` produce. Read them off the interpreter if you change
# the table above — that is what the `want` block below does.
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

# The interpreter and the VM are the same runner (`ainl run` is the VM), so five
# runners = VM, AOT C, python3, node, ruby.
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
    echo "FAIL $label: the VM errored (exit $vm_rc)"; echo "       $vm_out"
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

# Assert the interpreter's own answer, so the escape table cannot be changed on
# all five backends at once and still pass.
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

# ---- the card's repro, verbatim, and each escape on its own --------------
# Written as one-liners so a failure names the character that broke, rather
# than pointing at a line number in a 28-line program.
want 'a quote inside a string is escaped in a container' \
  '("a\"b")' \
  '(print (str (list "a\"b")))'

want 'a backslash inside a string is escaped' \
  '("a\\b")' \
  '(print (str (list "a\\b")))'

want 'a newline inside a string is escaped' \
  '("a\nb")' \
  '(print (str (list "a\nb")))'

want 'a tab inside a string is escaped' \
  '("a\tb")' \
  '(print (str (list "a\tb")))'

want 'a carriage return inside a string is escaped' \
  '("a\rb")' \
  '(print (str (list "a\rb")))'

# A lone backslash renders as two. Before the fix this was one, which is the
# clearest single-character statement of the bug.
want 'a lone backslash renders doubled' \
  '("\\")' \
  '(print (str (list "\\")))'

want 'a lone quote renders backslash-quote' \
  '("\"")' \
  '(print (str (list "\"")))'

# ---- escaping ORDER: the assertion a quote-first fix fails -------------
# A literal backslash followed by an escapable character must render as exactly
# TWO backslashes then the character's own escape. Escaping backslash first
# gives that; escaping the quote (or anything else) first gives three, because
# the backslash it introduced gets escaped again.
want 'a literal backslash-n stays two backslashes then n' \
  '("\\n")' \
  '(print (str (list "\\n")))'

want 'a literal backslash-quote stays two backslashes then a quote' \
  '("\\\"")' \
  '(print (str (list "\\\"")))'

# ---- the other half: _disp must not escape ------------------------------
# A bare string is display, not repr. A fix that put the escaping in `_disp`
# would print this backslash-escaped, and the container cases above would still
# pass — so this pair is what stops the fix landing in the wrong helper.
want 'a bare string is printed as-is, unquoted and unescaped' \
  'a"b' \
  '(print "a\"b")'

want 'a bare string with a backslash is printed as-is' \
  'a\b' \
  '(print "a\\b")'

# The VALUE is not what changed: len counts the 3 characters the string holds,
# not the 5 its rendering shows.
want 'the escaped rendering does not change the length' \
  '3' \
  '(print (len "a\"b"))'

# ---- the whole fixture, on every backend -------------------------------
check_all_five "the full string-rendering fixture" \
  "$(cat fixtures/string_repr_parity.ainl)"

# Containers, since that is the only path that reaches `_repr`. A list element,
# a nested list, a hash key and a hash value each go through the helper.
check_all_five "quotes and backslashes in nested containers" '
(print (str (list (list "x\"y") (list "p\\q"))))
(print (str (hash "k\"" "v\"")))
(print (str (hash "a\\b" 1)))
(print (str (list "a\"b" "c\\d" "e\nf")))
(print (str (list "plain" "" nil true 0 1.5)))
'

# A quoted string is still a string afterwards: compared, measured, reused. If
# the escaping had leaked into the value itself, `=` would be false and `len`
# would count backslashes.
check_all_five "a quoted string still behaves like a string" '
(def s "raw\"quote")
(print (len s))
(print (= s "raw\"quote"))
(print (str (str s "!")))
(print (str (list s s)))
(print (str (str s)))
'

# `map` over strings is the realistic path into `_repr` — a program that reads
# names and prints a list of them. The escaping has to survive a callback.
check_all_five "quotes survive map over a list of strings" '
(print (map (fn (x) x) (list "a\"b" "c\\d" "e")))
(print (str (map (fn (x) x) (list "a\"b" "c\\d"))))
'

echo ""
if [ $fail -eq 0 ]; then
  echo "every backend escapes the same five characters, the interpreter's table"
else
  echo "FAILED"
fi
exit $fail
