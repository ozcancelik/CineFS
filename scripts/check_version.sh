#!/usr/bin/env bash
# Checks that the app version is the same everywhere it is written down:
#   Cargo.toml (workspace.package.version), macos/CineFS/Info.plist
#   (CFBundleShortVersionString) and windows/CineFS.iss (MyAppVersion).
#
# Usage:
#   scripts/check_version.sh          # the three files must agree with each other
#   scripts/check_version.sh v0.2.0   # ...and with this tag (leading "v" optional)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

cargo_version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)"
plist_version="$(grep -A1 '<key>CFBundleShortVersionString</key>' macos/CineFS/Info.plist \
    | sed -n 's/.*<string>\(.*\)<\/string>.*/\1/p')"
iss_version="$(sed -n 's/^#define MyAppVersion "\(.*\)".*/\1/p' windows/CineFS.iss)"

echo "Cargo.toml                 : ${cargo_version:-<missing>}"
echo "macos/CineFS/Info.plist    : ${plist_version:-<missing>}"
echo "windows/CineFS.iss         : ${iss_version:-<missing>}"

status=0
if [[ -z "${cargo_version}" || -z "${plist_version}" || -z "${iss_version}" ]]; then
    echo "error: could not read the version from every file" >&2
    status=1
elif [[ "${cargo_version}" != "${plist_version}" || "${cargo_version}" != "${iss_version}" ]]; then
    echo "error: the version differs between files" >&2
    status=1
fi

if [[ $# -ge 1 ]]; then
    tag_version="${1#v}"
    echo "tag                        : ${tag_version}"
    if [[ "${tag_version}" != "${cargo_version}" ]]; then
        echo "error: tag ${1} does not match the version in the files (${cargo_version})" >&2
        status=1
    fi
fi

if [[ ${status} -eq 0 ]]; then
    echo "OK: version ${cargo_version}"
fi
exit ${status}
