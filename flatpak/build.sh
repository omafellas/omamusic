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
command -v cargo >/dev/null || { echo "error: cargo is not installed" >&2; exit 1; }

# Start from a clean work tree so stale repo/build output from a previous run
# can never be re-exported into the source tree.
rm -rf "$WORK"

# Pre-build the binary on the host. The GNOME SDK does not ship a rust toolchain
# for aarch64 (the rust-stable sdk-extension is x86_64-only on Flathub), so we
# compile here and copy the artifact into the exported source tree.
if command -v mbx >/dev/null; then
    echo "==> Building omamusic on host (mbx build --release)"
    mbx build --release
else
    echo "==> Building omamusic on host (cargo build --release)"
    cargo build --release
fi

# Export a clean source tree (tracked files plus new non-ignored ones) so the
# build never copies target/ or .git into the sandbox.
echo "==> Exporting sources to $SRC"
mkdir -p "$SRC"
git ls-files --cached --others --exclude-standard -z |
    tar --null --files-from - -cf - | tar -xf - -C "$SRC"

# Copy the pre-built binary into the source tree so flatpak-builder can install it.
mkdir -p "$SRC/target/release"
cp target/release/omamusic "$SRC/target/release/omamusic"

# Make sure flathub and the GNOME runtime/SDK are present.
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
if ! flatpak info "$RUNTIME//" "$RUNTIME//$VERSION" >/dev/null 2>&1; then
    echo "==> Installing $RUNTIME//$VERSION and $SDK//$VERSION"
    flatpak install -y --user flathub "$RUNTIME//$VERSION" "$SDK//$VERSION"
fi

echo "==> Building flatpak"
flatpak-builder --disable-rofiles-fuse --repo="$WORK/repo" --force-clean "$WORK/build" "flatpak/$APP_ID.json"

echo "==> Bundling $APP_ID.flatpak"
flatpak build-bundle "$WORK/repo" "$APP_ID.flatpak" "$APP_ID"

echo
echo "Built $APP_ID.flatpak"
echo "Install it with:  flatpak install --user ./$APP_ID.flatpak"
echo "Then start it:    flatpak run $APP_ID"
