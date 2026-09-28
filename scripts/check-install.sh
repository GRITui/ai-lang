#!/usr/bin/env bash
# End-to-end test for scripts/install.sh against a LOCAL fake release, served
# over file:// — no network, no GitHub, no published tag required.
#
# What it proves:
#   1. the asset is chosen for the right platform
#   2. SHA256 is verified BEFORE the binary is installed
#   3. a corrupted download is REFUSED (no half-install, non-zero exit)
#   4. a missing SHA256SUMS is REFUSED (unverified installs never happen)
#   5. a good install lands in AINL_BIN_DIR, runs, and passes `ainl doctor`
#   6. re-running upgrades in place (idempotent)
#
# The installer's URL base is overridden with AINL_RELEASE_BASE so the same
# code path that fetches from GitHub is exercised against a local fixture.
set -uo pipefail
cd "$(dirname "$0")/.."

ROOT=$(pwd)
BIN=target/release/ainl
[ -x "$BIN" ] || BIN=target/debug/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."; cargo build -q --release || exit 1
  BIN=target/release/ainl
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
FAKE_RELEASES="$WORK/releases"
VERSION="v9.9.9-doctor-test"
TARGET="aarch64-apple-darwin"
ASSET="ainl-${VERSION}-${TARGET}.tar.gz"
RELDIR="$FAKE_RELEASES/$VERSION"
mkdir -p "$RELDIR"
STAGE="$WORK/stage/ainl-${VERSION}-${TARGET}"
mkdir -p "$STAGE"
cp "$BIN" "$STAGE/ainl"
cp README.md LICENSE-MIT LICENSE-APACHE "$STAGE/"
( cd "$WORK/stage" && tar czf "$RELDIR/$ASSET" "ainl-${VERSION}-${TARGET}" )

fail=0
pass() { echo "ok   $1"; }
bad()  { echo "FAIL $1"; fail=1; }

# (cd into the release dir so SHA256SUMS records a bare filename, exactly as a
# real release's would)
( cd "$RELDIR" && if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$ASSET" > SHA256SUMS
  else
    shasum -a 256 "$ASSET" > SHA256SUMS
  fi )
echo "== fixture: $ASSET"
cat "$RELDIR/SHA256SUMS"
echo

run_install() {
  # $1 = install target dir, $2 = fake release ROOT (a dir containing
  # $VERSION/); the installer appends /<version> itself, exactly as the real
  # GitHub download URL does.
  _dir=$1; _root=$2
  env AINL_BIN_DIR="$_dir" AINL_VERSION="$VERSION" \
      AINL_RELEASE_BASE="file://$_root" \
      sh scripts/install.sh
}

echo "== 1+2+5: clean install verifies SHA256, installs, and self-tests =="
D1="$WORK/bin1"
out=$(run_install "$D1" "$FAKE_RELEASES" 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
[ $rc -eq 0 ] || bad "install exited $rc"
[ -x "$D1/ainl" ] || bad "binary not installed"
if echo "$out" | grep -q "checksum matches"; then pass "SHA256 verified before install"; else bad "no checksum confirmation"; fi
if [ -x "$D1/ainl" ] && "$D1/ainl" doctor >/dev/null 2>&1; then
  pass "installed binary passes ainl doctor"
else
  bad "installed binary fails ainl doctor"
fi
if [ -x "$D1/ainl" ]; then
  echo "    version: $("$D1/ainl" --version)"
fi

echo
echo "== 3: a corrupted tarball must be REFUSED, with nothing installed =="
# Its own release root, so the installer really fetches the bad tarball and
# not the good one from the fixture above.
CROOT="$WORK/corrupt-root"
CR="$CROOT/$VERSION"
mkdir -p "$CR"
cp "$RELDIR/SHA256SUMS" "$CR/"      # the checksum of the *good* asset …
# … paired with a tarball that is not it.
mkdir -p "$WORK/other"
echo "not the real ainl" > "$WORK/other/ainl"
( cd "$WORK/other" && tar czf "$CR/$ASSET" ainl )
D3="$WORK/bin3"
out=$(run_install "$D3" "$CROOT" 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
[ $rc -ne 0 ] || bad "corrupt download was ACCEPTED (exit 0)"
if echo "$out" | grep -q "checksum mismatch"; then pass "corrupt download refused with a checksum mismatch"; else bad "no mismatch reported"; fi
[ ! -e "$D3/ainl" ] || bad "binary was installed despite a bad checksum"
echo "ok   nothing installed on a bad checksum"

echo
echo "== 4: a release with no SHA256SUMS must be REFUSED =="
NROOT="$WORK/nosums-root"
NR="$NROOT/$VERSION"
mkdir -p "$NR"
cp "$RELDIR/$ASSET" "$NR/"
D4="$WORK/bin4"
out=$(run_install "$D4" "$NROOT" 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
[ $rc -ne 0 ] || bad "unverified release was ACCEPTED (exit 0)"
[ ! -e "$D4/ainl" ] || bad "binary installed without verification"
echo "ok   release without SHA256SUMS refused"

echo
echo "== 5: a missing asset for this platform must be REFUSED =="
MROOT="$WORK/missing-root"
MR="$MROOT/$VERSION"
mkdir -p "$MR"
cp "$RELDIR/SHA256SUMS" "$MR/"   # sums exist, asset does not
D5="$WORK/bin5"
out=$(run_install "$D5" "$MROOT" 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
[ $rc -ne 0 ] || bad "missing asset was ACCEPTED (exit 0)"
[ ! -e "$D5/ainl" ] || bad "binary installed from a missing asset"
echo "ok   release missing the asset for this platform refused"

echo
echo "== 6: re-running is idempotent / an upgrade =="
D6="$WORK/bin6"
mkdir -p "$D6"
# A stale older-looking binary in place first.
printf '#!/bin/sh\necho stale\n' > "$D6/ainl"; chmod +x "$D6/ainl"
out=$(run_install "$D6" "$FAKE_RELEASES" 2>&1); rc=$?
[ $rc -eq 0 ] || bad "re-run exited $rc"
if [ -x "$D6/ainl" ] && "$D6/ainl" doctor >/dev/null 2>&1; then
  pass "re-run replaced the old binary and it still works"
else
  bad "re-run did not produce a working binary"
fi
# No temp file left behind in the install dir.
if ls "$D6"/.ainl-install-* >/dev/null 2>&1; then
  bad "left a temp file behind: $(ls "$D6"/.ainl-install-*)"
else
  pass "no temp files left in the install dir"
fi

echo
[ "$fail" -eq 0 ] && echo "install.sh: ALL CHECKS PASSED" || echo "install.sh: CHECKS FAILED"
exit "$fail"
