#!/bin/sh
# ainl installer — one line, verified download, correct platform.
#
#   curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
#
# What it does, in order:
#   1. detect the platform and map it to a release asset
#   2. resolve the latest release (or honour AINL_VERSION)
#   3. download the asset *and* the release's SHA256SUMS
#   4. verify the checksum BEFORE unpacking anything
#   5. install to ~/.local/bin (or /usr/local/bin with sudo) and verify
#
# Written for POSIX sh, not bash: `sh` on macOS is bash-in-disguise (older),
# on Debian is dash, on Alpine is busybox ash. It uses only constructs that
# behave the same in all three. `set -eu`; every failure path exits non-zero
# with a message, never a half-installed binary.

set -eu

REPO="GRITui/ai-lang"
GITHUB_API="https://api.github.com"

# ---------------------------------------------------------------- reporting

# Colour, but only when a human is watching. Every variable is initialised
# unconditionally: under `set -u`, assigning them only inside a branch leaves
# them *unbound* on the other path, and the first printf then aborts the whole
# installer. Piped or non-tty output correctly gets the plain-text form.
BOLD=''; DIM=''; RED=''; GREEN=''; RESET=''
if [ -t 2 ] && [ -n "${TERM:-}" ] && [ "${TERM:-}" != "dumb" ]; then
  BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
  RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m'); RESET=$(printf '\033[0m')
fi

info() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*" >&2; }
note() { printf '    %s%s%s\n' "$DIM" "$*" "$RESET" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }
ok()   { printf '    %sok%s %s\n' "$GREEN" "$RESET" "$*" >&2; }

have() { command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------- platform

# Map (os, arch) -> the release asset's target triple. A release only ships
# what CI proved it can build, so an unlisted platform is a clear error rather
# than a best-effort download of something that will not run.
detect_target() {
  _os=$(uname -s)
  _arch=$(uname -m)
  case "$_os" in
    Linux)  _os=linux ;;
    Darwin) _os=darwin ;;
    MINGW*|MSYS*|CYGWIN*) _os=windows ;;
    *) die "unsupported OS '$_os' (this installer supports Linux and macOS)" ;;
  esac
  case "$_arch" in
    x86_64|amd64) _arch=x86_64 ;;
    arm64|aarch64) _arch=aarch64 ;;
    *) die "unsupported architecture '$_arch' (this installer supports x86_64 and arm64)" ;;
  esac
  # The asset naming convention is the Rust target triple with os/arch swapped
  # to the release labels: aarch64-apple-darwin, x86_64-unknown-linux-musl.
  case "$_os" in
    darwin) printf 'aarch64-apple-darwin\n' ;;
    linux)  printf 'x86_64-unknown-linux-musl\n' ;;
    windows) printf '%s-pc-windows-msvc\n' "$_arch" ;;
  esac
}

# ---------------------------------------------------------------- download

# curl and wget are both ubiquitous, but neither is guaranteed. The function
# picks whichever exists and reports clearly when neither does, instead of
# failing later with a confusing "command not found".
fetch() {
  # $1 = url, $2 = destination
  _url=$1; _dest=$2
  if have curl; then
    # -fL: fail on HTTP errors, follow redirects (release assets 302 to a CDN),
    # -sS: silent but keep the error message, --retry for flaky networks.
    curl -fsSL --retry 3 --retry-delay 1 -o "$_dest" "$_url"
  elif have wget; then
    wget -q -O "$_dest" "$_url"
  else
    die "neither curl nor wget is installed; install one, or download the release manually:
  https://github.com/$REPO/releases"
  fi
}

fetch_stdout() {
  # Fetch to stdout (used for the small metadata files).
  if have curl; then
    curl -fsSL --retry 3 --retry-delay 1 "$1"
  else
    wget -q -O - "$1"
  fi
}

# ---------------------------------------------------------------- checksum

# Verify a file against a SHA256SUMS line. Uses the system tools rather than
# assuming a GNU coreutils: `sha256sum` on Linux, `shasum -a 256` on macOS.
sha256_of() {
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif have openssl; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  else
    die "no SHA256 tool found (need sha256sum, shasum, or openssl) — refusing to install an unverified binary"
  fi
}

verify_checksum() {
  # $1 = file, $2 = SHA256SUMS file, $3 = expected basename
  _file=$1; _sums=$2; _name=$3
  _actual=$(sha256_of "$_file")
  # Match on the basename only: SHA256SUMS lines are "<hash>  <name>" and the
  # name in the file has no directory component, while $_file has a temp path.
  _expected=$(awk -v n="$_name" '$2 == n || $2 == "*"n {print $1; exit}' "$_sums")
  [ -n "$_expected" ] || die "SHA256SUMS has no entry for $_name — refusing to install"
  if [ "$_actual" != "$_expected" ]; then
    die "checksum mismatch for $_name
  expected $_expected
  actual   $_actual
The download is corrupt or the release was tampered with. Nothing was installed."
  fi
}

# ---------------------------------------------------------------- version

# Resolve which release to install. AINL_VERSION pins one (a tag, with or
# without the leading v); otherwise take the newest non-prerelease release.
resolve_version() {
  if [ -n "${AINL_VERSION:-}" ]; then
    case "$AINL_VERSION" in
      v*) printf '%s\n' "$AINL_VERSION" ;;
      *)  printf 'v%s\n' "$AINL_VERSION" ;;
    esac
    return
  fi
  _json=$(fetch_stdout "$GITHUB_API/repos/$REPO/releases/latest" 2>/dev/null) \
    || die "could not reach the GitHub API to find the latest release.
  Check your network, or pin a version:
    AINL_VERSION=0.3.0 sh <(curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh)
  Or download manually: https://github.com/$REPO/releases"
  # Tag name is the first "tag_name": "..." in the payload. Parsed with sed
  # rather than jq so the installer needs no JSON tooling.
  _tag=$(printf '%s' "$_json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$_tag" ] || die "could not parse the latest release tag from the GitHub API response"
  printf '%s\n' "$_tag"
}

# ---------------------------------------------------------------- main

main() {
  TARGET=$(detect_target)
  VERSION=$(resolve_version)
  ASSET="ainl-${VERSION}-${TARGET}.tar.gz"

  info "installing ainl ${VERSION} (${TARGET})"

  TMP=$(mktemp -d 2>/dev/null || mktemp -d -t ainl)
  # shellcheck disable=SC2064  # the trap must capture TMP's value now, not at exit
  trap "rm -rf '$TMP'" EXIT INT TERM

  BASE="${AINL_RELEASE_BASE:-https://github.com/$REPO/releases/download}"

  # Normalise a base to end in '/', so both the default (…/download/v1) and an
  # override (file:///tmp/releases) join the same way.
  case "$BASE" in
    */) ;;
    *) BASE="$BASE/" ;;
  esac
  BASE="${BASE}${VERSION}"

  note "downloading $ASSET"
  fetch "$BASE/$ASSET" "$TMP/$ASSET" \
    || die "could not download $ASSET from $BASE
  If this version has no asset for $TARGET, see: https://github.com/$REPO/releases"

  # The checksum file is the point of the whole exercise: an unverified binary
  # from a pipe is exactly what this installer exists to prevent.
  note "fetching SHA256SUMS"
  if ! fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
    die "SHA256SUMS not found for $VERSION — refusing to install unverified.
  The release may predate checksum publishing; download manually from
    https://github.com/$REPO/releases"
  fi

  info "verifying SHA256"
  verify_checksum "$TMP/$ASSET" "$TMP/SHA256SUMS" "$ASSET"
  ok "checksum matches"

  info "installing"
  # Unpack into TMP first, so a corrupt or unexpected tarball cannot scatter
  # files before the checksum has been checked. The tarball contains a single
  # top-level directory named after the release.
  ( cd "$TMP" && tar xzf "$ASSET" ) || die "could not unpack $ASSET"

  SRC=$(find "$TMP" -type f -name ainl -perm -u+x | head -1)
  [ -n "$SRC" ] || die "no executable 'ainl' inside $ASSET — the release is malformed"

  # Install target: a user-local dir by default (no root needed), or a system
  # dir on request. AINL_BIN_DIR overrides both, which is what the test in
  # scripts/check-install.sh uses to install into a throwaway location.
  BIN_DIR="${AINL_BIN_DIR:-}"
  SUDO=""
  if [ -z "$BIN_DIR" ]; then
    if [ "$(id -u)" = "0" ]; then
      BIN_DIR=/usr/local/bin
    elif [ -w "$HOME/.local/bin" ] || [ ! -d "$HOME/.local/bin" ]; then
      BIN_DIR="$HOME/.local/bin"
    else
      BIN_DIR=/usr/local/bin
      SUDO="sudo"
    fi
  fi

  mkdir -p "$BIN_DIR" 2>/dev/null || {
    [ -n "$SUDO" ] || die "cannot create $BIN_DIR"
    $SUDO mkdir -p "$BIN_DIR"
  }

  # Install atomically-ish: write to a temp name in the same directory, then
  # rename. rename(2) within a filesystem is atomic, so a concurrent `ainl`
  # invocation sees either the old binary or the new one, never a truncated
  # file. cp-directly-to-the-target would expose exactly that window.
  TMP_BIN="$BIN_DIR/.ainl-install-$$"
  if cp "$SRC" "$TMP_BIN" 2>/dev/null; then
    chmod 0755 "$TMP_BIN"
  else
    $SUDO cp "$SRC" "$TMP_BIN" && $SUDO chmod 0755 "$TMP_BIN"
  fi
  mv "$TMP_BIN" "$BIN_DIR/ainl" 2>/dev/null || $SUDO mv "$TMP_BIN" "$BIN_DIR/ainl"
  ok "installed $BIN_DIR/ainl"

  # Run it. This is the last line of defence: a binary that does not answer
  # `--version` is not an install, and the user should know now rather than at
  # their first script.
  if "$BIN_DIR/ainl" --version >/dev/null 2>&1; then
    ok "$("$BIN_DIR/ainl" --version)"
  else
    die "installed, but '$BIN_DIR/ainl --version' failed — the binary does not run on this machine.
  Remove it with: rm -f $BIN_DIR/ainl"
  fi

  # PATH advice, only when it is actually needed. `command -v ainl` reflects
  # the current shell, which is the question the user is asking.
  case ":${PATH:-}:" in
    *":$BIN_DIR:"*) ;;
    *) note ""; note "add it to your PATH:"; note "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
  esac
  if "$BIN_DIR/ainl" doctor >/dev/null 2>&1; then
    ok "self-test passed — run 'ainl doctor' for the full report"
  else
    note "note: 'ainl doctor' reported a problem; run it for detail"
  fi
}

main "$@"
