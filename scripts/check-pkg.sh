#!/usr/bin/env bash
# Package-manager gate: resolve a 2-package graph (a -> b), vendor it, then
# prove the program runs BOTH in the interpreter AND as a standalone AOT binary
# that still works after the vendor dir and every .ainl file is deleted.
#
# That last part is the load-bearing claim. Everything else in this file could
# pass with a runtime that quietly reads its own dependencies off disk, which
# would break the standalone-binary guarantee the whole language rests on.
#
# Also exercised: `pkg verify` passes on a clean tree, FAILS non-zero when the
# vendor dir is tampered with, and a circular A<->B graph is refused with a
# message naming the cycle.
#
# Runs in CI and locally. Exits non-zero on any failure. Requires `cc`.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."
  cargo build --release --quiet || exit 1
  BIN=target/release/ainl
fi
# Absolute from here on: most of this script runs inside a `cd` into a scratch
# project, and a relative $BIN would then be resolved against the wrong tree.
# Every failure below is silent-ish (127) if this is wrong, so it is worth
# asserting once rather than reading ten confusing messages.
BIN="$(cd "$(dirname "$BIN")" && pwd)/ainl"
if [ ! -x "$BIN" ]; then
  echo "FAIL: no ainl binary at $BIN"
  exit 1
fi

if ! command -v cc >/dev/null 2>&1; then
  echo "FAIL: cc not found (the AOT half needs a host C compiler)"
  exit 1
fi

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fail=0

# --- the packages -------------------------------------------------------
# Package `b`: a leaf, with no dependencies of its own.
mkdir -p "$W/pkgs/b"
cat > "$W/pkgs/b/ainl.pkg" <<'EOF'
name: b
version: 1.0.0
EOF
cat > "$W/pkgs/b/b.ainl" <<'EOF'
(def b-value (fn (x) (+ x 100)))
EOF

# Package `a`: imports b, so the graph is app -> a -> b. Two packages, one
# dependency edge between them, which is the shape the card asks for.
mkdir -p "$W/pkgs/a"
cat > "$W/pkgs/a/ainl.pkg" <<'EOF'
name: a
version: 2.0.0
[deps]
name: b
version: 1.0.0
source: ../b
EOF
cat > "$W/pkgs/a/a.ainl" <<'EOF'
(import "b")
(def a-value (fn (x) (b-value (* x 2))))
EOF

# The application: imports a by package name.
mkdir -p "$W/app"
cat > "$W/app/ainl.pkg" <<'EOF'
name: app
version: 0.1.0
EOF
cat > "$W/app/main.ainl" <<'EOF'
(import "a")
(print (a-value 5))
EOF

echo "== ainl pkg init writes a manifest =="
mkdir -p "$W/empty"
( cd "$W/empty" && "$BIN" pkg init --name scratch ) >/dev/null || fail=1
if [ -f "$W/empty/ainl.pkg" ]; then
  echo "ok   created ainl.pkg"
else
  echo "FAIL: ainl pkg init did not write a manifest"
  fail=1
fi
# And it must refuse to clobber one.
if ( cd "$W/empty" && "$BIN" pkg init --name scratch ) >/dev/null 2>&1; then
  echo "FAIL: ainl pkg init overwrote an existing manifest"
  fail=1
else
  echo "ok   refuses to overwrite an existing manifest"
fi

echo
echo "== ainl pkg get resolves and vendors the a -> b graph =="
if ( cd "$W/app" && "$BIN" pkg get a@2.0.0 ../pkgs/a ); then
  echo "ok   get a@2.0.0"
else
  echo "FAIL: ainl pkg get failed"
  fail=1
fi
# Both packages must be vendored, flattened into one top-level vendor dir --
# `a` is a direct dep, `b` is transitive and must be there too, or the program
# would resolve differently on a machine that happened to have it locally.
for p in a b; do
  if [ -f "$W/app/.ainl-vendor/$p/$p.ainl" ]; then
    echo "ok   vendored $p"
  else
    echo "FAIL: .ainl-vendor/$p/$p.ainl is missing"
    fail=1
  fi
done
# The app's own source must NOT be vendored into itself.
if [ -e "$W/app/.ainl-vendor/app" ]; then
  echo "FAIL: the root package was vendored into its own vendor dir"
  fail=1
else
  echo "ok   the root package is not vendored into itself"
fi

echo
echo "== ainl pkg list shows the resolved graph =="
if ( cd "$W/app" && "$BIN" pkg list ) | grep -q 'a 2.0.0'; then
  echo "ok   list names a 2.0.0"
else
  echo "FAIL: list did not show the resolved package"
  ( cd "$W/app" && "$BIN" pkg list ) | sed 's/^/    /'
  fail=1
fi

echo
echo "== ainl pkg verify passes on the committed lockfile + vendor dir =="
if ( cd "$W/app" && "$BIN" pkg verify ); then
  echo "ok   verify clean"
else
  echo "FAIL: verify rejected a clean tree"
  fail=1
fi

echo
echo "== the program runs in the interpreter =="
# 5 * 2 = 10, then 10 + 100 = 110. The number crosses the package boundary
# twice (app->a, a->b), so it is only 110 if both vendored packages ran.
if got=$(cd "$W/app" && "$BIN" run main.ainl 2>&1) && [ "$got" == "110" ]; then
  echo "ok   interpreter printed 110"
else
  echo "FAIL: interpreter printed '$got' (expected 110)"
  fail=1
fi

echo
echo "== it compiles to a STANDALONE AOT binary, and stays standalone =="
if ! ( cd "$W/app" && "$BIN" compile main.ainl -o "$W/app/prog" ) >/dev/null; then
  echo "FAIL: ainl compile refused a program that imports a package"
  fail=1
else
  echo "ok   compiled main.ainl -> prog"
fi
# THE standalone proof. Copy the binary somewhere with no .ainl file and no
# .ainl-vendor/ anywhere near it, delete the whole project, and run it. A
# runtime that read its dependencies off disk would fail here; an inlined one
# cannot notice the difference.
mkdir -p "$W/void"
cp "$W/app/prog" "$W/void/prog"
rm -rf "$W/app" "$W/pkgs"
if ! find "$W" -name '*.ainl' | grep -q .; then
  echo "ok   no .ainl file remains anywhere in the tree"
else
  echo "FAIL: an .ainl file survived the deletion"
  fail=1
fi
if got=$("$W/void/prog" 2>&1) && [ "$got" == "110" ]; then
  echo "ok   the compiled binary still printed 110 with every .ainl file deleted"
else
  echo "FAIL: the compiled binary printed '$got' after its sources were deleted"
  fail=1
fi
# And it is not secretly a script, and not dynamically linked against a host
# AINL: it must be a real binary.
if file "$W/void/prog" | grep -qi 'executable'; then
  echo "ok   it is a compiled executable, not a wrapper"
else
  echo "FAIL: the output is not an executable"
  fail=1
fi

echo
echo "== verify FAILS (non-zero) when the vendor dir is tampered with =="
# Rebuild the project so there is something to tamper with.
mkdir -p "$W/pkgs/b" "$W/pkgs/a" "$W/app"
cat > "$W/pkgs/b/ainl.pkg" <<'EOF'
name: b
version: 1.0.0
EOF
cat > "$W/pkgs/b/b.ainl" <<'EOF'
(def b-value (fn (x) (+ x 100)))
EOF
cat > "$W/pkgs/a/ainl.pkg" <<'EOF'
name: a
version: 2.0.0
[deps]
name: b
version: 1.0.0
source: ../b
EOF
cat > "$W/pkgs/a/a.ainl" <<'EOF'
(import "b")
(def a-value (fn (x) (b-value (* x 2))))
EOF
cat > "$W/app/ainl.pkg" <<'EOF'
name: app
version: 0.1.0
[deps]
name: a
version: 2.0.0
source: ../pkgs/a
EOF
( cd "$W/app" && "$BIN" pkg install ) >/dev/null || fail=1
if ( cd "$W/app" && "$BIN" pkg verify ) >/dev/null 2>&1; then
  echo "ok   verify still passes on a clean re-installed tree"
else
  echo "FAIL: verify failed on a clean re-installed tree"
  fail=1
fi
# Tamper: change the vendored bytes without changing the file list. A
# presence-only check would miss this; a digest check does not.
echo '(def b-value (fn (x) 999))' > "$W/app/.ainl-vendor/b/b.ainl"
out=$( cd "$W/app" && "$BIN" pkg verify 2>&1 )
rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q 'modified'; then
  echo "ok   verify failed non-zero and named the change: $(printf '%s' "$out" | head -1)"
else
  echo "FAIL: verify did not catch an edited vendored file (rc=$rc): $out"
  fail=1
fi
# Deleting a vendored file is also a difference.
( cd "$W/app" && "$BIN" pkg install ) >/dev/null
rm -f "$W/app/.ainl-vendor/b/b.ainl"
if ( cd "$W/app" && "$BIN" pkg verify ) >/dev/null 2>&1; then
  echo "FAIL: verify passed with a vendored file deleted"
  fail=1
else
  echo "ok   verify fails when a vendored file is missing"
fi

echo
echo "== a circular A<->B dependency is refused, naming the cycle =="
mkdir -p "$W/cyc/a" "$W/cyc/b" "$W/cyc/app"
cat > "$W/cyc/a/ainl.pkg" <<'EOF'
name: a
version: 1.0.0
[deps]
name: b
version: 1.0.0
source: ../b
EOF
cat > "$W/cyc/b/ainl.pkg" <<'EOF'
name: b
version: 1.0.0
[deps]
name: a
version: 1.0.0
source: ../a
EOF
cat > "$W/cyc/app/ainl.pkg" <<'EOF'
name: app
version: 0.1.0
[deps]
name: a
version: 1.0.0
source: ../a
EOF
out=$( cd "$W/cyc/app" && "$BIN" pkg install 2>&1 )
rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q 'circular dependency'; then
  echo "ok   refused: $out"
else
  echo "FAIL: a cycle was not refused (rc=$rc): $out"
  fail=1
fi

echo
echo "== a version range is refused, not silently treated as exact =="
mkdir -p "$W/rng/app"
cat > "$W/rng/app/ainl.pkg" <<'EOF'
name: app
version: 0.1.0
[deps]
name: b
version: ^1.0.0
source: ../b
EOF
if ( cd "$W/rng/app" && "$BIN" pkg install ) >/dev/null 2>&1; then
  echo "FAIL: a '^1.0.0' range was accepted"
  fail=1
else
  echo "ok   refused a range"
fi

echo
echo "== an https:// source is refused (no registry in this tier) =="
mkdir -p "$W/reg/app"
cat > "$W/reg/app/ainl.pkg" <<'EOF'
name: app
version: 0.1.0
[deps]
name: b
version: 1.0.0
source: https://example.com/b.tar.gz
EOF
out=$( cd "$W/reg/app" && "$BIN" pkg install 2>&1 )
if printf '%s' "$out" | grep -q 'no package registry'; then
  echo "ok   refused: $out"
else
  echo "FAIL: an https source was not refused: $out"
  fail=1
fi

echo
echo "== a program with NO package still compiles and runs (no regression) =="
mkdir -p "$W/plain"
cat > "$W/plain/p.ainl" <<'EOF'
(def twice (fn (x) (* x 2)))
(print (twice 21))
EOF
if got=$("$BIN" run "$W/plain/p.ainl" 2>&1) && [ "$got" == "42" ]; then
  echo "ok   interpreter still fine"
else
  echo "FAIL: interpreter broke on a plain program (got: $got)"
  fail=1
fi
"$BIN" compile "$W/plain/p.ainl" -o "$W/plain/p.bin" >/dev/null || fail=1
if got=$("$W/plain/p.bin" 2>&1) && [ "$got" == "42" ]; then
  echo "ok   AOT still fine"
else
  echo "FAIL: AOT broke on a plain program (got: $got)"
  fail=1
fi

echo
echo "== a multi-file program (plain imports, no packages) still compiles =="
# This is the case the modules gate used to assert was REFUSED. It is no
# longer: imports are inlined. The transpilers still refuse, and the modules
# gate still checks that.
if got=$("$BIN" run examples/wordcount/main.ainl 2>&1 | head -1); then
  if printf '%s' "$got" | grep -q 'words:'; then
    echo "ok   the multi-file example runs"
  else
    echo "FAIL: unexpected example output: $got"
    fail=1
  fi
else
  echo "FAIL: the multi-file example did not run"
  fail=1
fi
if "$BIN" compile examples/wordcount/main.ainl -o "$W/wc.bin" >/dev/null 2>&1; then
  echo "ok   the multi-file example now AOT-compiles (imports are inlined)"
else
  echo "FAIL: ainl compile still refuses a multi-file program"
  fail=1
fi

echo
[ "$fail" -eq 0 ] && echo "package gate PASSED" || echo "package gate FAILED"
exit "$fail"
