# Announcing completion via Zigzag (best effort)

Your department task id is **{{TASK_ID}}**.

When the task is fully done — work complete, PR open, CI green, final report
written — announce it through the local Zigzag relay so the orchestrator hears
about it immediately instead of at the next status poll. Best-effort only: if
the relay isn't reachable, finish normally; never fail the task over this.

Run exactly this, replacing SUMMARY with your one-line outcome:

```sh
TASK_ID="{{TASK_ID}}" SUMMARY="one-line outcome, e.g. PR #57 opened and CI green" python3 - <<'PYEOF'
import json, os, sys, urllib.request
tid = os.environ["TASK_ID"]
token_path = os.path.expanduser("~/.codex/zigzag/zigzag.token")
if not os.path.exists(token_path):
    sys.exit(0)
token = open(token_path).read().strip()
body = json.dumps({
    "id": f"{tid}-done",
    "task_id": tid,
    "summary": os.environ["SUMMARY"],
}).encode()
req = urllib.request.Request(
    "http://127.0.0.1:8765/v1/events", data=body,
    headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    method="POST")
try:
    urllib.request.urlopen(req, timeout=10).read()
except Exception:
    pass
PYEOF
```

The event `id` is `{task_id}-done`, so re-announcing is idempotent — safe to
run once at the very end, after the final report is written.
