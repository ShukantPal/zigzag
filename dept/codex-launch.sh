#!/bin/bash
# Relay launcher run in the macOS GUI session. It keeps per-task diagnostics
# beside the task payload so early wrapper failures are visible to dept.py.
set -u
mode="${1:?usage: codex-launch.sh run|resume <taskdir>}"
rdir="${2:?usage: codex-launch.sh run|resume <taskdir>}"
# Start capturing before any payload validation.  If a bad or incomplete task
# payload reaches the relay, its wrapper error must still be inspectable via
# `dept.py result`, rather than disappearing into the relay's transient buffer.
exec 2> "$rdir/stderr.log"
echo "codex-launch start $(date -u +%Y-%m-%dT%H:%M:%SZ) mode=$mode pid=$$" >&2
[ -f "$rdir/dir.txt" ] && [ -f "$rdir/prompt.txt" ] || exit 2
d="$(cat "$rdir/dir.txt")"
[ -d "$d" ] || exit 3
cd "$d" || exit 3

CODEX="${CODEX:-/run/current-system/sw/bin/codex}"
if [ -f "$rdir/read-only.txt" ]; then
  set -- exec --json --sandbox read-only --skip-git-repo-check
else
  set -- exec --json --approve-for-me --skip-git-repo-check
fi
if [ -f "$rdir/model.txt" ]; then
  model="$(cat "$rdir/model.txt")"
  case "$model" in
    ""|-*|*[!A-Za-z0-9._:/-]*) echo "invalid model identifier" >&2; exit 2 ;;
  esac
  set -- "$@" -m "$model"
fi
if [ "$mode" = "resume" ]; then
  [ -f "$rdir/resume.txt" ] || exit 2
  sid="$(cat "$rdir/resume.txt")"
  exec "$CODEX" "$@" resume "$sid" "$(cat "$rdir/prompt.txt")" \
    -o "$rdir/last-message.txt" < /dev/null > "$rdir/events.jsonl"
else
  exec "$CODEX" "$@" -C "$d" -o "$rdir/last-message.txt" "$(cat "$rdir/prompt.txt")" \
    < /dev/null > "$rdir/events.jsonl"
fi
