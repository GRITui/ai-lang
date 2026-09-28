#!/usr/bin/env bash
# The acceptance test for the card: run scripts/install.sh in a CLEAN
# environment (fresh HOME, fresh PATH) against a real release, then verify
# `ainl --version` and `ainl doctor` pass from the installed binary.
#
# "Clean" means more than a temp dir: a fresh HOME (so ~/.local/bin is not
# pre-populated or on PATH from the developer's shell), a PATH containing
# only the few tools the installer needs, and no inherited AINL_* variables.
# Anything less and the test would pass using state the real user does not
# have.
#
# The release is a local fixture served over file:// (see check-install.sh
# for the same technique), because the newest published tag predates
# SHA256SUMS and therefore cannot be installed by an installer that refuses
# to install unverified binaries. That is the installer working as designed,
# not a gap in the test.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."; cargo build -q --release || exit 1
  BIN=target/release/ainl
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
VERSION="v9.9.9-acceptance"
TARGET="aarch64-apple-darwin"
ASSET="ainl-${VERSION}-${TARGET}.tar.gz"
REL="$WORK/rel/$VERSION"

fail=0
pass() { echo "ok   $1"; }
bad()  { echo "FAIL $1"; fail=1; }

# ---- build a real release fixture -------------------------------------
mkdir -p "$REL"
STAGE="$WORK/stage/ainl-${VERSION}-${TARGET}"
mkdir -p "$STAGE"
cp "$BIN" "$STAGE/ainl"
cp README.md LICENSE-MIT LICENSE-APACHE "$STAGE/"
( cd "$WORK/stage" && tar czf "$REL/$ASSET" "ainl-${VERSION}-${TARGET}" )
( cd "$REL" && sha256sum "$ASSET" > SHA256SUMS )
echo "== fixture release $VERSION =="
cat "$REL/SHA256SUMS" | sed 's/^/    /'
echo

# ---- a genuinely clean environment ------------------------------------
CLEAN="$WORK/clean"
mkdir -p "$CLEAN/home" "$CLEAN/bin"
# Only the tools install.sh itself needs, plus the two the *user* is assumed
# to have: a downloader (curl or wget) and a SHA256 tool. `cc` is
# deliberately absent — the AOT check must SKIP and the install must still
# succeed. The list is derived from what install.sh actually invokes;
# mktemp, find and head are load-bearing and easy to forget, which is exactly
# the class of thing a minimal environment breaks.
for t in sh tar awk sed uname id mkdir cp mv chmod rm dirname cat find head \
         mktemp grep cut ls printf curl sha256sum; do
  p=$(command -v "$t" 2>/dev/null) && ln -sf "$p" "$CLEAN/bin/$t"
done
echo "== clean env =="
echo "    HOME=$CLEAN/home"
echo "    PATH=$CLEAN/bin (no cc: $(PATH="$CLEAN/bin" command -v cc || echo 'absent, as intended'))"
echo

echo "== running: env -i HOME=… PATH=… sh scripts/install.sh =="
# `env -i` is the point: nothing from the developer's environment leaks in.
set +e
env -i \
  HOME="$CLEAN/home" \
  PATH="$CLEAN/bin" \
  TMPDIR="$CLEAN" \
  AINL_VERSION="$VERSION" \
  AINL_RELEASE_BASE="file://$WORK/rel" \
  "$CLEAN/bin/sh" scripts/install.sh 2>&1 | sed 's/^/    /'
rc=${PIPESTATUS[0]}
set -e
echo "    installer exit: $rc"
echo

[ $rc -eq 0 ] || bad "installer exited $rc in a clean env"
INSTALLED_DIR="$CLEAN/home/.local/bin"
INSTALLED="$INSTALLED_DIR/ainl"
[ -x "$INSTALLED" ] || bad "no binary at $INSTALLED"
[ -x "$INSTALLED" ] && pass "installed to ~/.local/bin with no root and no sudo"

echo
echo "== the installed binary, run from the clean env =="
ver=$(env -i HOME="$CLEAN/home" PATH="$CLEAN/bin:$INSTALLED_DIR" "$INSTALLED" --version 2>&1)
echo "    \$ ainl --version"
echo "$ver" | sed 's/^/    /'
if echo "$ver" | grep -q "ainl 0.2.0"; then pass "ainl --version reports the version"; else bad "unexpected --version output"; fi
if echo "$ver" | grep -q "aarch64-apple-darwin"; then pass "reports the build target"; else bad "no target in --version"; fi
if echo "$ver" | grep -qE '\([0-9a-f]{12}\)'; then pass "reports the source commit"; else bad "no commit in --version"; fi

echo
echo "    \$ ainl doctor"
env -i HOME="$CLEAN/home" PATH="$CLEAN/bin:$INSTALLED_DIR" "$INSTALLED" doctor 2>&1 | sed 's/^/    /'
drc=${PIPESTATUS[0]}
[ $drc -eq 0 ] || bad "ainl doctor exited $drc"
[ $drc -eq 0 ] && pass "ainl doctor exits 0 in the clean env"

echo
echo "    \$ ainl eval '(* 6 7)'"
ev=$(env -i HOME="$CLEAN/home" PATH="$CLEAN/bin:$INSTALLED_DIR" "$INSTALLED" eval '(* 6 7)' 2>&1)
echo "    $ev"
[ "$ev" = "42" ] && pass "ainl eval works from the installed binary" || bad "eval returned '$ev'"

echo
[ "$fail" -eq 0 ] && echo "CLEAN-ENV INSTALL: ALL CHECKS PASSED" || echo "CLEAN-ENV INSTALL: CHECKS FAILED"
exit "$fail"
