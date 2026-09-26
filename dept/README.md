# Department status

`python3 dept/dept.py status` is a read-only live view of Zigzag execution
state. It reads the relay's `GET /v1/agents?state=running` and cursor event
endpoint, plus the Mac-local durable audit directory beside `events.json`.
It never invokes a control endpoint or reads agent output.

Use `--once` for a non-interactive snapshot. The full-screen view refreshes on
its interval and accepts `↑`/`↓` (or `j`/`k`) to select an execution and `q` to
quit. `*` beside an observed total and the detail-pane marker both mean a
clock boundary was encountered: the display shows timestamps but does not
invent a Mac/VM transit duration. Missing pairs read **not observed**.

The Phase 2 base contains no department scheduling, review, or PR-watch
scripts to own review/human transition emission. `events.py` supplies the
schema-v1, idempotent event helper for those owners when they are added to
this branch; no synthetic hooks are introduced here.
