#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# ibackup release builder for Windows, macOS and Linux.
#
#   ./scripts/build-all.sh                 build for THIS machine only (fastest)
#   ./scripts/build-all.sh --cross         also try cross-toolchains that are
#                                          installed (see notes), for
#                                          windows-x64, macos-x64, macos-arm64
#   ./scripts/build-all.sh --clean         wipe dist/ first
#
# Output lands in dist/<name>/ — ready to zip and share. Every artifact has the
# icon embedded or bundled:
#   Windows:  build.rs embeds assets/ibackup.ico into the .exe resources
#             (Explorer, taskbar, shortcuts pick it up automatically)
#   macOS:    assets/ibackup.icns inside a proper ibackup.app bundle
#   Linux:    window/taskbar icon is compiled into the binary (egui), plus a
#             .desktop file + 512px icon to install system-wide
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "$(dirname "$0")")" && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)"
DIST="$ROOT/dist"
CROSS=no
CLEAN=no

for arg in "$@"; do
    case "$arg" in
        --clean) CLEAN=yes ;;
        --cross) CROSS=yes ;;
        *) echo "unknown argument: $arg (see the header of this script)"; exit 2 ;;
    esac
done
[[ "$CLEAN" = yes ]] && rm -rf "$DIST"

cd "$ROOT"
cargo -vV | sed -n 's/^host:/HOST= /p'

# --- one build target -------------------------------------------------------

needs_cross_toolchain() { case "$1" in "$HOST"|*-windows-msvc*|*-windows-gnu*) false ;; *) true ;; esac }

build() { # build <target>   (or no argument = native)
    local target="${1:-}"
    local dir="$DIST/ibackup-$VERSION-${target:-native}"
    rm -rf "$dir"; mkdir -p "$dir"
    echo "== building ${target:-native} -> $dir"
    if [[ -n "$target" ]]; then
        if needs_cross_toolchain "$target" ; then
            if command -v cargo-zigbuild >/dev/null; then
                cargo zigbuild --release --target "$target"
            else
                echo "   (skipped: no cross toolchain for $target - install cargo-zigbuild)"
                return 0
            fi
        else
            cargo build --release --target "$target"
        fi
        cp -r "target/$target/release/ibackup."* "$dir/" 2>/dev/null || true
        cp "target/$target/release/ibackup"* "$dir/" 2>/dev/null || true
    else
        cargo build --release
        cp -r target/release/ibackup* "$dir/" 2>/dev/null || true
    fi
    # keep just the runnable file(s) from the copy above
    case "$target" in
        *windows*) mv "$dir/ibackup.exe" "$dir/ibackup.exe" 2>/dev/null || true ;;
    esac
    find "$dir" -maxdepth 1 -type f ! \( -name ibackup -o -name 'ibackup.exe' \) -delete || true
    find "$dir" -maxdepth 1 -type d ! -samepath "$dir" -exec rm -rf {} + 2>/dev/null || true

    case "$target" in
        *apple-darwin*)      pack_mac "$dir" "$target" ;;
        *windows*)           pack_windows "$dir" ;;
        ""|*linux*)          pack_linux "$dir" ;;
    esac
    echo
}

# --- per-OS packaging: the icon travels with it -----------------------------

pack_windows() { # .exe already carries the icon (build.rs); add icon + readme
    local dir="$1"; shift
    cp "$ROOT/assets/ibackup.ico" "$dir/" 2>/dev/null || true
    cp "$ROOT/assets/ibackup-256.png" "$dir/icon.png" 2>/dev/null || true
}

pack_mac() { # wrap in a .app bundle with the icns icon (runs only on a mac)
    local dir="$1" target="$2"; shift 2
    if [[ "$target" != "$HOST" ]]; then
        echo "   (icon bundle for $target needs a mac to zip into .app; the binary is ready)"
        return 0
    fi
    local app="$dir/ibackup.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    mv "$dir/ibackup" "$app/Contents/MacOS/"
    cp "$ROOT/assets/ibackup.icns" "$app/Contents/Resources/ibackup.icns"
    sed "s/@VERSION@/$VERSION/g" "$ROOT/packaging/Info.plist" > "$app/Contents/Info.plist"
    ln -s /Applications "$dir/Applications" || true
    codesign --force --deep --sign - "$app" 2>/dev/null \
        || echo "   (not codesigned: unsigned builds work with right-click → Open)"
}

pack_linux() { # icon is compiled-in; ship the desktop entry for menus
    local dir="$1"; shift
    cp "$ROOT/assets/ibackup-512.png" "$dir/ibackup.png" 2>/dev/null || true
    cp "$ROOT/packaging/ibackup.desktop" "$dir/" 2>/dev/null || true
    cat >> "$dir/README.txt" <<'EOF'
Optional desktop-menu entry:
  install -m 755 ibackup ~/.local/bin/
  install -m 644 ibackup.png ~/.local/share/icons/hicolor/512x512/apps/ibackup.png
  install -m 644 ibackup.desktop ~/.local/share/applications/ibackup.desktop
EOF
}

# --- go ---------------------------------------------------------------------

build   # this machine, always

if [[ "$CROSS" = yes ]]; then
    for t in x86_64-pc-windows-msvc x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
             x86_64-apple-darwin aarch64-apple-darwin; do
        [[ "$t" = "$HOST" ]] && continue
        build "$t" || true
    done
fi

echo "artifacts in $DIST:"
ls -1 "$DIST" 2>/dev/null
