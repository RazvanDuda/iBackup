#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Package ONE already-built binary into dist/ and compress it for a release.
#
#   ./scripts/package-release.sh <label> <target-triple> <version>
#
#   ./scripts/package-release.sh linux-amd64   x86_64-unknown-linux-gnu  0.3.0
#   ./scripts/package-release.sh macos-intel   x86_64-apple-darwin       0.3.0
#   ./scripts/package-release.sh macos-arm64   aarch64-apple-darwin      0.3.0
#   ./scripts/package-release.sh windows-amd64 x86_64-pc-windows-msvc    0.3.0
#
# Expects `cargo build --release --target <target-triple>` to have run first.
# Produces dist/ibackup-<version>-<label>/ plus one archive next to it:
#   windows -> .zip (exe + icon + README.txt)
#   macOS   -> .zip (ad-hoc signed ibackup.app + INSTALL.txt)
#   Linux   -> .tar.gz (binary + .desktop + icon + README.txt)
# Used by .github/workflows/release.yml; safe to run by hand.
# ---------------------------------------------------------------------------
set -euo pipefail

LABEL="${1:?usage: package-release.sh <label> <target-triple> <version>}"
TARGET="${2:?usage: package-release.sh <label> <target-triple> <version>}"
VERSION="${3:?usage: package-release.sh <label> <target-triple> <version>}"

ROOT="$(cd "$(dirname "$(dirname "$0")")" && pwd)"
DIST="$ROOT/dist"
DIR="$DIST/ibackup-$VERSION-$LABEL"
REL="$ROOT/target/$TARGET/release"

[[ -d "$REL" ]] || { echo "error: $REL does not exist - run: cargo build --release --target $TARGET"; exit 1; }

rm -rf "$DIR"
mkdir -p "$DIR"

case "$TARGET" in
  *windows*)
    cp "$REL/ibackup.exe" "$DIR/"
    cp "$ROOT/assets/ibackup.ico" "$DIR/"
    cp "$ROOT/assets/ibackup-256.png" "$DIR/icon.png"
    cat > "$DIR/README.txt" <<'EOF'
ibackup - back up an iPhone over USB to this computer

Run:  ibackup.exe all --out C:\iPhoneBackups
      ibackup.exe ui          (desktop window)

Requirements: iTunes or the Apple Devices app (for the Apple Mobile Device
USB driver) is needed for the iPhone connection on Windows.

The .exe already carries the app icon; ibackup.ico / icon.png are here for
shortcuts. Windows SmartScreen may warn on first run (unsigned build):
"More info" -> "Run anyway".
EOF
    ARCHIVE="$DIST/ibackup-$VERSION-$LABEL.zip"
    rm -f "$ARCHIVE"
    if command -v zip >/dev/null 2>&1; then
        (cd "$DIST" && zip -qr "$ARCHIVE" "$(basename "$DIR")")
    elif command -v 7z >/dev/null 2>&1; then
        (cd "$DIST" && 7z a -tzip -bso0 -bsp0 "$ARCHIVE" "$(basename "$DIR")")
    else
        powershell.exe -NoProfile -Command \
            "Compress-Archive -Path '$(cygpath -w "$DIR")' -DestinationPath '$(cygpath -w "$ARCHIVE")' -Force"
    fi
    ;;

  *apple-darwin*)
    APP="$DIR/ibackup.app"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$REL/ibackup" "$APP/Contents/MacOS/ibackup"
    chmod +x "$APP/Contents/MacOS/ibackup"
    cp "$ROOT/assets/ibackup.icns" "$APP/Contents/Resources/ibackup.icns"
    sed "s/@VERSION@/$VERSION/g" "$ROOT/packaging/Info.plist" > "$APP/Contents/Info.plist"
    ln -s /Applications "$DIR/Applications" 2>/dev/null || true
    codesign --force --deep --sign - "$APP" 2>/dev/null \
        || echo "   (ad-hoc codesign unavailable - the app still runs after right-click Open)"
    cat > "$DIR/INSTALL.txt" <<EOF
ibackup $VERSION - back up an iPhone over USB to this Mac

Drag ibackup.app into Applications, then open it.

This build is ad-hoc signed (not notarised), so the first launch needs
right-click -> Open (or System Settings -> Privacy & Security -> Open Anyway).

Command line:  /Applications/ibackup.app/Contents/MacOS/ibackup all --out ~/iPhoneBackups
               /Applications/ibackup.app/Contents/MacOS/ibackup ui
EOF
    ARCHIVE="$DIST/ibackup-$VERSION-$LABEL.zip"
    rm -f "$ARCHIVE"
    ditto -c -k --sequesterRsrc --keepParent "$DIR" "$ARCHIVE"
    ;;

  *)
    cp "$REL/ibackup" "$DIR/"
    chmod +x "$DIR/ibackup"
    cp "$ROOT/assets/ibackup-512.png" "$DIR/ibackup.png"
    sed "s/^Version=.*/Version=$VERSION/" "$ROOT/packaging/ibackup.desktop" > "$DIR/ibackup.desktop"
    cat > "$DIR/README.txt" <<'EOF'
ibackup - back up an iPhone over USB to this computer

Run:  ./ibackup all --out ~/iPhoneBackups
      ./ibackup ui          (desktop window)

Requirements: usbmuxd (Fedora: sudo dnf install usbmuxd; Debian/Ubuntu:
sudo apt install usbmuxd). Unplug and replug the iPhone after installing it.

Optional desktop-menu entry:
  install -m 755 ibackup ~/.local/bin/
  install -m 644 ibackup.png ~/.local/share/icons/hicolor/512x512/apps/ibackup.png
  install -m 644 ibackup.desktop ~/.local/share/applications/ibackup.desktop
EOF
    ARCHIVE="$DIST/ibackup-$VERSION-$LABEL.tar.gz"
    rm -f "$ARCHIVE"
    tar -C "$DIST" -czf "$ARCHIVE" "$(basename "$DIR")"
    ;;
esac

echo "packaged: $ARCHIVE"
ls -lh "$ARCHIVE"
