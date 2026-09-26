#!/bin/bash
# Relay launcher run in the macOS GUI session. It keeps per-task diagnostics
# beside the task payload so early wrapper failures are visible to dept.py.
set -u
mode="${1:?usage: codex-launch.sh run|resume <taskdir>}"
rdir="${2:?usage: codex-launch.sh run|resume <taskdir>}"
[ -f "$rdir/dir.txt" ] && [ -f "$rdir/prompt.txt" ] || exit 2
d="$(cat "$rdir/dir.txt")"
[ -d "$d" ] || exit 3
cd "$d" || exit 3
exec 2> "$rdir/stderr.log"
echo "codex-launch start $(date -u +%Y-%m-%dT%H:%M:%SZ) mode=$mode pid=$$" >&2

# macOS /bin/bash is 3.2: expanding an empty array under set -u is fatal.
MODEL_FLAG=""
if [ -f "$rdir/model.txt" ]; then
  MODEL_FLAG="-m $(cat "$rdir/model.txt")"
fi
CODEX="${CODEX:-/run/current-system/sw/bin/codex}"
if [ "$mode" = "resume" ]; then
  [ -f "$rdir/resume.txt" ] || exit 2
  sid="$(cat "$rdir/resume.txt")"
  exec "$CODEX" exec --json --approve-for-me --skip-git-repo-check \
    $MODEL_FLAG resume "$sid" "$(cat "$rdir/prompt.txt")" \
    -o "$rdir/last-message.txt" < /dev/null > "$rdir/events.jsonl"
else
  exec "$CODEX" exec --json --approve-for-me --skip-git-repo-check \
    $MODEL_FLAG -C "$d" -o "$rdir/last-message.txt" "$(cat "$rdir/prompt.txt")" \
    < /dev/null > "$rdir/events.jsonl"
fi
