#!/bin/sh
# Install mnem from a GitHub release: no Rust, no build.
#
#   curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | sh
#   gh release download -R daefery/mnem -p install.sh -O - | sh    # while the repo is private
#
# Downloads the binary for this system, checks its SHA-256 against the release's
# checksums, puts it in ~/.local/bin (MNEM_BIN_DIR to change), then runs
# `mnem install --watch`: connects Claude Code, Codex and pi, starts the background
# service, and sets up distillation through Claude Code or Codex if nothing is set.
# MNEM_VERSION=v0.2.0 picks a release; MNEM_NO_SETUP=1 only installs the binary.
set -eu

REPO="${MNEM_REPO:-daefery/mnem}"
VERSION="${MNEM_VERSION:-latest}"
BIN_DIR="${MNEM_BIN_DIR:-$HOME/.local/bin}"

say() { printf 'mnem: %s\n' "$*"; }
fail() { printf 'mnem: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64)
    # Rosetta reports x86_64 for a shell running translated on Apple Silicon.
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      target=aarch64-apple-darwin
    else
      target=x86_64-apple-darwin
    fi ;;
  *) fail "no prebuilt binary for $os $arch (Linux and macOS on x86_64 or arm64; Windows: use WSL)" ;;
esac

# Linux: the full build needs glibc 2.39+ (its ONNX Runtime does); glibc 2.35 to 2.38
# gets the lite build (no ONNX Runtime; meaning search with the small potion model).
flavour=""
if [ "$os" = Linux ]; then
  if ldd --version 2>&1 | head -1 | grep -qi musl; then
    fail "this Linux uses musl (Alpine?); the binaries need glibc 2.35 or newer"
  fi
  glibc=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)
  [ -n "$glibc" ] || fail "could not tell the glibc version (ldd --version); the binaries need 2.35 or newer"
  major=${glibc%.*}; minor=${glibc#*.}
  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
    fail "glibc $glibc is older than 2.35 (Ubuntu 22.04, Debian 12 or newer needed)"
  fi
  if [ "$major" -eq 2 ] && [ "$minor" -lt 39 ]; then
    flavour=-lite
    say "glibc $glibc: installing the lite build (meaning search with a smaller model; glibc 2.39+ gets the full one)"
  fi
fi

asset="mnem-$target$flavour.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# A public repo downloads with curl; a private one through the GitHub CLI's login.
fetch() {
  name=$1
  if [ "$VERSION" = latest ]; then
    url="https://github.com/$REPO/releases/latest/download/$name"
  else
    url="https://github.com/$REPO/releases/download/$VERSION/$name"
  fi
  if curl -fsSL -o "$tmp/$name" "$url" 2>/dev/null; then
    return 0
  fi
  if command -v gh >/dev/null 2>&1; then
    tag=""
    [ "$VERSION" = latest ] || tag=$VERSION
    # shellcheck disable=SC2086
    gh release download $tag -R "$REPO" -p "$name" -D "$tmp" --clobber >/dev/null 2>&1 && return 0
  fi
  return 1
}

say "downloading $asset ($VERSION)"
fetch "$asset" || fail "could not download $asset from $REPO. If the repository is private, install the GitHub CLI and sign in (gh auth login) first."
fetch checksums.txt || fail "could not download checksums.txt from $REPO"

want=$(grep " $asset\$" "$tmp/checksums.txt" | cut -d' ' -f1)
[ -n "$want" ] || fail "$asset is not listed in checksums.txt"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
[ "$got" = "$want" ] || fail "checksum mismatch for $asset (expected $want, got $got); not installed"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR"
# Replace atomically: a running watcher keeps its old file until it restarts.
cp "$tmp/mnem-$target$flavour/mnem" "$BIN_DIR/mnem.new"
chmod 755 "$BIN_DIR/mnem.new"
# macOS marks downloaded files; a binary fetched by curl is not, but one fetched by a
# browser or some tools is, and Gatekeeper would then block an unsigned binary.
if [ "$os" = Darwin ]; then
  xattr -d com.apple.quarantine "$BIN_DIR/mnem.new" 2>/dev/null || true
fi
mv -f "$BIN_DIR/mnem.new" "$BIN_DIR/mnem"
say "installed $("$BIN_DIR/mnem" --version) to $BIN_DIR/mnem"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "add $BIN_DIR to your PATH (for example in ~/.bashrc or ~/.zshrc): export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if [ "${MNEM_NO_SETUP:-0}" = 1 ]; then
  say "skipped setup (MNEM_NO_SETUP=1); run: mnem install --watch"
  exit 0
fi
"$BIN_DIR/mnem" install --watch
say "done. Check with: mnem doctor (it ends with \"status: OK\" once the first sessions are read)"
