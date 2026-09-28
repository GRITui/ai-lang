#!/usr/bin/env bash
# `ainl test` end to end, against the real binary.
#
# The library and parity suites prove the `test` builtin behaves; this proves the
# *runner* does, which is the half CI actually depends on: discover a directory,
# report a summary, and exit non-zero on failure. A runner that printed a nice
# summary and always exited 0 would pass every other suite in this repo.
#
# Each case is asserted on the exit code, because that is the contract a CI step
# consumes — nothing else about the output is load-bearing.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl..."; cargo build -q || exit 1
  BIN=target/debug/ainl
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

check() {
  # check <name> <expected-exit> <command...>
  local name="$1" want="$2"; shift 2
  "$@" > "$WORK/out" 2> "$WORK/err"
  local got=$?
  if [ "$got" -eq "$want" ]; then
    echo "ok   $name (exit $got)"
  else
    echo "FAIL $name — expected exit $want, got $got"
    sed 's/^/    /' "$WORK/out" "$WORK/err"
    fail=1
  fi
}

# ---- a real suite passes ---------------------------------------------------
check "the repo's own suite passes" 0 "$BIN" test tests/

# ---- a directory of test files is discovered -------------------------------
mkdir -p "$WORK/suite"
cat > "$WORK/suite/a.ainl" <<'EOF'
(test "a" 1 "1")
(test "b" "x" "x")
EOF
cat > "$WORK/suite/b.ainl" <<'EOF'
(test "c" (+ 1 2) "3")
EOF
check "a directory of two files runs" 0 "$BIN" test "$WORK/suite" --quiet

# Files are discovered in sorted order, so a report is the same on every
# machine. Checking the order in the output is what pins it.
"$BIN" test "$WORK/suite" > "$WORK/out" 2>&1
if grep -q "a.ainl" "$WORK/out" && grep -n "a.ainl" "$WORK/out" | cut -d: -f1 |
     awk -v n="$(grep -n 'b.ainl' "$WORK/out" | cut -d: -f1)" '$1 < n {found=1} END {exit !found}'; then
  echo "ok   files are reported in sorted order"
else
  echo "FAIL files are not reported in sorted order"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

# ---- a single file is accepted as a path -----------------------------------
check "a single file is accepted" 0 "$BIN" test "$WORK/suite/a.ainl" --quiet

# ---- failure exits non-zero, with a readable diff --------------------------
cat > "$WORK/bad.ainl" <<'EOF'
(test "deliberately wrong" (+ 1 2) "4")
EOF
check "a failing test exits non-zero" 1 "$BIN" test "$WORK/bad.ainl"

"$BIN" test "$WORK/bad.ainl" > "$WORK/out" 2>&1
for want in "deliberately wrong" "expected 4" "got 3" "1 failed"; do
  if grep -q "$want" "$WORK/out"; then
    echo "ok   the failure report contains '$want'"
  else
    echo "FAIL the failure report is missing '$want':"
    sed 's/^/    /' "$WORK/out"
    fail=1
  fi
done

# The pass/fail split must be exact. A runner that counted the *failing* test as
# passed would report "1 passed, 1 failed" here, for a file whose only test
# failed — a count that reads as if something had succeeded.
"$BIN" test "$WORK/bad.ainl" > "$WORK/out" 2>&1
if grep -q "^0 passed, 1 failed" "$WORK/out"; then
  echo "ok   a file whose only test failed reports 0 passed, 1 failed"
else
  echo "FAIL the pass/fail split is wrong:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi
if grep -q "every test failed" "$WORK/out"; then
  echo "ok   an all-failing suite says so, rather than blaming an empty suite"
else
  echo "FAIL an all-failing suite reported the wrong cause:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

# A file whose second test fails must report exactly 1 passed — the one before it.
cat > "$WORK/second.ainl" <<'EOF'
(test "first is fine" 1 "1")
(test "second is wrong" 2 "3")
(test "third never runs" 3 "3")
EOF
"$BIN" test "$WORK/second.ainl" > "$WORK/out" 2>&1
if grep -q "^1 passed, 1 failed" "$WORK/out"; then
  echo "ok   a later failure counts the tests before it"
else
  echo "FAIL a later failure was miscounted:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

# ---- one broken file does not hide the others ------------------------------
# The first failing test aborts its file, but the runner must keep going: a
# suite that stops at the first failure reports one problem per run and hides
# the rest.
mkdir -p "$WORK/mixed"
cat > "$WORK/mixed/1-bad.ainl" <<'EOF'
(test "broken" 1 "2")
EOF
cat > "$WORK/mixed/2-good.ainl" <<'EOF'
(test "fine" 1 "1")
EOF
check "a broken file does not stop the suite" 1 "$BIN" test "$WORK/mixed"
"$BIN" test "$WORK/mixed" > "$WORK/out" 2>&1
if grep -q "1 passed" "$WORK/out"; then
  echo "ok   the passing file still reported its passing test"
else
  echo "FAIL the passing file was not counted:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

# ---- a non-test error is reported as an error, not a failed test ------------
# A test file that raises outside a `(test ...)` is a broken program, and
# conflating that with a failed assertion would send a reader to edit the wrong
# thing.
cat > "$WORK/raises.ainl" <<'EOF'
(error "boom")
EOF
check "a program error exits non-zero" 1 "$BIN" test "$WORK/raises.ainl"
"$BIN" test "$WORK/raises.ainl" > "$WORK/out" 2>&1
if grep -qi "error" "$WORK/out" && ! grep -q "test failed" "$WORK/out"; then
  echo "ok   a program error is reported as an error, not a test failure"
else
  echo "FAIL a program error was misreported:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

# ---- an empty suite is a failure, never a vacuous success -------------------
mkdir -p "$WORK/empty"
: > "$WORK/empty/nothing.ainl"
check "a file with no tests is not a silent pass" 1 "$BIN" test "$WORK/empty"

check "an empty directory is a failure" 1 "$BIN" test "$WORK/no-such-dir"

# ---- unknown flags are rejected -------------------------------------------
check "an unknown flag is rejected" 1 "$BIN" test tests --bogus

# ---- --quiet keeps a passing run to its summary ----------------------------
"$BIN" test tests --quiet > "$WORK/out" 2>&1
if [ "$(wc -l < "$WORK/out" | tr -d ' ')" -le 5 ]; then
  echo "ok   --quiet suppresses the per-file lines"
else
  echo "FAIL --quiet printed per-file lines:"
  sed 's/^/    /' "$WORK/out"
  fail=1
fi

[ "$fail" -eq 0 ] && echo "ainl test: all runner checks pass" || echo "ainl test: RUNNER CHECKS FAILED"
exit "$fail"
