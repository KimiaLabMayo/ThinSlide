#!/usr/bin/env bash
# Packages the thinslide-gui binary into a macOS .app bundle (ad-hoc signed)
# and wraps it in a distributable .dmg.
#
# Usage: package_macos_app.sh <thinslide-gui-binary> <version> <output-dir>
set -euo pipefail

BINARY="$1"
VERSION="$2"
OUT_DIR="$3"

APP_NAME="ThinSlide"
BUNDLE_ID="io.github.kimialabmayo.thinslide"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

APP_DIR="$WORK_DIR/$APP_NAME.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "$BINARY" "$APP_DIR/Contents/MacOS/thinslide-gui"
chmod +x "$APP_DIR/Contents/MacOS/thinslide-gui"

# Build the .icns from the source PNG (sips/iconutil are macOS-only, so this
# is generated at packaging time rather than checked in as a binary asset).
ICONSET="$WORK_DIR/icon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$REPO_ROOT/assets/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z "$((size * 2))" "$((size * 2))" "$REPO_ROOT/assets/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP_DIR/Contents/Resources/icon.icns"

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>thinslide-gui</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.medical</string>
</dict>
</plist>
PLIST

# Ad-hoc sign: no paid Developer ID is required, but macOS still refuses to
# run an unsigned arm64 binary, so a local "-" signature is the minimum
# needed for the app to launch at all.
codesign --force --deep --sign - "$APP_DIR"

mkdir -p "$OUT_DIR"

DMG_DIR="$WORK_DIR/dmg"
mkdir -p "$DMG_DIR"
cp -R "$APP_DIR" "$DMG_DIR/"
ln -s /Applications "$DMG_DIR/Applications"

hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_DIR" -ov -format UDZO \
    "$OUT_DIR/thinslide-macos-arm64.dmg"
