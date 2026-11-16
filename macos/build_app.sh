#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="CineFS"
BUILD_DIR="${ROOT_DIR}/macos/build"
APP_BUNDLE="${BUILD_DIR}/${APP_NAME}.app"

echo "==> [1/4] Building Rust core + GPU player (libcinefs_player.a) and CLI (cinefs-cli) in release mode..."
cd "${ROOT_DIR}"
cargo build --release

echo "==> [2/4] Assembling ${APP_NAME}.app bundle structure..."
rm -rf "${APP_BUNDLE}"
mkdir -p "${APP_BUNDLE}/Contents/MacOS"
mkdir -p "${APP_BUNDLE}/Contents/Resources"

cp "${ROOT_DIR}/macos/CineFS/Info.plist" "${APP_BUNDLE}/Contents/Info.plist"
cp "${ROOT_DIR}/macos/CineFS/AppIcon.icns" "${APP_BUNDLE}/Contents/Resources/AppIcon.icns"
cp "${ROOT_DIR}/target/release/cinefs-cli" "${APP_BUNDLE}/Contents/MacOS/cinefs-cli"

echo "==> [3/4] Compiling SwiftUI macOS app and linking libcinefs_core.a..."
xcrun swiftc \
    -O \
    -parse-as-library \
    -import-objc-header "${ROOT_DIR}/macos/CineFS/cinefs_ffi.h" \
    "${ROOT_DIR}/macos/CineFS/CineFSApp.swift" \
    "${ROOT_DIR}/target/release/libcinefs_player.a" \
    -framework AppKit \
    -framework Metal \
    -framework QuartzCore \
    -framework CoreGraphics \
    -framework IOKit \
    -framework CoreAudio \
    -framework AudioToolbox \
    -framework SwiftUI \
    -framework UniformTypeIdentifiers \
    -o "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"

echo "==> [4/4] Code-signing ${APP_NAME}.app..."
codesign --force --deep --sign - "${APP_BUNDLE}"

echo "==> Creating distributable ${BUILD_DIR}/${APP_NAME}.dmg..."
DMG_STAGING="${BUILD_DIR}/dmg_staging"
rm -rf "${DMG_STAGING}" "${BUILD_DIR}/${APP_NAME}.dmg"
mkdir -p "${DMG_STAGING}"
cp -R "${APP_BUNDLE}" "${DMG_STAGING}/"
cp "${ROOT_DIR}/macos/CineFS/AppIcon.icns" "${DMG_STAGING}/.VolumeIcon.icns"
SetFile -a C "${DMG_STAGING}" 2>/dev/null || true
ln -s /Applications "${DMG_STAGING}/Applications"
hdiutil create -volname "${APP_NAME}" -srcfolder "${DMG_STAGING}" -ov -format UDZO "${BUILD_DIR}/${APP_NAME}.dmg" >/dev/null
rm -rf "${DMG_STAGING}"

echo "==> Done!"
echo "    App Bundle : ${APP_BUNDLE}"
echo "    Disk Image : ${BUILD_DIR}/${APP_NAME}.dmg"
