#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3h (mkdir/rename/copy/
# is-dir/file-size) against the real binary.
#
# The section is almost entirely about behaviour that is *not* what a host
# library does by default — mkdir refusing an existing path, rename refusing an
# existing destination, file-size counting bytes, lstat not following a
# symlink — so every one of those claims is a way the docs can quietly become
# wrong. A unit test cannot see doc drift; this can.
#
# Each `want` runs one snippet and compares the *exact* output, error text and
# all, because the error strings are part of the documented contract and are
# what `try`/`catch` users match on.
#
# The cross-backend half of the section lives in scripts/check-fs-builtins.sh;
# this script is the documentation half.
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

# Every case gets its own scratch directory, because these builtins mutate the
# filesystem and a suite that shares one tree would be testing its own leftovers.
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

# ---- the option is a string, not a keyword --------------------------------
want "a bare :recursive is an unbound symbol, not an option" \
  "runtime error: unbound symbol ':recursive' at line 1, col 16 (byte 15)" \
  '(mkdir "a/b/c" :recursive)'

want "an unknown option is named, not ignored" \
  "runtime error: mkdir: unknown option ':parents' at line 1, col 1 (byte 0)" \
  '(mkdir "a/b/c" ":parents")'

want '":recursive" is accepted and creates every level' \
  "true" \
  '(do (mkdir "a/b/c" ":recursive") (print (is-dir "a/b/c")))'

# ---- mkdir refuses an existing path, in both modes -------------------------
# Each case is self-contained: `want` gives every case a fresh directory, so a
# case that needs an existing path has to create it in the same snippet.
want "a fresh mkdir succeeds" "" '(mkdir "d")'
want "mkdir on an existing path says so" \
  "runtime error: mkdir: cannot create 'd': it exists at line 1, col 17 (byte 16)" \
  '(do (mkdir "d") (mkdir "d"))'
want "and so does the recursive form" \
  "runtime error: mkdir: cannot create 'd': it exists at line 1, col 17 (byte 16)" \
  '(do (mkdir "d") (mkdir "d" ":recursive"))'
want "the documented guard makes a re-run work" "" \
  '(do (mkdir "d") (if (not (file-exists "d")) (mkdir "d" ":recursive")))'
want "a missing parent is an error, naming the path the caller wrote" \
  "runtime error: mkdir: cannot create 'absent/child' at line 1, col 1 (byte 0)" \
  '(mkdir "absent/child")'
want "and the parent is not created behind the caller's back" "nil" \
  '(do (try (mkdir "absent/child") (catch (e) nil))
     (print (is-dir "absent")))'

# ---- rename refuses an existing destination --------------------------------
want "rename moves a file" "nil" \
  '(do (write-file "a.txt" "A") (print (rename "a.txt" "b.txt")))'
want "the content moved with it, and the source is gone" "A nil" \
  '(do (write-file "a.txt" "A") (rename "a.txt" "b.txt")
     (print (read-file "b.txt") (file-exists "a.txt")))'
# The position suffix is omitted deliberately here: it depends on where the
# failing call sits inside the snippet, and the *message* is the documented
# contract. The one-line form further down pins the full text including
# position, which is what a reader pasting the doc's snippet actually sees.
want "rename onto an existing destination is refused" \
  "rename: cannot move 'a.txt': 'b.txt' exists" \
  '(do (write-file "a.txt" "A") (rename "a.txt" "b.txt")
     (write-file "a.txt" "A") (write-file "b.txt" "B")
     (try (rename "a.txt" "b.txt") (catch (e) (print (get e "message")))))'
want "the destination survived the refusal" "B A" \
  '(do (write-file "a.txt" "A") (write-file "b.txt" "B")
     (try (rename "a.txt" "b.txt") (catch (e) nil))
     (print (read-file "b.txt") (read-file "a.txt")))'
want "a missing source has its own message" \
  "runtime error: rename: cannot move 'missing.txt': it does not exist at line 1, col 1 (byte 0)" \
  '(rename "missing.txt" "b.txt")'
want "rename moves a whole directory tree" "true x" \
  '(do (mkdir "src/inner" ":recursive") (write-file "src/inner/deep.txt" "x")
     (rename "src" "dst")
     (print (is-dir "dst/inner") (read-file "dst/inner/deep.txt")))'

# ---- copy ------------------------------------------------------------------
want "copy returns nil and preserves the content" "nil A" \
  '(do (write-file "a.txt" "A") (print (copy "a.txt" "b.txt") (read-file "b.txt")))'
want "copy DOES overwrite an existing destination" "nil A" \
  '(do (write-file "a.txt" "A") (write-file "c.txt" "C") (copy "a.txt" "c.txt")
     (print (copy "a.txt" "c.txt") (read-file "c.txt")))'
want "the two copies then diverge" "zulu A" \
  '(do (write-file "a.txt" "A") (copy "a.txt" "b.txt")
     (write-file "b.txt" "zulu")
     (print (read-file "b.txt") (read-file "a.txt")))'
want "copy refuses a directory, by name" \
  "copy: cannot copy 'src': it is a directory" \
  '(do (mkdir "src") (try (copy "src" "dst") (catch (e) (print (get e "message")))))'

# ---- file-size counts bytes ------------------------------------------------
want "file-size is 6 bytes for a 5-character string" "6 5" \
  '(do (write-file "u.txt" "héllo")
     (print (file-size "u.txt") (len (read-file "u.txt"))))'
want "a directory is an error, not the inode size" \
  "file-size: cannot read 'd': it is a directory" \
  '(do (mkdir "d") (try (file-size "d") (catch (e) (print (get e "message")))))'
want "an empty file is zero" "0" \
  '(do (write-file "e.txt" "") (print (file-size "e.txt")))'

# ---- is-dir ----------------------------------------------------------------
want "a directory is true and a file is nil" "true nil" \
  '(do (mkdir "d") (write-file "f" "x")
     (print (is-dir "d") (is-dir "f")))'
want "a missing path is nil, not an error, and is not false" "nil" \
  '(print (is-dir "nope"))'
want "a trailing separator gives the same answer" "true" \
  '(do (mkdir "d") (print (is-dir "d/")))'
want "the root survives the trim" "true" \
  '(print (is-dir "/"))'

# AINL has no `symlink` builtin, so the link is made by the shell before the
# program runs. This is the one doc claim a snippet cannot set up for itself,
# and it is the one that most needs pinning: `os.path.isdir`, `fs.statSync` and
# `File.directory?` all follow the link and would answer true, so a port that
# used the host's default would disagree with the interpreter here.
ln_dir="$D/symlink_case"
mkdir -p "$ln_dir/realdir"
ln -s "$ln_dir/realdir" "$ln_dir/link-to-dir"
printf '(print (is-dir "link-to-dir"))\n' > "$ln_dir/t.ainl"
got=$(cd "$ln_dir" && "$OLDPWD/$B" run t.ainl 2>&1)
if [ "$got" == "nil" ]; then
  echo "ok   a symlink to a directory is NOT a directory (lstat, not stat)"
else
  echo "FAIL a symlink to a directory is NOT a directory (lstat, not stat)"
  echo "     want: nil"
  echo "     got:  $got"
  fail=1
fi

# A broken symlink is still a directory entry, so it exists for `file-exists`
# and blocks `mkdir` — the lstat consequence a stat-based port gets wrong.
ln -s "$ln_dir/does-not-exist" "$ln_dir/broken"
printf '(print (file-exists "broken"))\n' > "$ln_dir/t.ainl"
got=$(cd "$ln_dir" && "$OLDPWD/$B" run t.ainl 2>&1)
if [ "$got" == "true" ]; then
  echo "ok   a broken symlink still exists for file-exists"
else
  echo "FAIL a broken symlink still exists for file-exists"
  echo "     want: true"
  echo "     got:  $got"
  fail=1
fi
printf '(mkdir "broken" ":recursive")\n' > "$ln_dir/t.ainl"
got=$(cd "$ln_dir" && "$OLDPWD/$B" run t.ainl 2>&1)
if [ "$got" == "runtime error: mkdir: cannot create 'broken': it exists at line 1, col 1 (byte 0)" ]; then
  echo "ok   and it blocks mkdir"
else
  echo "FAIL and it blocks mkdir"
  echo "     want: runtime error: mkdir: cannot create 'broken': it exists at line 1, col 1 (byte 0)"
  echo "     got:  $got"
  fail=1
fi

# ---- error message table ---------------------------------------------------
# The three headline refusal messages, in the form the docs show. The full text
# including the `line N, col C` suffix is compared, because the position is part
# of what a user sees — and the position is the *call site*, so a snippet that
# has to build its subject first reports a position inside itself. That is the
# correct behavior (an error should point at the line that failed), so the
# expected values below carry the real positions rather than papering over them
# with a wildcard.
want "the rename-exists message, with its call site" \
  "runtime error: rename: cannot move 'a.txt': 'b.txt' exists at line 1, col 55 (byte 54)" \
  '(do (write-file "a.txt" "A") (write-file "b.txt" "B") (rename "a.txt" "b.txt"))'
want "the copy-directory message" \
  "runtime error: copy: cannot copy 'src': it is a directory at line 1, col 19 (byte 18)" \
  '(do (mkdir "src") (copy "src" "dst"))'
want "the file-size-directory message" \
  "runtime error: file-size: cannot read 'd': it is a directory at line 1, col 17 (byte 16)" \
  '(do (mkdir "d") (file-size "d"))'
want "the mkdir-exists message" \
  "runtime error: mkdir: cannot create 'd': it exists at line 1, col 17 (byte 16)" \
  '(do (mkdir "d") (mkdir "d"))'
want "mkdir: wrong type" \
  "runtime error: mkdir expects a str path, got int at line 1, col 1 (byte 0)" \
  '(mkdir 1)'
want "mkdir: arity 0" \
  "runtime error: mkdir expects (mkdir path) or (mkdir path option) at line 1, col 1 (byte 0)" \
  '(mkdir)'
want "mkdir: arity 3" \
  "runtime error: mkdir expects (mkdir path) or (mkdir path option) at line 1, col 1 (byte 0)" \
  '(mkdir "a" "b" "c")'
want "rename: wrong type" \
  "runtime error: rename expects a str path, got int at line 1, col 1 (byte 0)" \
  '(rename 1 2)'
want "rename: arity" \
  "runtime error: rename expects (rename from to) at line 1, col 1 (byte 0)" \
  '(rename "a")'
want "copy: wrong type" \
  "runtime error: copy expects a str path, got int at line 1, col 1 (byte 0)" \
  '(copy 1 2)'
want "copy: arity" \
  "runtime error: copy expects (copy from to) at line 1, col 1 (byte 0)" \
  '(copy "a")'
want "is-dir: wrong type" \
  "runtime error: is-dir expects a str path, got int at line 1, col 1 (byte 0)" \
  '(is-dir 1)'
want "is-dir: arity" \
  "runtime error: is-dir expects (is-dir path) at line 1, col 1 (byte 0)" \
  '(is-dir)'
want "file-size: wrong type" \
  "runtime error: file-size expects a str path, got int at line 1, col 1 (byte 0)" \
  '(file-size 1)'
want "file-size: arity" \
  "runtime error: file-size expects (file-size path) at line 1, col 1 (byte 0)" \
  '(file-size)'

# ---- every one of them is catchable ---------------------------------------
# The table above is only useful if a program can intercept these, which is the
# point of routing them through the AINL error channel rather than a host one.
want "a caught mkdir error is a map with the documented message" \
  "mkdir: cannot create 'q': it exists" \
  '(do (mkdir "q") (try (mkdir "q") (catch (e) (print (get e "message")))))'
want "a caught file-size error too" "refused" \
  '(do (mkdir "z") (try (file-size "z") (catch (e) (print "refused"))))'

[ "$fail" -eq 0 ] && echo "SYNTAX 3h: every doc claim verified" || echo "SYNTAX 3h: DOC CLAIMS FAILED"
exit $fail
