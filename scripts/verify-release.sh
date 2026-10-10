#!/bin/bash
# verify-release.sh — verify the full GitHub trust chain of a zigzag release.
#
# This mirrors the checks in zzd/src/update.rs (Manager::fetch_candidate):
#   1. The release manifest carries a SLSA v1 provenance attestation,
#      keyless-signed by Fulcio using the release workflow's GitHub OIDC
#      identity and recorded in the Rekor transparency log. We verify it was
#      issued for this repo's release workflow on refs/heads/main.
#   2. The manifest's version matches the release tag and its target matches
#      the updater's expected target.
#   3. The release binary's SHA-256 matches the manifest digest.
#   4. The binary satisfies the Apple codesign requirement (anchor +
#      identifier + team). The requirement below was derived empirically from
#      a shipped release ("explicit requirement satisfied"):
#        - `anchor apple generic`, NOT `anchor apple`: plain `anchor apple`
#          fails for certificates chaining through Apple Worldwide Developer
#          Relations CA G3, while `anchor apple generic` matches.
#        - TeamIdentifier is 7YZK8D3B48 (from `codesign -d`). The
#          "(NH5F3PDHQ8)" parenthetical in the "Apple Development" certificate
#          CN is stale; the real subject OU / TeamIdentifier is 7YZK8D3B48.
#        - The requirement must NOT be prefixed with `=designated =>`: that is
#          `codesign -d -r-` display syntax and a syntax error as `-R` input.
#   5. The binary carries its own SLSA attestation from the same workflow,
#      pinned to the exact source commit claimed by the manifest
#      (--source-digest). This closes the loop: the manifest is trusted first,
#      then the binary is proven to have been built from the manifest's commit
#      by the trusted workflow.
#
# Usage: verify-release.sh [--tag v0.1.123] [--trust-root PATH] [--workdir DIR]
#   --tag        release tag to verify (default: latest release)
#   --trust-root Sigstore trusted root JSON for gh (default: gh's built-in)
#   --workdir    scratch directory (default: a fresh mktemp dir, cleaned up)
set -euo pipefail

REPO="ShukantPal/zigzag"
WORKFLOW="ShukantPal/zigzag/.github/workflows/ci.yml"
SOURCE_REF="refs/heads/main"
PREDICATE="https://slsa.dev/provenance/v1"
TARGET="aarch64-apple-darwin"
BINARY_NAME="zigzag-macos-aarch64"
MANIFEST_NAME="zigzag-macos-aarch64.manifest.json"
TEAM_ID="7YZK8D3B48"
IDENTIFIER="com.shukantpal.zigzag"
REQUIREMENT="anchor apple generic and identifier \"$IDENTIFIER\" and certificate leaf[subject.OU] = \"$TEAM_ID\""

TAG=""
TRUST_ROOT=""
WORKDIR=""

while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="$2"; shift 2;;
    --trust-root) TRUST_ROOT="$2"; shift 2;;
    --workdir) WORKDIR="$2"; shift 2;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

command -v gh >/dev/null || { echo "gh CLI is required" >&2; exit 2; }
[ "$(uname)" = "Darwin" ] || { echo "codesign checks require macOS" >&2; exit 2; }
if [ -n "$TRUST_ROOT" ]; then
  [ -f "$TRUST_ROOT" ] || { echo "trust root not found: $TRUST_ROOT" >&2; exit 2; }
fi

if [ -z "$WORKDIR" ]; then
  WORKDIR="$(mktemp -d)"
  trap 'rm -rf "$WORKDIR"' EXIT
else
  mkdir -p "$WORKDIR"
fi

# Do not use unauthenticated curl for GitHub API or release-asset requests.
# GitHub-hosted runners share the unauthenticated REST API rate limit, which
# can turn an otherwise valid release lookup into a 403. `gh` automatically
# uses GH_TOKEN (or the user's authenticated gh session), including in
# `gh release download` below.
github_api() { # $1 = endpoint relative to https://api.github.com/
  local endpoint="$1"
  shift
  gh api --method GET "$endpoint" "$@"
}

if [ -z "$TAG" ]; then
  if ! TAG="$(github_api "repos/$REPO/releases/latest" --jq '.tag_name')"; then
    echo "could not retrieve the latest release; check GH_TOKEN authentication and contents: read permission" >&2
    exit 1
  fi
fi
echo "verifying $REPO release $TAG"

if ! RELEASE_JSON="$(github_api "repos/$REPO/releases/tags/$TAG")"; then
  # The release action can return before the API has made the new release
  # visible.  This is deliberately a retryable failure for the CI wrapper.
  echo "release $TAG is not available through the GitHub API yet; it may still be publishing, or GH_TOKEN lacks contents: read permission" >&2
  exit 1
fi

require_asset() { # $1 = asset name; return nonzero until it is attached
  printf '%s' "$RELEASE_JSON" | python3 -c '
import json, sys
name = sys.argv[1]
assets = json.load(sys.stdin).get("assets", [])
if not any(asset.get("name") == name for asset in assets):
    raise SystemExit(f"release asset not available yet: {name}")
' "$1"
}

download_asset() { # $1 = asset name
  local name="$1"
  if ! require_asset "$name"; then
    echo "release $TAG is visible but $name is not attached yet" >&2
    return 1
  fi
  # This preserves authentication for private repositories and avoids a
  # second unauthenticated API request for browser_download_url.
  gh release download "$TAG" --repo "$REPO" --pattern "$name" --dir "$WORKDIR" --clobber
}

gh_verify() { # $1 = file, rest = extra gh attestation args
  local file="$1"; shift
  local args=(attestation verify "$file" --repo "$REPO"
    --signer-workflow "$WORKFLOW" --source-ref "$SOURCE_REF"
    --predicate-type "$PREDICATE" "$@")
  if [ -n "$TRUST_ROOT" ]; then
    args+=(--custom-trusted-root "$TRUST_ROOT")
  fi
  gh "${args[@]}"
}

step() { echo "==> $*"; }

step "1/5 manifest attestation (SLSA provenance, Sigstore, Rekor)"
download_asset "$MANIFEST_NAME"
# No --source-digest here: the manifest is authenticated before its claimed
# commit is trusted (mirrors the updater).
gh_verify "$WORKDIR/$MANIFEST_NAME"

step "2/5 manifest contents"
MANIFEST_JSON="$(cat "$WORKDIR/$MANIFEST_NAME")"
VERSION="$(printf '%s' "$MANIFEST_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')"
MTARGET="$(printf '%s' "$MANIFEST_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target"])')"
DIGEST="$(printf '%s' "$MANIFEST_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])')"
COMMIT="$(printf '%s' "$MANIFEST_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["commit"])')"
[ "$VERSION" = "$TAG" ] || { echo "manifest version $VERSION != tag $TAG" >&2; exit 1; }
[ "$MTARGET" = "$TARGET" ] || { echo "manifest target $MTARGET != $TARGET" >&2; exit 1; }
echo "version=$VERSION target=$MTARGET commit=$COMMIT"

step "3/5 binary digest"
download_asset "$BINARY_NAME"
ACTUAL="$(shasum -a 256 "$WORKDIR/$BINARY_NAME" | awk '{print $1}')"
[ "$ACTUAL" = "$DIGEST" ] || { echo "digest mismatch: $ACTUAL != $DIGEST" >&2; exit 1; }

step "4/5 Apple codesign requirement"
codesign --verify --strict --deep --verbose=2 -R="$REQUIREMENT" "$WORKDIR/$BINARY_NAME"
DETAIL="$(codesign -d --verbose=4 "$WORKDIR/$BINARY_NAME" 2>&1)"
printf '%s\n' "$DETAIL" | grep -q "Identifier=$IDENTIFIER"
printf '%s\n' "$DETAIL" | grep -q "TeamIdentifier=$TEAM_ID"

step "5/5 binary attestation (source digest pinned to manifest commit)"
gh_verify "$WORKDIR/$BINARY_NAME" --source-digest "$COMMIT"

echo "OK: $TAG trust chain verified"
