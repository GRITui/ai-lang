#!/bin/sh
# Map the host to a release asset target triple.
#
# install.sh carries its own copy of this logic because it is piped to `sh` and
# must stand alone — it cannot source a file from the repo it was downloaded
# from. To stop the two from silently drifting apart,
# scripts/check-target-sync.sh sources *this* file and compares its output
# against install.sh's own detect_target on the same host. Editing one without
# the other fails CI.

detect_target() {
  _os=$(uname -s)
  _arch=$(uname -m)
  case "$_os" in
    Linux)  _os=linux ;;
    Darwin) _os=darwin ;;
    MINGW*|MSYS*|CYGWIN*) _os=windows ;;
    *) echo "unsupported OS '$_os'" >&2; return 1 ;;
  esac
  case "$_arch" in
    x86_64|amd64) _arch=x86_64 ;;
    arm64|aarch64) _arch=aarch64 ;;
    *) echo "unsupported architecture '$_arch'" >&2; return 1 ;;
  esac
  case "$_os" in
    darwin) printf 'aarch64-apple-darwin\n' ;;
    linux)  printf 'x86_64-unknown-linux-musl\n' ;;
    windows) printf '%s-pc-windows-msvc\n' "$_arch" ;;
  esac
}
