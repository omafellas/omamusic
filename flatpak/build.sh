#!/usr/bin/env bash
# Build omamusic as a standalone Flatpak bundle.
#
# Requires: flatpak, flatpak-builder, git. The first run downloads the GNOME
# runtime and SDK from flathub (~2 GB). Cargo fetches crates over the network
# during the build; the produced app itself asks for no network permission.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

APP_ID=io.github.omafellas.omamusic
RUNTIME=org.gnome.Platform
SDK=org.gnome.Sdk
VERSION=48
WORK=flatpak/.build
SRC=$WORK/src

command -v flatpak >/dev/null || { echo "error: flatpak is not installed" >&2; exit 1; }
command -v flatpak-builder >/dev/null || { echo "error: flatpak-builder is not installed" >&2; exit 1; }

# Export a clean source tree (tracked files plus new non-ignored ones) so the
# build never copies target/ or .git into the sandbox.
echo "==> Exporting sources to $SRC"
rm -rf "$SRC"
mkdir -p "$SRC"
git ls-files --cached --others --exclude-standard -z |
    tar --null --files-from - -cf - | tar -xf - -C "$SRC"

# Make sure flathub and the GNOME runtime/SDK are present.
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
if ! flatpak info "$RUNTIME//" "$RUNTIME//$VERSION" >/dev/null 2>&1; then
    echo "==> Installing $RUNTIME//$VERSION and $SDK//$VERSION"
    flatpak install -y --user flathub "$RUNTIME//$VERSION" "$SDK//$VERSION"
fi

echo "==> Building flatpak"
flatpak-builder --repo="$WORK/repo" --force-clean "$WORK/build" "flatpak/$APP_ID.json"

echo "==> Bundling $APP_ID.flatpak"
flatpak build-bundle "$WORK/repo" "$APP_ID.flatpak" "$APP_ID"

echo
echo "Built $APP_ID.flatpak"
echo "Install it with:  flatpak install --user ./$APP_ID.flatpak"
echo "Then start it:    flatpak run $APP_ID"
