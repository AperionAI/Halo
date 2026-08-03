#!/bin/sh
# Smartflow Halo installer.
#
#   curl -fsSL https://get.halo.aperion.ai | sh
#
# Detects OS/arch, downloads the matching release tarball from GitHub, verifies
# its SHA-256, and drops the `halo` (and `halo-relay`) binaries into an install
# dir on PATH. POSIX sh, no dependencies beyond curl/tar/shasum.
set -eu

REPO="AperionAI/Halo"
BIN="halo"
# Override with HALO_INSTALL_DIR=... to change the destination.
INSTALL_DIR="${HALO_INSTALL_DIR:-}"
# Override with HALO_VERSION=halo-v1.2.0 to pin; default is the latest release.
VERSION="${HALO_VERSION:-latest}"

err() { echo "halo-install: $*" >&2; exit 1; }

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) err "unsupported OS '$os'. Windows users: download the .zip from https://github.com/$REPO/releases" ;;
  esac
  case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) err "unsupported architecture '$arch'" ;;
  esac
  echo "${arch_part}-${os_part}"
}

pick_install_dir() {
  if [ -n "$INSTALL_DIR" ]; then echo "$INSTALL_DIR"; return; fi
  # Prefer a writable dir already on PATH; fall back to ~/.local/bin.
  if [ -w /usr/local/bin ] 2>/dev/null; then echo /usr/local/bin; return; fi
  echo "$HOME/.local/bin"
}

main() {
  command -v curl >/dev/null 2>&1 || err "curl is required"
  command -v tar  >/dev/null 2>&1 || err "tar is required"

  target="$(detect_target)"
  asset="halo-${target}.tar.gz"

  if [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$VERSION"
  fi
  url="$base/$asset"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  echo "halo-install: downloading $asset ..."
  curl -fSL "$url" -o "$tmp/$asset" || err "download failed: $url"

  # Verify checksum when the sidecar file is published.
  if curl -fSL "$url.sha256" -o "$tmp/$asset.sha256" 2>/dev/null; then
    echo "halo-install: verifying checksum ..."
    ( cd "$tmp"
      expected="$(cut -d' ' -f1 < "$asset.sha256")"
      if command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$asset" | cut -d' ' -f1)"
      else
        actual="$(sha256sum "$asset" | cut -d' ' -f1)"
      fi
      [ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"
    )
  fi

  tar -xzf "$tmp/$asset" -C "$tmp"
  extracted="$tmp/halo-${target}"

  dir="$(pick_install_dir)"
  mkdir -p "$dir"
  install -m 0755 "$extracted/halo" "$dir/halo"
  [ -f "$extracted/halo-relay" ] && install -m 0755 "$extracted/halo-relay" "$dir/halo-relay"

  echo "halo-install: installed to $dir"
  case ":$PATH:" in
    *":$dir:"*) : ;;
    *) echo "halo-install: NOTE: $dir is not on your PATH. Add it, e.g.:"
       echo "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
  esac
  "$dir/$BIN" --version || true
  echo "halo-install: done. Next: 'halo agent add <name> --provider ...' then 'halo serve'."
}

main "$@"
