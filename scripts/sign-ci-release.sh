#!/bin/bash
# Sign a release binary with the Apple-issued identity required by the updater.
#
# The updater (relay/src/update.rs verify_codesign) pins the Apple Team ID
# 7YZK8D3B48 and the identifier com.shukantpal.zigzag. The post-sign check
# below enforces the SAME requirement the updater enforces, so a release whose
# signing identity drifts (wrong cert type, wrong team) fails the build instead
# of shipping an update the relay will reject. Keep the requirement string in
# sync with verify_codesign().
set -euo pipefail

: "${SIGNING_IDENTITY:?ZIGZAG_SIGNING_IDENTITY is required for release signing}"

# Must match the requirement built in relay/src/update.rs verify_codesign().
UPDATER_REQUIREMENT='anchor apple generic and identifier "com.shukantpal.zigzag" and certificate leaf[subject.OU] = "7YZK8D3B48"'

binary="${1:?usage: sign-ci-release.sh BINARY}"
codesign --force --sign "$SIGNING_IDENTITY" \
    --identifier com.shukantpal.zigzag --options runtime --timestamp "$binary"
codesign --verify --strict --deep --verbose=2 "$binary"
codesign --verify --strict --deep --verbose=2 -R="$UPDATER_REQUIREMENT" "$binary"
