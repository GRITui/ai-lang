#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3l (the key-value layer)
# against the real binary.
#
# The sibling of check-docs-db.sh, for the same reason. The 3l section is almost
# entirely about behaviour a host file API does not have — a tombstone delete, a
# sorted key list, an int that stays an int, a `nil` that means both "absent"
# and "deleted" — so every claim in it is a way the docs can go stale while the
# code stays correct. A unit test cannot see doc drift; this can.
#
# The error strings are asserted exactly, because they are part of the
# documented contract and are what try/catch callers match on.
#
# The cross-backend half of the section — the C hand-port, the cross-process
# persistence test, and the transpiler refusals — lives in the test suites, not
# here; this script is the documentation half.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
# The release binary is only trusted when it is at least as new as the sources;
# otherwise a stale build becomes the reference and every claim "fails".
if [ -x "$B" ] && [ target/debug/ainl -nt "$B" ]; then
  B=./target/debug/ainl
fi
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

# Every case gets its own scratch directory, because these builtins create files
# and a suite sharing one tree would be testing its own leftovers.
want() {
  local label="$1" expected="$2" src="$3"
  local dir="$D/$(echo "$label" | tr -c 'a-zA-Z0-9' '_')"
  mkdir -p "$dir"
  printf '%s\n' "$src" > "$dir/t.ainl"
  local got
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1)
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "     want: $expected"
    echo "     got:  $got"
    fail=1
  fi
}

# ---- the section's own example, verbatim ------------------------------------
# The section opens with a store-then-reopen program and quotes its output. If
# that program stops working, the first thing a reader tries is wrong.
{
  dir="$D/section-example"
  mkdir -p "$dir"
  # The section's example opens, sets, closes, then REOPENS — so it has to be
  # two files to be a persistence claim rather than an in-memory one.
  cat > "$dir/a.ainl" <<'EOF'
(def h (db-open "settings.ainl-db"))
(db-set h "theme" "dark")
(db-set h "columns" 80)
(db-set h "recent" (list "a.ainl" "b.ainl"))
(db-set h "onboarded" true)
(db-flush h)
(db-close h)
EOF
  cat > "$dir/b.ainl" <<'EOF'
(def h (db-open "settings.ainl-db"))
(print (db-get h "theme"))
(print (db-get h "columns"))
(print (db-get h "missing"))
(db-del h "theme")
(print (db-count h))
(print (db-keys h))
(db-close h)
EOF
  got1=$(cd "$dir" && "$OLDPWD/$B" run a.ainl 2>&1)
  got2=$(cd "$dir" && "$OLDPWD/$B" run b.ainl 2>&1)
  want_out="dark
80
nil
3
(\"columns\" \"onboarded\" \"recent\")"
  if [ "$got1" == "" ] && [ "$got2" == "$want_out" ]; then
    echo "ok   the section's own example prints what the section says"
  else
    echo "FAIL the section's own example prints what the section says"
    echo "     want: $want_out"
    echo "     got:  $got2"
    fail=1
  fi
}

# ---- return values ---------------------------------------------------------
# "db-set returns nil, db-del returns a bool, db-count returns an int" is
# load-bearing: a caller chains on it, and `if (db-del ...)` depends on it.
want "db-set returns nil" \
  "nil" \
  '(do (def h (db-open "d.ainl-db")) (print (db-set h "k" 1)) (db-close h))'

want "db-del returns true then false" \
  "(true false)" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "k" 1)
      (def r (list (db-del h "k") (db-del h "k")))
      (db-close h)
      (print r))'

want "db-count returns an int and db-keys a list" \
  "(1 (\"k\"))" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "k" 1)
      (def r (list (db-count h) (db-keys h)))
      (db-close h)
      (print r))'

# ---- every value type round-trips -------------------------------------------
# The section's claim is "every value type round-trips exactly, including the
# int/float distinction". `=` would not catch an int turning into a float, so
# these compare the *re-serialized* value, which does.
want "every value type round-trips" \
  "[42,-7,1.5,1.0,true,false,null,\"hello\",\"\",[1,\"two\",false,null,[3,4]]]" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "a" 42) (db-set h "b" -7) (db-set h "c" 1.5) (db-set h "d" 1.0)
      (db-set h "e" true) (db-set h "f" false) (db-set h "g" nil)
      (db-set h "h" "hello") (db-set h "i" "")
      (db-set h "j" (list 1 "two" false nil (list 3 4)))
      (def r (json-serialize (list (db-get h "a") (db-get h "b") (db-get h "c")
                                    (db-get h "d") (db-get h "e") (db-get h "f")
                                    (db-get h "g") (db-get h "h") (db-get h "i")
                                    (db-get h "j"))))
      (db-close h)
      (print r))'

# The int/float case on its own, because it is the one the section singles out.
want "an int stays an int and 1.0 stays a float" \
  "(\"1\" \"1.0\")" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "i" 1) (db-set h "f" 1.0)
      (def r (list (json-serialize (db-get h "i")) (json-serialize (db-get h "f"))))
      (db-close h)
      (print r))'

# ---- last-write-wins, and a set after a delete ------------------------------
want "the last write wins" \
  "second" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "k" "first") (db-set h "k" "second")
      (db-close h)
      (def h2 (db-open "d.ainl-db"))
      (def v (db-get h2 "k"))
      (db-close h2)
      (print v))'

want "a set after a delete brings the key back" \
  "(2 1)" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "k" 1) (db-del h "k") (db-set h "k" 2)
      (def r (list (db-get h "k") (db-count h)))
      (db-close h)
      (print r))'

# ---- deletion removes the key from all three readers -----------------------
# The three assertions are together on purpose: a delete that clears db-get but
# not db-keys is exactly the bug the section warns about, and either one alone
# would pass.
want "a deleted key is absent from get, keys and count" \
  "(true nil (\"b\") 1)" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "a" 1) (db-set h "b" 2)
      (def gone (db-del h "a"))
      (def r (list gone (db-get h "a") (db-keys h) (db-count h)))
      (db-close h)
      (print r))'

# ---- keys are sorted, and that is a parity requirement ---------------------
want "keys are sorted by byte value" \
  "(\"10\" \"2\" \"A\" \"a\" \"b\")" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "b" 1) (db-set h "A" 1) (db-set h "a" 1)
      (db-set h "10" 1) (db-set h "2" 1)
      (def r (db-keys h))
      (db-close h)
      (print r))'

# An overwritten key must appear ONCE. This is the assertion the C port's
# chaining index failed, and it is here so the same class of bug cannot return.
want "an overwritten key is listed and counted once" \
  "(1 (\"k\"))" \
  '(do (def h (db-open "d.ainl-db"))
      (db-set h "k" 1) (db-set h "k" 2) (db-set h "k" 3)
      (def r (list (db-count h) (db-keys h)))
      (db-close h)
      (print r))'

# ---- an empty store --------------------------------------------------------
want "an empty database has no keys and a count of zero" \
  "(() 0)" \
  '(do (def h (db-open "d.ainl-db"))
      (def r (list (db-keys h) (db-count h)))
      (db-close h)
      (print r))'

# ---- the two layers, and the sharp edge ------------------------------------
# "db-put text read through db-get": valid JSON decodes, anything else errors.
want "db-put text that is JSON reads as that value" \
  "42" \
  '(do (def h (db-open "d.ainl-db"))
      (db-put h "k" "42")
      (def v (db-get h "k"))
      (db-close h)
      (print v))'

want "db-put text that is not JSON is an error naming the text and the fix" \
  "runtime error: db-get: 'note' holds text that is not an AINL value (buy milk); store it with db-set rather than db-put at line 3, col 7 (byte 75)" \
  '(do (def h (db-open "d.ainl-db"))
      (db-put h "note" "buy milk")
      (db-get h "note"))'

# db-get-raw is the byte layer's reader, under its own name, errors and all.
want "db-get-raw returns the stored text and reports its own name" \
  "buy milk" \
  '(do (def h (db-open "d.ainl-db"))
      (db-put h "note" "buy milk")
      (def v (db-get-raw h "note"))
      (db-close h)
      (print v))'

want "a stale handle is named db-get-raw on the byte layer" \
  "runtime error: db-get-raw: handle 1 is not open at line 1, col 48 (byte 47)" \
  '(do (def h (db-open "d.ainl-db")) (db-close h) (db-get-raw h "k"))'

# ---- refusals: the error strings are the contract --------------------------
want "a function is refused with the json writer's own message" \
  "runtime error: json-serialize: cannot serialize a fn at line 1, col 1 (byte 0)" \
  '(db-set 1 "k" (fn (x) x))'

want "an arity error names the form the caller should have written" \
  "runtime error: db-set expects (db-set handle key value) at line 1, col 1 (byte 0)" \
  '(db-set 1 "k")'

want "a type error names the handle operand and its type" \
  "runtime error: db-set expects a db handle, got str at line 1, col 1 (byte 0)" \
  '(db-set "x" "k" 1)'

want "a type error names the key operand and its type" \
  "runtime error: db-set expects a str key, got int at line 1, col 1 (byte 0)" \
  '(db-set 1 5 1)'

# ---- backend scope: the transpilers refuse ---------------------------------
# The section quotes the refusal and calls it "transpiler-only", *not*
# "interpreter-only", because `ainl compile` runs these programs. If the wording
# ever reverts, the docs and the binary are wrong in the same way and this is
# the only place it shows.
for tgt in python js ruby; do
  dir="$D/refuse-$tgt"
  mkdir -p "$dir"
  printf '(db-set 1 "k" 2)\n' > "$dir/t.ainl"
  got=$(cd "$dir" && "$OLDPWD/$B" transpile --to "$tgt" t.ainl 2>&1)
  case "$got" in
    *transpiler-only*) echo "ok   transpile --to $tgt refuses with 'transpiler-only'" ;;
    *) echo "FAIL transpile --to $tgt refuses with 'transpiler-only'"; echo "     got: $got"; fail=1 ;;
  esac
  case "$got" in
    *interpreter-only*)
      echo "FAIL transpile --to $tgt must not call it interpreter-only (ainl compile runs it)"
      fail=1 ;;
    *) echo "ok   transpile --to $tgt does not mislabel it interpreter-only" ;;
  esac
  case "$got" in
    *'db-set'*byte*) echo "ok   the $tgt refusal names the symbol and the offset" ;;
    *) echo "FAIL the $tgt refusal names the symbol and the offset"; fail=1 ;;
  esac
done

# ---- backend scope: AOT does NOT refuse ------------------------------------
# The asymmetry is the point of the section, so it gets its own assertion.
{
  dir="$D/aot"
  mkdir -p "$dir"
  printf '(do (def h (db-open "d.ainl-db")) (db-set h "k" (list 1 2)) (db-close h))\n' > "$dir/t.ainl"
  if (cd "$dir" && "$OLDPWD/$B" compile t.ainl -o t >/dev/null 2>&1); then
    echo "ok   ainl compile accepts a key-value program"
  else
    echo "FAIL ainl compile accepts a key-value program"
    fail=1
  fi
}

echo
[ "$fail" -eq 0 ] && echo "SYNTAX 3l: every doc claim verified" || echo "SYNTAX 3l: DOC CLAIMS FAILED"
exit "$fail"
