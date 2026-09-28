#!/usr/bin/env bash
# Verify the release job's SHA256SUMS step, run exactly as CI runs it.
#
# The step is the one piece of the pipeline that cannot be exercised on a dev
# machine, so it is reproduced here against a fake `dist/` tree shaped the way
# actions/download-artifact actually produces it (one subdirectory per
# artifact, which is the detail the `find -exec cp` flattening exists for).
#
# Checks:
#   1. nested artifacts are flattened, and the sums name bare basenames
#   2. `sha256sum -c` passes on the generated file
#   3. the installer accepts the resulting release (end-to-end, over file://)
#   4. a missing asset is caught by the asset/sum count assertion
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

BIN=target/release/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."; cargo build -q --release || exit 1
fi
if [ ! -x "$BIN" ]; then
  echo "FAIL: no ainl binary at $BIN"; exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
VERSION="v9.9.9-sums-test"
A_LIN="ainl-${VERSION}-x86_64-unknown-linux-musl.tar.gz"
A_MAC="ainl-${VERSION}-aarch64-apple-darwin.tar.gz"

fail=0
pass() { echo "ok   $1"; }
bad()  { echo "FAIL $1"; fail=1; }

# Mirror what the two build jobs upload: dist/<artifact-name>/<file>
DIST="$WORK/dist"
for pair in "ainl-$VERSION-x86_64-unknown-linux-musl:$A_LIN" \
            "ainl-$VERSION-aarch64-apple-darwin:$A_MAC"; do
  art=${pair%%:*}; file=${pair##*:}
  mkdir -p "$DIST/$art"
  echo "payload for $file" > "$DIST/$art/$file"
done
echo "== fixture dist tree (as download-artifact lays it out) =="
find "$DIST" -type f | sed "s|$WORK|…|"

# The CI step, verbatim. It runs in the checkout root (where actions/
# download-artifact left `dist/`), so this reproduces that: `dist` is staged
# into a scratch checkout and the step is executed there.
echo
echo "== the CI step, verbatim (run in a scratch checkout with dist/ present) =="
CKN="$WORK/checkout"
mkdir -p "$CKN"
# Copied, not symlinked: `find` does not descend into a symlinked directory, so
# a symlinked dist/ would silently yield zero files and the step would "pass"
# while summing nothing. Copying is what download-artifact effectively does.
cp -R "$DIST" "$CKN/dist"
(
  cd "$CKN"
  set -euo pipefail
  mkdir -p sums
  find dist -name '*.tar.gz' -exec cp {} sums/ \;
  cd sums
  ls -l
  sha256sum *.tar.gz > SHA256SUMS
  echo "== SHA256SUMS =="
  cat SHA256SUMS
  n_tgz=$(ls *.tar.gz | wc -l)
  n_sums=$(wc -l < SHA256SUMS)
  echo "assets=$n_tgz sums=$n_sums"
  [ "$n_tgz" = "$n_sums" ]
  sha256sum -c SHA256SUMS
)
rc=$?
if [ $rc -eq 0 ]; then pass "the CI step runs clean and sha256sum -c verifies"; else bad "the CI step failed (exit $rc)"; fi

SUMS="$CKN/sums/SHA256SUMS"
if [ -f "$SUMS" ]; then
  # 1. names must be bare, never dist/<artifact>/<file>
  if grep -q 'dist/' "$SUMS"; then bad "SHA256SUMS contains a nested path"; else pass "SHA256SUMS names bare basenames only"; fi
  n=$(grep -c . "$SUMS")
  if [ "$n" -eq 2 ]; then pass "both assets are covered ($n entries)"; else bad "expected 2 entries, got $n"; fi
else
  bad "no SHA256SUMS produced"
fi

echo
echo "== 3: the installer accepts a release laid out this way =="
# A real release root: <root>/<version>/{<asset>, SHA256SUMS}. The assets are
# real tarballs containing a real ainl, so the installer's unpack + smoke test
# both get exercised — a text payload would only prove the checksum matched.
REL="$WORK/rel/$VERSION"
mkdir -p "$REL"
for pair in "ainl-$VERSION-x86_64-unknown-linux-musl:$A_LIN" \
            "ainl-$VERSION-aarch64-apple-darwin:$A_MAC"; do
  art=${pair%%:*}; file=${pair##*:}
  stage="$WORK/stage/$art/ainl-$VERSION-aarch64-apple-darwin"
  rm -rf "$WORK/stage/$art"
  mkdir -p "$stage"
  cp "$ROOT/target/release/ainl" "$stage/ainl"
  cp README.md LICENSE-MIT "$stage/"
  ( cd "$WORK/stage/$art" && tar czf "$REL/$file" "ainl-$VERSION-aarch64-apple-darwin" )
done
# Regenerate the sums over the *real* assets, exactly as the release job does.
( cd "$REL" && sha256sum "$A_LIN" "$A_MAC" > SHA256SUMS )
out=$(env AINL_BIN_DIR="$WORK/inst" AINL_VERSION="$VERSION" \
        AINL_RELEASE_BASE="file://$WORK/rel" \
        sh scripts/install.sh 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
if [ $rc -ne 0 ]; then
  bad "installer rejected the CI-produced release (exit $rc)"
else
  pass "installer accepts the CI-produced release"
fi
if [ -x "$WORK/inst/ainl" ] && "$WORK/inst/ainl" doctor >/dev/null 2>&1; then
  pass "the installed binary passes ainl doctor"
  echo "    version: $("$WORK/inst/ainl" --version)"
else
  bad "installed binary does not pass ainl doctor"
fi

echo
echo "== 4: a missing asset is caught by the count assertion =="
# Drop one asset: the sums then describe 2 files while only 1 is present.
HALF="$WORK/half"
mkdir -p "$HALF"
cp "$WORK/dist/ainl-$VERSION-x86_64-unknown-linux-musl/$A_LIN" "$HALF/"
( cd "$HALF" && sha256sum "$A_LIN" > SHA256SUMS.only1 )
# The CI assertion compares asset count to sum-line count, so a release with
# fewer assets than the sum file would be caught. Reconstruct that case:
mkdir -p "$WORK/half2"
cp "$SUMS" "$WORK/half2/SHA256SUMS"     # sums for BOTH
cp "$HALF/$A_LIN" "$WORK/half2/"         # but only ONE asset present
(
  cd "$WORK/half2"
  n_tgz=$(ls *.tar.gz | wc -l)
  n_sums=$(wc -l < SHA256SUMS)
  echo "assets=$n_tgz sums=$n_sums"
  if [ "$n_tgz" = "$n_sums" ]; then echo "MATCHED (unexpected)"; exit 1; else echo "MISMATCH DETECTED (expected)"; fi
)
rc=$?
if [ $rc -eq 0 ]; then pass "the count assertion catches a missing asset"; else bad "count assertion did not catch the mismatch"; fi

echo
[ "$fail" -eq 0 ] && echo "SHA256SUMS: ALL CHECKS PASSED" || echo "SHA256SUMS: CHECKS FAILED"
exit "$fail"
