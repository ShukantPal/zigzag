#!/bin/bash
# Sign a release binary with the Apple identity required by the updater.
#
# The updater (relay/src/update.rs) and scripts/verify-release.sh require:
#   identifier "com.shukantpal.zigzag" and
#   certificate leaf[subject.OU] = "7YZK8D3B48"
# (the "(NH5F3PDHQ8)" parenthetical in the certificate CN is stale; the real
# TeamIdentifier is 7YZK8D3B48 — see PROVENANCE.md).
set -euo pipefail

: "${SIGNING_IDENTITY:?ZIGZAG_SIGNING_IDENTITY is required for release signing}"

binary="${1:?usage: sign-ci-release.sh BINARY}"
codesign --force --sign "$SIGNING_IDENTITY" \
    --identifier com.shukantpal.zigzag --options runtime --timestamp "$binary"
codesign --verify --strict --deep --verbose=2 "$binary"
