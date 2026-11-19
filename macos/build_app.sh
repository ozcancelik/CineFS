#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="CineFS"
BUILD_DIR="${ROOT_DIR}/macos/build"
APP_BUNDLE="${BUILD_DIR}/${APP_NAME}.app"
ARCH_DIR="${BUILD_DIR}/arch"

# Oldest macOS the app runs on. Must match LSMinimumSystemVersion in Info.plist.
# Without an explicit target, swiftc uses the build machine's own macOS version as the
# minimum, and the app then refuses to launch on anything older.
MIN_MACOS="13.0"
export MACOSX_DEPLOYMENT_TARGET="${MIN_MACOS}"   # same minimum for the Rust code

# Universal binary by default: Apple Silicon + Intel. For a quicker local build of one
# architecture only:  MACOS_ARCHS=arm64 ./macos/build_app.sh
MACOS_ARCHS="${MACOS_ARCHS:-arm64 x86_64}"

rust_triple() {
    case "$1" in
        arm64)  echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *) echo "unsupported architecture: $1" >&2; exit 1 ;;
    esac
}

cd "${ROOT_DIR}"
rm -rf "${APP_BUNDLE}" "${ARCH_DIR}"
mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APP_BUNDLE}/Contents/Resources" "${ARCH_DIR}"

APP_SLICES=()
CLI_SLICES=()
for ARCH in ${MACOS_ARCHS}; do
    TRIPLE="$(rust_triple "${ARCH}")"

    echo "==> [1/4] (${ARCH}) Building Rust GPU player (libcinefs_player.a) and CLI (cinefs-cli)..."
    cargo build --release --target "${TRIPLE}" -p cinefs-player -p cinefs-cli

    echo "==> [2/4] (${ARCH}) Compiling SwiftUI macOS app and linking libcinefs_player.a..."
    xcrun swiftc \
        -O \
        -parse-as-library \
        -target "${ARCH}-apple-macos${MIN_MACOS}" \
        -import-objc-header "${ROOT_DIR}/macos/CineFS/cinefs_ffi.h" \
        "${ROOT_DIR}/macos/CineFS/CineFSApp.swift" \
        "${ROOT_DIR}/target/${TRIPLE}/release/libcinefs_player.a" \
        -framework AppKit \
        -framework Metal \
        -framework QuartzCore \
        -framework CoreGraphics \
        -framework IOKit \
        -framework CoreAudio \
        -framework AudioToolbox \
        -framework SwiftUI \
        -framework UniformTypeIdentifiers \
        -o "${ARCH_DIR}/${APP_NAME}-${ARCH}"

    APP_SLICES+=("${ARCH_DIR}/${APP_NAME}-${ARCH}")
    CLI_SLICES+=("${ROOT_DIR}/target/${TRIPLE}/release/cinefs-cli")
done

echo "==> [3/4] Assembling ${APP_NAME}.app bundle (${MACOS_ARCHS})..."
cp "${ROOT_DIR}/macos/CineFS/Info.plist" "${APP_BUNDLE}/Contents/Info.plist"
cp "${ROOT_DIR}/macos/CineFS/AppIcon.icns" "${APP_BUNDLE}/Contents/Resources/AppIcon.icns"
lipo -create "${APP_SLICES[@]}" -output "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"
lipo -create "${CLI_SLICES[@]}" -output "${APP_BUNDLE}/Contents/MacOS/cinefs-cli"
# Standalone copy of the universal CLI next to the DMG (uploaded by CI).
cp "${APP_BUNDLE}/Contents/MacOS/cinefs-cli" "${BUILD_DIR}/cinefs-cli"
rm -rf "${ARCH_DIR}"
echo "    architectures: $(lipo -archs "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}")"

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
