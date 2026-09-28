#!/usr/bin/env bash
# Reproduce the ubuntu-runner path of the packaging tests on a macOS host, by
# putting a fake `uname` (and the absence of `cc`) on PATH.
#
# The first CI run of this work failed on ubuntu-latest while passing on macOS,
# because the fixtures hardcoded the macOS asset name. A test that only ever
# runs on the developer's platform will keep finding that class of bug late.
# This runs the packaging suite twice — once as macOS, once as Linux — so the
# host-dependent branch is covered on every machine.
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
pass() { echo "ok   $1"; }
bad()  { echo "FAIL $1"; fail=1; }

FAKEBIN=$(mktemp -d)
cleanup() { rm -rf "$FAKEBIN"; }
trap cleanup EXIT

# A `uname` that reports Linux/x86_64 regardless of the real host. Everything
# else is passed through, so the tests still exercise real tar/sha/curl.
cat > "$FAKEBIN/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) echo "Linux" ;;
  -m) echo "x86_64" ;;
  -a) echo "Linux GRITs-Mac-mini 6.0.0 x86_64 GNU/Linux" ;;
  *)  exec /usr/bin/uname "$@" ;;
esac
EOF
chmod +x "$FAKEBIN/uname"

# Prove the faking works before relying on it.
fake=$(PATH="$FAKEBIN:$PATH" uname -s)
if [ "$fake" = "Linux" ]; then
  pass "fake uname reports Linux"
else
  bad "fake uname reports '$fake', expected Linux"
  exit 1
fi

echo
echo "== the installer must ask for the LINUX asset under a Linux uname =="
got=$(PATH="$FAKEBIN:$PATH" sh -c '. scripts/lib-target.sh; detect_target')
if [ "$got" = "x86_64-unknown-linux-musl" ]; then
  pass "lib-target.sh maps Linux/x86_64 -> x86_64-unknown-linux-musl"
else
  bad "lib-target.sh gave '$got'"
fi
got=$(sed 's/^main "\$@"$//' scripts/install.sh | PATH="$FAKEBIN:$PATH" sh -c '. /dev/stdin; detect_target')
if [ "$got" = "x86_64-unknown-linux-musl" ]; then
  pass "install.sh maps Linux/x86_64 -> x86_64-unknown-linux-musl"
else
  bad "install.sh gave '$got'"
fi

echo
echo "== the full packaging suite under a Linux uname =="
# The fake uname is prepended so the fixtures build a Linux-named asset; the
# rest of each script runs normally. This is the configuration that failed on
# the first CI run.
for s in check-target-sync.sh check-install.sh check-sums.sh \
         check-install-clean-env.sh; do
  echo
  echo "---- $s (as Linux) ----"
  if PATH="$FAKEBIN:$PATH" bash "scripts/$s" 2>&1 | tail -14; then
    pass "$s as Linux"
  else
    bad "$s as Linux"
  fi
done

echo
echo "== the same suite under the real (macOS) uname =="
for s in check-target-sync.sh check-install.sh check-sums.sh \
         check-install-clean-env.sh; do
  if bash "scripts/$s" >/dev/null 2>&1; then
    pass "$s as macOS"
  else
    bad "$s as macOS"
  fi
done

echo
[ "$fail" -eq 0 ] && echo "PLATFORM MATRIX: ALL PASSED" || echo "PLATFORM MATRIX: FAILURES"
exit "$fail"
