#!/usr/bin/env bash
# Assert that the repo's copy of the platform→target mapping
# (scripts/lib-target.sh) agrees with install.sh's own detect_target.
#
# install.sh cannot source that file — it is piped to `sh` and must be
# self-contained — so the mapping exists twice. This is the test that keeps the
# two copies from drifting, and it is what caught the real bug this file
# exists for: check-install.sh hardcoded the macOS asset name, so on an ubuntu
# runner the installer correctly asked for x86_64-unknown-linux-musl and the
# fixture had nothing to serve.
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
pass() { echo "ok   $1"; }
bad()  { echo "FAIL $1"; fail=1; }

# The canonical copy, from the repo.
. scripts/lib-target.sh
LIB_TARGET=$(detect_target) || { echo "FAIL: lib-target.sh could not detect a target"; exit 1; }

# install.sh's copy, on the same host. The trailing `main "$@"` invocation is
# stripped before sourcing, so sourcing has no side effects — install.sh must
# stay runnable as `… | sh` with no arguments, which is the whole point.
SCRIPT_TARGET=$(sed 's/^main "\$@"$//' scripts/install.sh | sh -c '
  . /dev/stdin
  detect_target
') || { echo "FAIL: could not evaluate install.sh's detect_target"; exit 1; }

echo "    lib-target.sh : $LIB_TARGET"
echo "    install.sh    : $SCRIPT_TARGET"
if [ "$LIB_TARGET" = "$SCRIPT_TARGET" ]; then
  pass "both platform mappings agree on this host ($LIB_TARGET)"
else
  bad "platform mapping DRIFT: lib=$LIB_TARGET install=$SCRIPT_TARGET"
fi

# The asset name the installer would build must match the name the test
# fixtures build, or those fixtures serve the wrong file.
ASSET="ainl-v9.9.9-doctor-test-${LIB_TARGET}.tar.gz"
echo "    derived asset : $ASSET"

# And the real release workflow must name the same two targets the installer
# can ask for. A release that builds an asset the installer cannot request is
# an uninstallable release.
REL_YNML=".github/workflows/release.yml"
for t in aarch64-apple-darwin x86_64-unknown-linux-musl; do
  if grep -q "$t" "$REL_YNML"; then
    pass "release.yml builds $t"
  else
    bad "release.yml does not build $t"
  fi
done

echo
[ "$fail" -eq 0 ] && echo "target mapping: IN SYNC" || echo "target mapping: OUT OF SYNC"
exit "$fail"
