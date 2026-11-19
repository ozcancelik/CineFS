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

# Signing / notarization are driven by environment variables so the same script works
# locally and in CI:
#   CODESIGN_IDENTITY   "Developer ID Application: ..." name or SHA-1 hash of the identity.
#                       Unset or "-" = ad-hoc signature (fine for local testing only;
#                       other Macs will refuse to open a downloaded ad-hoc app).
#   NOTARY_KEY_PATH     App Store Connect API key (.p8) used by notarytool, plus
#   NOTARY_KEY_ID       its Key ID and
#   NOTARY_ISSUER_ID    Issuer ID. When all three are set (and the app is Developer ID
#                       signed), the DMG is notarized by Apple and the ticket is stapled.
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"

echo "==> [4/4] Code-signing ${APP_NAME}.app..."
if [[ "${CODESIGN_IDENTITY}" == "-" ]]; then
    echo "    (ad-hoc signature: set CODESIGN_IDENTITY to sign with a Developer ID)"
    codesign --force --deep --sign - "${APP_BUNDLE}"
else
    # Sign inside-out: the bundled CLI first, then the app. Hardened runtime + secure
    # timestamp are required for notarization. No entitlements are needed (no sandbox,
    # no JIT, no Apple Events).
    codesign --force --timestamp --options runtime --sign "${CODESIGN_IDENTITY}" \
        "${APP_BUNDLE}/Contents/MacOS/cinefs-cli"
    codesign --force --timestamp --options runtime --sign "${CODESIGN_IDENTITY}" \
        "${APP_BUNDLE}"
    codesign --verify --strict --verbose=2 "${APP_BUNDLE}"
fi

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

if [[ "${CODESIGN_IDENTITY}" != "-" ]]; then
    echo "==> Signing ${APP_NAME}.dmg..."
    codesign --force --timestamp --sign "${CODESIGN_IDENTITY}" "${BUILD_DIR}/${APP_NAME}.dmg"

    if [[ -n "${NOTARY_KEY_PATH:-}" && -n "${NOTARY_KEY_ID:-}" && -n "${NOTARY_ISSUER_ID:-}" ]]; then
        echo "==> Notarizing ${APP_NAME}.dmg with Apple (usually a few minutes)..."
        xcrun notarytool submit "${BUILD_DIR}/${APP_NAME}.dmg" \
            --key "${NOTARY_KEY_PATH}" --key-id "${NOTARY_KEY_ID}" --issuer "${NOTARY_ISSUER_ID}" \
            --wait
        echo "==> Stapling the notarization ticket to ${APP_NAME}.dmg..."
        xcrun stapler staple "${BUILD_DIR}/${APP_NAME}.dmg"
        xcrun stapler validate "${BUILD_DIR}/${APP_NAME}.dmg"
        spctl --assess --type open --context context:primary-signature --verbose=2 "${BUILD_DIR}/${APP_NAME}.dmg"
    else
        echo "    (not notarized: set NOTARY_KEY_PATH, NOTARY_KEY_ID and NOTARY_ISSUER_ID)"
    fi
fi

echo "==> Done!"
echo "    App Bundle : ${APP_BUNDLE}"
echo "    Disk Image : ${BUILD_DIR}/${APP_NAME}.dmg"
