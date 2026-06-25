#!/bin/sh
# intune-container installer — downloads the latest AppImage from GitHub Releases
# and installs it to ~/.local/bin (override with DEST=...).
#
#   curl -fsSL https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
#   wget -qO-  https://raw.githubusercontent.com/magicabdel/intune-container/master/install.sh | sh
#
# The AppImage is the whole app: run `intune-container` with no arguments for the
# graphical interface, or with a subcommand (enroll, edge, stop, …) for the CLI.
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

# --- check arch --------------------------------------------------------------
# Only x86_64 bundles are published. Other arches build from source.
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) ;;
    *)
        echo "No prebuilt AppImage for '$arch'. Build from source instead:" >&2
        echo "  git clone https://github.com/$REPO && cd intune-container" >&2
        echo "  just install        # builds the GUI (needs Rust + Node + WebKitGTK)" >&2
        exit 1
        ;;
esac

# --- resolve the release + AppImage asset ------------------------------------
if [ -n "${VERSION:-}" ]; then
    api="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
else
    api="https://api.github.com/repos/$REPO/releases/latest"
fi

release=$(fetch "$api") || { echo "error: could not query GitHub Releases" >&2; exit 1; }

# Pull the AppImage download URL out of the release JSON (prefer the 64-bit one).
urls=$(printf '%s' "$release" \
    | grep -oE '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]+\.AppImage"' \
    | sed -E 's/.*"([^"]+)"$/\1/')
url=$(printf '%s\n' "$urls" | grep -iE 'amd64|x86_64|x86-64' | head -1)
[ -n "$url" ] || url=$(printf '%s\n' "$urls" | head -1)
[ -n "$url" ] || { echo "error: no AppImage found in the release" >&2; exit 1; }

# --- download + install ------------------------------------------------------
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $BIN ($url)..."
fetch_to "$url" "$tmp/$BIN.AppImage"

mkdir -p "$DEST"
install -m755 "$tmp/$BIN.AppImage" "$DEST/$BIN"

echo "✓ Installed $BIN to $DEST/$BIN"
case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "  Add $DEST to your PATH, e.g.:  export PATH=\"$DEST:\$PATH\"" ;;
esac
# AppImages need FUSE; most desktops ship it, but flag the common failure.
if ! command -v fusermount >/dev/null 2>&1 && ! command -v fusermount3 >/dev/null 2>&1; then
    echo "  Note: AppImages need FUSE. If it won't start, install 'fuse' or run:" >&2
    echo "        $BIN --appimage-extract-and-run" >&2
fi
echo "  Open the interface:  $BIN"
echo "  Or use the CLI:      $BIN enroll"
