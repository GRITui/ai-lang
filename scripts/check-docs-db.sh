#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3k (db-open/db-put/
# db-get-raw/db-flush/db-close) against the real binary.
#
# The sibling of check-docs-fs.sh, for the same reason. A storage section is
# almost entirely about behaviour a host file API does *not* have by default —
# an append-only log, last-write-wins, a missing key as nil, a 64-handle cap, a
# foreign file refused rather than repaired — so every claim in it is a way the
# docs can quietly become wrong while the code stays correct. A unit test cannot
# see doc drift; this can.
#
# The error strings are asserted exactly, because they are part of the
# documented contract and are what `try`/`catch` callers match on.
#
# The cross-backend half of the section — the C hand-port, the crash test, and
# the transpiler refusals — lives in the test suites, not here; this script is
# the documentation half.
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

# The same, for a program that is expected to fail AND to leave the filesystem
# in a particular state. `after` runs in the case's directory once the program
# has run, so the "leaves no file behind" claims can be asserted rather than
# asserted-by-comment.
want_fs() {
  local label="$1" expected="$2" src="$3" after="$4"
  local dir="$D/$(echo "$label" | tr -c 'a-zA-Z0-9' '_')"
  mkdir -p "$dir"
  printf '%s\n' "$src" > "$dir/t.ainl"
  local got state
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1)
  state=$(cd "$dir" && eval "$after" 2>&1)
  if [ "$got" == "$expected" ] && [ "$state" == "clean" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "     want: $expected"
    echo "     got:  $got"
    echo "     fs:   $state (expected clean)"
    fail=1
  fi
}

# ---- the documented example, verbatim -------------------------------------
# The section opens with this program and its printed output. If the example
# stops working, the first thing a reader tries is wrong.
want "the section's own example prints what the section says" \
  "buy oat milk
nil" \
  '(def h (db-open "notes.ainl-db"))
(db-put h "todo" "buy milk")
(db-put h "todo" "buy oat milk")
(db-flush h)
(print (db-get-raw  h "todo"))
(print (db-get-raw  h "absent"))
(db-close h)'

# ---- return values --------------------------------------------------------
# "db-put/db-flush/db-close return nil" is load-bearing: a caller chains on it.
want "put, flush and close all return nil" \
  "(nil nil nil)" \
  '(do (def h (db-open "d.ainl-db"))
      (def r (list (db-put h "k" "v") (db-flush h) (db-close h)))
      (print r))'

want "db-open returns the handle 1" \
  "1" \
  '(do (def h (db-open "d.ainl-db")) (print h) (db-close h))'

# ---- last write wins, and the log is append-only --------------------------
want "an overwrite is visible to a reader" \
  "second" \
  '(do (def h (db-open "d.ainl-db"))
      (db-put h "k" "first")
      (db-put h "k" "second")
      (db-close h)
      (def h2 (db-open "d.ainl-db"))
      (print (db-get-raw  h2 "k"))
      (db-close h2))'

# "the older record is still on disk" is a format claim, so it is checked
# against the bytes rather than through an AINL read — `read-file` is for text
# and refuses the binary log on purpose.
{
  dir="$D/append-only"
  mkdir -p "$dir"
  printf '(do (def h (db-open "d.ainl-db"))
        (db-put h "k" "first") (db-put h "k" "second") (db-close h))\n' > "$dir/t.ainl"
  (cd "$dir" && "$OLDPWD/$B" run t.ainl >/dev/null 2>&1)
  if grep -q first "$dir/d.ainl-db" && grep -q second "$dir/d.ainl-db"; then
    echo "ok   the older record is still on disk: the log is append-only"
  else
    echo "FAIL the older record is still on disk: the log is append-only"
    fail=1
  fi
}

# ---- a missing key is nil, not an error -----------------------------------
want "a key that was never written is nil" \
  "nil" \
  '(do (def h (db-open "d.ainl-db")) (print (db-get-raw  h "absent")) (db-close h))'

# ---- handle reuse ---------------------------------------------------------
want "a closed handle number comes back on the next open" \
  "true" \
  '(do (def a (db-open "a.ainl-db"))
      (def b (db-open "b.ainl-db"))
      (db-close a)
      (def c (db-open "c.ainl-db"))
      (def r (= c a))
      (db-close b) (db-close c)
      (print r))'

# ---- refusals: the error strings are the contract -------------------------
want "a stale handle names the number" \
  "runtime error: db-get-raw: handle 1 is not open at line 1, col 48 (byte 47)" \
  '(do (def h (db-open "d.ainl-db")) (db-close h) (db-get-raw  h "k"))'

# The single-line cases below carry no such dependency: their offset is byte 0,
# because the call is the whole form.

want "a handle that was never issued is refused the same way" \
  "runtime error: db-get-raw: handle 7 is not open at line 1, col 1 (byte 0)" \
  '(db-get-raw  7 "k")'

want "handle 0 is refused, not read off the front of the table" \
  "runtime error: db-get-raw: handle 0 is not open at line 1, col 1 (byte 0)" \
  '(db-get-raw  0 "k")'

want "a negative handle is refused" \
  "runtime error: db-get-raw: handle -1 is not open at line 1, col 1 (byte 0)" \
  '(db-get-raw  -1 "k")'

# The foreign-file case needs the bad file to exist first, so it cannot go
# through `want` — which starts from an empty directory.
{
  dir="$D/foreign"
  mkdir -p "$dir"
  echo "this is not a database, it is a text file" > "$dir/bad.ainl-db"
  printf '(db-open "bad.ainl-db")\n' > "$dir/t.ainl"
  want_msg="runtime error: db-open: 'bad.ainl-db' is not an AINL database at line 1, col 1 (byte 0)"
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1)
  if [ "$got" == "$want_msg" ]; then
    echo "ok   a foreign file is refused, not repaired"
  else
    echo "FAIL a foreign file is refused, not repaired"
    echo "     want: $want_msg"
    echo "     got:  $got"
    fail=1
  fi
}

want "an arity error names the form the caller should have written" \
  "runtime error: db-get-raw expects (db-get-raw handle key) at line 1, col 1 (byte 0)" \
  '(db-get-raw 1)'

want "a type error names the operand and its type" \
  "runtime error: db-put expects a str value, got int at line 1, col 1 (byte 0)" \
  '(db-put 1 "k" 2)'

# ---- the 64-handle cap, and the no-side-effect ordering -------------------
# The section claims the cap is 64 AND that the check runs before the file is
# created. Both are asserted: the second is the one a natural implementation
# gets wrong, and it is invisible unless you look for the file afterwards.
{
  dir="$D/cap"
  mkdir -p "$dir"
  {
    echo "(do"
    i=1
    while [ "$i" -le 64 ]; do
      echo "  (def h$i (db-open \"d$i.ainl-db\"))"
      i=$((i + 1))
    done
    echo '  (db-open "overflow.ainl-db"))'
  } > "$dir/t.ainl"
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1)
  # The message is matched without its `at line …, col … (byte …)` suffix: the
  # offset depends on this script's own indentation of the generated program,
  # which is not a claim the documentation makes. The text before it is.
  if [ "${got%% at line*}" == "runtime error: db-open: too many open databases (max 64)" ]; then
    echo "ok   the 65th open is refused with the documented message"
  else
    echo "FAIL the 65th open is refused with the documented message"
    echo "     want: runtime error: db-open: too many open databases (max 64)"
    echo "     got:  $got"
    fail=1
  fi
  if [ -e "$dir/overflow.ainl-db" ]; then
    echo "FAIL a refused db-open created the file anyway"
    fail=1
  else
    echo "ok   a refused db-open created no file"
  fi
}

# ---- the format -----------------------------------------------------------
# The header is asserted from the bytes, because "AINLDB" and a 16-byte header
# are a format claim, not a behaviour one, and no error message covers them.
{
  dir="$D/format"
  mkdir -p "$dir"
  printf '(do (def h (db-open "d.ainl-db")) (db-put h "k" "v") (db-close h))\n' > "$dir/t.ainl"
  (cd "$dir" && "$OLDPWD/$B" run t.ainl >/dev/null 2>&1)
  magic=$(dd if="$dir/d.ainl-db" bs=1 count=6 2>/dev/null)
  ver=$(od -An -tu1 -j6 -N1 "$dir/d.ainl-db" 2>/dev/null | tr -d ' ')
  hlen=$(od -An -tu4 -j8 -N4 "$dir/d.ainl-db" 2>/dev/null | tr -d ' ')
  if [ "$magic" == "AINLDB" ] && [ "$ver" == "1" ] && [ "$hlen" == "16" ]; then
    echo "ok   the header is AINLDB / version 1 / header_len 16"
  else
    echo "FAIL the header is AINLDB / version 1 / header_len 16"
    echo "     got: magic=$magic version=$ver header_len=$hlen"
    fail=1
  fi
  # One record: 12 header bytes + 1 key byte + 1 value byte, after 16.
  size=$(wc -c < "$dir/d.ainl-db" | tr -d ' ')
  if [ "$size" == "30" ]; then
    echo "ok   a one-character record is 30 bytes (16 header + 12 + 1 + 1)"
  else
    echo "FAIL a one-character record is 30 bytes (16 header + 12 + 1 + 1)"
    echo "     got: $size"
    fail=1
  fi
}

# ---- crash recovery: the property the format exists for -------------------
# A torn tail is dropped, the intact records survive, and the file is truncated
# so the next append lands on a clean boundary.
{
  dir="$D/torn"
  mkdir -p "$dir"
  printf '(do (def h (db-open "d.ainl-db"))
        (db-put h "a" "1") (db-put h "b" "2") (db-close h))\n' > "$dir/w.ainl"
  (cd "$dir" && "$OLDPWD/$B" run w.ainl >/dev/null 2>&1)
  good=$(wc -c < "$dir/d.ainl-db" | tr -d ' ')
  # Append a half-written record: a valid fixed header, a short body.
  printf '\005\000\000\000\143\000\000\000\000\000\000\000parti' >> "$dir/d.ainl-db"
  printf '(do (def h (db-open "d.ainl-db"))
        (print (db-get-raw  h "a") (db-get-raw  h "b") (db-get-raw  h "parti"))
        (db-close h))\n' > "$dir/r.ainl"
  got=$(cd "$dir" && "$OLDPWD/$B" run r.ainl 2>&1)
  want_out="1 2 nil"
  after=$(wc -c < "$dir/d.ainl-db" | tr -d ' ')
  if [ "$got" == "$want_out" ]; then
    echo "ok   a torn tail is dropped and the intact records survive"
  else
    echo "FAIL a torn tail is dropped and the intact records survive"
    echo "     want: $want_out"
    echo "     got:  $got"
    fail=1
  fi
  if [ "$after" == "$good" ]; then
    echo "ok   the torn tail is truncated on disk, not just ignored"
  else
    echo "FAIL the torn tail is truncated on disk, not just ignored"
    echo "     want: $good bytes"
    echo "     got:  $after bytes"
    fail=1
  fi
}

# ---- backend scope: the transpilers refuse --------------------------------
# The section quotes the refusal and calls it "transpiler-only", *not*
# "interpreter-only", because `ainl compile` runs these programs. If the wording
# ever reverts, the docs and the binary are both wrong in the same way and this
# is the only place it shows.
for tgt in python js ruby; do
  dir="$D/refuse-$tgt"
  mkdir -p "$dir"
  printf '(db-open "d.ainl-db")\n' > "$dir/t.ainl"
  got=$(cd "$dir" && "$OLDPWD/$B" transpile --to "$tgt" t.ainl 2>&1)
  case "$got" in
    *transpiler-only*) echo "ok   transpile --to $tgt refuses with 'transpiler-only'" ;;
    *)
      echo "FAIL transpile --to $tgt refuses with 'transpiler-only'"
      echo "     got: $got"
      fail=1
      ;;
  esac
  case "$got" in
    *interpreter-only*)
      echo "FAIL transpile --to $tgt must not call it interpreter-only (ainl compile runs it)"
      fail=1
      ;;
    *) echo "ok   transpile --to $tgt does not mislabel it interpreter-only" ;;
  esac
  # The refusal must name the symbol and the offset, or it is not actionable.
  case "$got" in
    *'db-open'*byte*) echo "ok   the $tgt refusal names the symbol and the offset" ;;
    *)
      echo "FAIL the $tgt refusal names the symbol and the offset"
      fail=1
      ;;
  esac
done

# ---- backend scope: AOT does NOT refuse ----------------------------------
# The asymmetry is the point of the section, so it gets its own assertion: a
# "storage is too hard for a compiled binary" change would break the docs and
# the section's whole argument while every refusal test kept passing.
{
  dir="$D/aot"
  mkdir -p "$dir"
  printf '(do (def h (db-open "d.ainl-db")) (db-put h "k" "v") (db-close h))\n' > "$dir/t.ainl"
  if (cd "$dir" && "$OLDPWD/$B" compile t.ainl -o t >/dev/null 2>&1); then
    echo "ok   ainl compile accepts a storage program"
  else
    echo "FAIL ainl compile accepts a storage program"
    fail=1
  fi
}

echo
[ "$fail" -eq 0 ] && echo "SYNTAX 3k: every doc claim verified" || echo "SYNTAX 3k: DOC CLAIMS FAILED"
exit "$fail"
