#!/bin/bash
# Build, stably code-sign, and redeploy the zigzag relay.
#
# MUST run in an interactive terminal on the Mac (the GUI login session).
# Two things require it:
#   1. `codesign` reads the signing key from the login keychain, which SSH
#      sessions cannot reach ("User interaction is not allowed").
#   2. Restarting the LaunchAgent from SSH lands the daemon in the wrong
#      macOS security session and its keychain reads hang forever
#      (learned 2026-09-17). Only a GUI-session bootstrap is safe.
#
# Stable signing is what keeps the keychain allowlist working across
# rebuilds: every `cargo build` re-generates the ad-hoc signature (new
# identifier suffix, new cdhash), so the keychain sees each rebuild as a
# different app and re-prompts. Signing with a real certificate and a fixed
# --identifier gives every build the same designated requirement, so the
# keychain ACL granted once keeps matching forever.
set -euo pipefail

if [ -n "${SSH_CLIENT:-}${SSH_TTY:-}" ]; then
    echo "ERROR: run this in your Mac terminal, not over SSH." >&2
    echo "Code signing needs the login keychain and the daemon restart needs" >&2
    echo "the GUI login session." >&2
    exit 1
fi

cd "$(dirname "$0")/.."

CERT="${ZIGZAG_SIGN_IDENTITY:-Apple Development: Shukant Pal (NH5F3PDHQ8)}"
IDENTIFIER="com.shukantpal.zigzag"
BIN="target/release/zigzag"

echo "==> building release"
cargo build --release

echo "==> signing with: $CERT"
codesign --force --sign "$CERT" --identifier "$IDENTIFIER" \
    --options runtime --timestamp "$BIN"

echo "==> verifying signature"
codesign -v "$BIN"
codesign -d -v "$BIN" 2>&1 | grep -E "Identifier|TeamIdentifier|Signature"

echo "==> restarting LaunchAgent"
launchctl bootout "gui/$(id -u)/com.shukantpal.zigzag" 2>/dev/null || true
sleep 2
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.shukantpal.zigzag.plist"
sleep 3

echo "==> smoke test"
TOKEN="$(cat "$HOME/.codex/zigzag/zigzag.token")"
curl -s -m 10 http://127.0.0.1:8765/v1/events \
    -H "Authorization: Bearer $TOKEN" -o /dev/null \
    -w "events endpoint: HTTP %{http_code}\n"

echo "done. First exec request may trigger one keychain prompt;"
echo "choose 'Always Allow' so future rebuilds never prompt again."
