#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3e (try/catch) against the
# real binary, and gate the 4-backend parity the section asserts.
#
# Doc claims written from memory are the ones that rot; this pins them. The
# parity half is the point of the card: a `catch` binds a message a program may
# compare, so the interpreter, the VM, the AOT binary and the three transpiler
# hosts have to agree on the bytes. Position suffixes are exempt (per the
# Tier 2 card-1 precedent) because the AOT binary and the transpiled sources
# embed no source text.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

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

# ---- the form, the two paths -------------------------------------------
want "success returns the body value" \
  '3' '(print (try (+ 1 2) (catch (e) "unreachable")))'

want "failure returns the handler value" \
  'handled' '(print (try (/ 1 0) (catch (e) "handled")))'

want "the handler does not run on success" \
  '7' '(print (try 7 (catch (e) (error "handler must not run"))))'

# ---- the shape of e, exactly as the table in the docs says -------------
want "e is a map with message then kind" \
  '{"message" "boom" "kind" "runtime"}' \
  '(print (try (error "boom") (catch (e) e)))'

want "get on e reaches both keys" \
  'boom/runtime' \
  '(print (str (get (try (error "boom") (catch (e) e)) "message") "/" (get (try (error "boom") (catch (e) e)) "kind")))'

# ---- message bodies are AINL's, not the host's -------------------------
want "read-file reports AINL's message" \
  "read-file: cannot read 'definitely-not-here.txt'" \
  '(print (try (read-file "definitely-not-here.txt") (catch (e) (get e "message"))))'

want "division by zero, not Infinity" \
  'division by zero' \
  '(print (try (/ 1 0) (catch (e) (get e "message"))))'

want "a str operand is rejected, not coerced" \
  'expected a number, got str' \
  '(print (try (+ 1 "s") (catch (e) (get e "message"))))'

want "a bool operand is rejected, not promoted" \
  'expected a number, got bool' \
  '(print (try (+ true 1) (catch (e) (get e "message"))))'

# ---- nesting, innermost catch wins --------------------------------------
want "the innermost catch wins" \
  'inner' \
  '(print (try (try (error "inner") (catch (e) (get e "message")))
                 (catch (e) "outer")))'

want "an outer catch still runs when the inner one is absent" \
  'outer:boom' \
  '(print (try (error "boom") (catch (e) (str "outer:" (get e "message")))))'

# ---- placement ---------------------------------------------------------
want "try inside fn, success" '2.0' \
  '(print ((fn (x) (try (/ 10 x) (catch (e) -1))) 5))'

want "try inside fn, caught" '-1' \
  '(print ((fn (x) (try (/ 10 x) (catch (e) -1))) 0))'

want "a caught error does not end the while loop" \
  '("c0" 1.0 "c2" 1.0 "c4")' \
  '(def i 0)
   (def acc (list))
   (while (< i 5)
     (def acc (push acc (try (/ 1 (mod i 2)) (catch (e) (str "c" i)))))
     (def i (+ i 1)))
   (print acc)'

# ---- scoping: the docs say the two sides are siblings -------------------
want "a def in a failed body is not visible to its own handler" \
  'escaped' \
  '(print (try (try (error "x") (catch (e) leaked))
               (catch (e2) "escaped")))'

want "a def in a successful body does not leak either" \
  'nil' \
  '(try (do (def tmp 99) 1) (catch (e) 0))
   (print (try tmp (catch (e) nil)))'

# ---- uncaught is unchanged ---------------------------------------------
printf '%s\n' '(print "before")' '(error "the-end")' '(print "after")' > "$D/t.ainl"
out=$("$B" run "$D/t.ainl" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "runtime error: the-end" \
   && printf '%s' "$out" | grep -q "at line 2" && ! printf '%s' "$out" | grep -q "after"; then
  echo "ok   an uncaught error still exits non-zero with the model-readable message"
else
  echo "FAIL uncaught error: rc=$rc out=$out"
  fail=1
fi

# ---- the 4-backend parity the section promises -------------------------
# Only PORTABLE programs: an unbound symbol is a compile-time failure in
# Python/JS/Ruby, so a `catch` cannot intercept it there. That difference is
# real, documented, and covered in crates/ainl-core/tests/try_catch.rs.
cat > "$D/parity.ainl" <<'AINL'
(print (try (+ 1 2) (catch (e) "no")))
(print (try (/ 1 0) (catch (e) (get e "message"))))
(print (try (error "boom") (catch (e) e)))
(print (try (get "x" "y") (catch (e) (get e "message"))))
(print (try (len 5) (catch (e) (get e "message"))))
(print (try (first 5) (catch (e) (get e "message"))))
(print (try (try (error "in") (catch (e) (get e "message"))) (catch (e) "out")))
AINL

ref=$("$B" run "$D/parity.ainl" 2>&1)
"$B" transpile "$D/parity.ainl" --to python > "$D/parity.py" 2>/dev/null
"$B" transpile "$D/parity.ainl" --to js     > "$D/parity.js"  2>/dev/null
"$B" transpile "$D/parity.ainl" --to ruby  > "$D/parity.rb"  2>/dev/null

if [ "$ref" == "" ]; then
  echo "FAIL parity: the interpreter produced no reference output"
  fail=1
fi

check_parity() {
  local label="$1" cmd="$2" out
  out=$($cmd 2>&1)
  if [ "$out" == "$ref" ]; then
    echo "ok   caught messages are byte-identical: $label"
  else
    echo "FAIL caught messages differ on $label"
    diff <(printf '%s\n' "$ref") <(printf '%s\n' "$out") | sed 's/^/       /'
    fail=1
  fi
}

check_parity python "python3 $D/parity.py"
check_parity js     "node $D/parity.js"
check_parity ruby   "ruby $D/parity.rb"

# AOT: a portable program, so it must compile and match byte-for-byte.
if "$B" compile "$D/parity.ainl" -o "$D/parity.aot" >/dev/null 2>&1; then
  out=$("$D/parity.aot" 2>&1)
  if [ "$out" == "$ref" ]; then
    echo "ok   caught messages are byte-identical: aot"
  else
    echo "FAIL caught messages differ on aot"
    diff <(printf '%s\n' "$ref") <(printf '%s\n' "$out") | sed 's/^/       /'
    fail=1
  fi
  # The generated C must compile clean — a `-Wcomment` from a nested comment
  # terminator in the unwind doc-comment shipped once and warned on EVERY
  # AOT compile, so it is worth pinning.
  if "$B" compile "$D/parity.ainl" -o "$D/parity2.aot" 2>&1 | grep -qi warning; then
    echo "FAIL the AOT compile emitted a warning"
    fail=1
  else
    echo "ok   the AOT compile is warning-free"
  fi
else
  echo "FAIL the AOT backend refused a portable try/catch program"
  fail=1
fi

[ "$fail" -eq 0 ] && echo "SYNTAX 3e: every doc claim verified" || echo "SYNTAX 3e: CLAIM VERIFICATION FAILED"
exit "$fail"
