#!/bin/bash
# Data-only launcher for daemon-owned PR reviewers. Unlike codex-launch, this
# has no resume mode and exposes no agent tools or user integrations. The
# daemon validates the structured result before performing the separate,
# trusted GitHub comment publication step.
set -euo pipefail
rdir="${2:?usage: codex-review-launch.sh run <taskdir>}"
[ "${1:-}" = "run" ] || exit 2
d="$(cat "$rdir/dir.txt")"
[ -d "$d" ] || exit 3
cd "$d" || exit 3
exec 2> "$rdir/stderr.log"
echo "codex-review-launch start $(date -u +%Y-%m-%dT%H:%M:%SZ) pid=$$" >&2
CODEX=/run/current-system/sw/bin/codex
{
  cat "$rdir/prompt.txt"
  printf '\n\n<untrusted_patch_json>\n'
  cat "$d/review-material.json"
  printf '\n</untrusted_patch_json>\n'
} | env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
  "$CODEX" --sandbox read-only --ask-for-approval never exec --json \
    --ignore-user-config \
    --ignore-rules \
    --ephemeral \
    --skip-git-repo-check \
    --disable shell_tool \
    --disable apps \
    --disable skill_mcp_dependency_install \
    -c 'web_search="disabled"' \
    -c shell_environment_policy.inherit=none \
    --output-schema "$rdir/result-schema.json" \
    -C "$d" \
    -o "$rdir/last-message.txt" \
    - > "$rdir/events.jsonl"
