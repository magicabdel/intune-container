#!/bin/sh
# intune-container installer — downloads the latest prebuilt binary from GitHub
# Releases and installs it to ~/.local/bin (override with DEST=...).
#
#   curl -fsSL https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
#   wget -qO-  https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
#
# Env overrides:
#   DEST=/usr/local/bin   install location (default: ~/.local/bin)
#   VERSION=v0.2.0        install a specific tag (default: latest release)
set -eu

REPO="magicabdel/intune-container"
BIN="intune-container"
DEST="${DEST:-$HOME/.local/bin}"

# --- pick a downloader -------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
    fetch_to() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    fetch_to() { wget -qO "$2" "$1"; }
else
    echo "error: need curl or wget" >&2
    exit 1
fi

# --- detect target -----------------------------------------------------------
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
    *)
        echo "No prebuilt binary for '$arch'. Install from source instead:" >&2
        echo "  cargo install $BIN" >&2
        exit 1
        ;;
esac

# --- resolve version ---------------------------------------------------------
tag="${VERSION:-}"
if [ -z "$tag" ]; then
    tag=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' \
        | head -1 | sed -E 's/.*"([^"]+)"$/\1/')
fi
[ -n "$tag" ] || { echo "error: could not determine the latest release" >&2; exit 1; }

asset="$BIN-$tag-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

# --- download + verify + install ---------------------------------------------
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $BIN $tag ($target)..."
fetch_to "$url" "$tmp/$asset"

# Verify checksum if available (best-effort).
if fetch_to "$url.sha256" "$tmp/$asset.sha256" 2>/dev/null && command -v sha256sum >/dev/null 2>&1; then
    ( cd "$tmp" && sha256sum -c "$asset.sha256" >/dev/null 2>&1 ) \
        && echo "Checksum OK" || { echo "error: checksum verification failed" >&2; exit 1; }
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$DEST"
install -m755 "$tmp/$BIN-$tag-$target/$BIN" "$DEST/$BIN"

echo "✓ Installed $BIN $tag to $DEST/$BIN"
case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "  Add $DEST to your PATH, e.g.:  export PATH=\"$DEST:\$PATH\"" ;;
esac
echo "  Get started:  $BIN enroll"
