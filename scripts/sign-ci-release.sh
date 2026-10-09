#!/bin/bash
# Sign a release binary with the Developer ID identity required by the updater.
set -euo pipefail

: "${SIGNING_IDENTITY:?ZIGZAG_SIGNING_IDENTITY is required for release signing}"

binary="${1:?usage: sign-ci-release.sh BINARY}"
codesign --force --sign "$SIGNING_IDENTITY" \
    --identifier com.shukantpal.zigzag --options runtime --timestamp "$binary"
codesign --verify --strict --deep --verbose=2 "$binary"
