#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WRAPPER="$ROOT/launchd/zzd-wrapper"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/install"

cat > "$TMP/bin/uname" <<'SH'
#!/bin/sh
echo Darwin
SH
cat > "$TMP/bin/curl" <<'SH'
#!/bin/sh
set -eu
url=""
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  *releases/latest) cp "$ZZD_FIXTURES/release.json" "$out" ;;
  *manifest.json) cp "$ZZD_FIXTURES/manifest.json" "$out" ;;
  *zigzag-macos-aarch64) cp "$ZZD_FIXTURES/zigzag-macos-aarch64" "$out" ;;
  *) exit 1 ;;
esac
SH
cat > "$TMP/bin/codesign" <<'SH'
#!/bin/sh
case "$1" in
  -d) echo 'Identifier=com.shukantpal.zigzag' >&2; echo 'TeamIdentifier=7YZK8D3B48' >&2 ;;
esac
exit 0
SH
cat > "$TMP/bin/launchctl" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$ZZD_LAUNCHCTL_LOG"
SH
cat > "$TMP/bin/security" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$ZZD_SECURITY_LOG"
grep -q 'echo old-daemon' "$ZZD_BINARY_PATH"
candidate=""
while [ $# -gt 0 ]; do
  if [ "$1" = -T ]; then
    candidate="$2"
    break
  fi
  shift
done
[ -n "$candidate" ] && [ -x "$candidate" ]
if [ "${ZZD_SECURITY_FAIL:-0}" = 1 ]; then
  exit 1
fi
SH
cat > "$TMP/install/zigzag" <<'SH'
#!/bin/sh
echo old-daemon
SH
chmod +x "$TMP/bin/"* "$TMP/install/zigzag"
printf 'v0.1.1\n' > "$TMP/install/zigzag.version"
mkdir -p "$TMP/fixtures"
cat > "$TMP/fixtures/zigzag-macos-aarch64" <<'SH'
#!/bin/sh
echo new-daemon
SH
DIGEST="$(shasum -a 256 "$TMP/fixtures/zigzag-macos-aarch64" | awk '{print $1}')"
python3 - "$TMP/fixtures" "$DIGEST" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
(p / "manifest.json").write_text(json.dumps({
    "version": "v0.1.2", "target": "aarch64-apple-darwin",
    "sha256": sys.argv[2], "commit": "a" * 40,
}))
(p / "release.json").write_text(json.dumps({
    "tag_name": "v0.1.2", "assets": [
        {"name": "zigzag-macos-aarch64.manifest.json",
         "browser_download_url": "https://github.com/ShukantPal/zigzag/releases/download/v0.1.2/manifest.json"},
        {"name": "zigzag-macos-aarch64",
         "browser_download_url": "https://github.com/ShukantPal/zigzag/releases/download/v0.1.2/zigzag-macos-aarch64"},
    ],
}))
PY

export PATH="$TMP/bin:$PATH"
export ZZD_INSTALL_DIR="$TMP/install"
export ZZD_BINARY_PATH="$TMP/install/zigzag"
export ZZD_RELEASE_API_URL="https://api.github.com/repos/ShukantPal/zigzag/releases/latest"
export ZZD_FIXTURES="$TMP/fixtures"
export ZZD_LAUNCHCTL_LOG="$TMP/launchctl.log"
export ZZD_SECURITY_LOG="$TMP/security.log"
export HOME="$TMP/home"
mkdir -p "$HOME/Library/Keychains"
touch "$HOME/Library/Keychains/login.keychain-db"

"$WRAPPER" --probe > "$TMP/update-output" 2> "$TMP/update-error"
[ "$(cat "$TMP/install/zigzag.version")" = v0.1.2 ]
grep -q 'kickstart -k gui/' "$TMP/launchctl.log"
grep -q -- '-U -a exec-allowlist -s zigzag -T .*login.keychain-db' "$TMP/security.log"
[ "$(wc -l < "$TMP/security.log" | tr -d ' ')" = 1 ]

# On the next launch, the installed version marker suppresses another restart.
"$WRAPPER" --probe > "$TMP/normal-output"
grep -q '^new-daemon$' "$TMP/normal-output"
[ "$(wc -l < "$TMP/launchctl.log" | tr -d ' ')" = 1 ]

# If ACL carry-forward fails, keep the installed daemon and version intact.
cat > "$TMP/install/zigzag" <<'SH'
#!/bin/sh
echo old-daemon
SH
chmod +x "$TMP/install/zigzag"
printf 'v0.1.1\n' > "$TMP/install/zigzag.version"
export ZZD_SECURITY_FAIL=1
"$WRAPPER" --probe > "$TMP/acl-failure-output" 2> "$TMP/acl-failure-error"
unset ZZD_SECURITY_FAIL
grep -q '^old-daemon$' "$TMP/acl-failure-output"
grep -q 'could not authorize verified update' "$TMP/acl-failure-error"
[ "$(cat "$TMP/install/zigzag.version")" = v0.1.1 ]
[ "$(wc -l < "$TMP/launchctl.log" | tr -d ' ')" = 1 ]

# A bad digest must not replace the daemon or request a restart.
printf 'v0.1.1\n' > "$TMP/install/zigzag.version"
python3 - "$TMP/fixtures/manifest.json" <<'PYBAD'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
m = json.loads(p.read_text())
m["sha256"] = "0" * 64
p.write_text(json.dumps(m))
PYBAD
"$WRAPPER" --probe > "$TMP/rejected-output" 2> "$TMP/rejected-error"
grep -q '^old-daemon$' "$TMP/rejected-output"
grep -q 'SHA-256 mismatch' "$TMP/rejected-error"
[ "$(cat "$TMP/install/zigzag.version")" = v0.1.1 ]
[ "$(wc -l < "$TMP/launchctl.log" | tr -d ' ')" = 1 ]

echo "zzd wrapper update, Keychain ACL, no-op, and rejection paths passed"
