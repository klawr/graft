#!/usr/bin/env sh
# graft installer — downloads a prebuilt release binary from GitHub.
#
#   # latest release:
#   curl -fsSL https://raw.githubusercontent.com/OWNER/REPO/main/install.sh | sh
#
#   # a specific version / custom location:
#   GRAFT_VERSION=v0.1.0 GRAFT_BIN_DIR=/usr/local/bin ./install.sh
#
# Override the repo without editing this file via GRAFT_REPO=owner/name.
set -eu

# ─── set this to your GitHub repo (owner/name) ──────────────────────────────────
REPO="${GRAFT_REPO:-OWNER/REPO}"
# ────────────────────────────────────────────────────────────────────────────────

VERSION="${GRAFT_VERSION:-latest}"
BIN_DIR="${GRAFT_BIN_DIR:-$HOME/.local/bin}"

err() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

[ "$REPO" = "OWNER/REPO" ] && err "set REPO at the top of this script (or GRAFT_REPO=owner/name)"

os="$(uname -s)"
arch="$(uname -m)"
[ "$os" = "Linux" ] || err "graft currently ships a Linux binary only (got $os)"
case "$arch" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) err "no prebuilt binary for $arch — build from source: cargo install --git https://github.com/$REPO" ;;
esac

asset="graft-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download graft"
fi

mkdir -p "$BIN_DIR"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
info "downloading $asset ($VERSION)"
fetch "$url" "$tmp/graft.tar.gz" || err "download failed: $url"
tar -xzf "$tmp/graft.tar.gz" -C "$tmp" || err "could not extract $asset"
install -m755 "$tmp/graft" "$BIN_DIR/graft" 2>/dev/null || { mv "$tmp/graft" "$BIN_DIR/graft"; chmod +x "$BIN_DIR/graft"; }
info "installed graft to $BIN_DIR/graft"

# Warn about runtime dependencies graft shells out to.
missing=""
for tool in docker tmux patchelf ldd; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
[ -n "$missing" ] && warn "missing required tools:$missing (install them via your package manager)"
command -v oras >/dev/null 2>&1 || info "note: 'oras' is only needed if your devcontainers use features (https://oras.land)"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH — add it, e.g.  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

info "done — run 'graft up' in a project with a .devcontainer"
