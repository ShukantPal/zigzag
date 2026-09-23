# Codex department tooling (`dept/`)

This directory holds the tooling that runs Shukant's Codex engineering
department: a fleet of `codex exec` agents on his MacBook Pro, driven from
Muse's VM over Tailscale SSH (and, for GUI-session work, through the Zigzag
relay).

## Commands

- **`dept.py`** — the department manager. `start` launches a tracked task;
  `resume` continues an existing Codex session; `status TASK_ID`, `list`,
  `result`, `tokens`, `check`, and `kill` inspect and manage tasks. Task state
  is kept in `ledger.jsonl` beside the deployed script (local runtime state,
  gitignored).
- **`dept.py status`** — a read-only live view of Zigzag execution state.
  With no task id (or with status-view options) it reads the relay's
  `GET /v1/agents?state=running` and cursor event endpoint plus the Mac-local
  durable audit directory beside `events.json`. It never invokes a control
  endpoint or reads agent output. Use `--once` for a non-interactive snapshot.
- **`approval_gate.py`** — the merge gate for Muse-owned PRs: required CI
  green on the latest head and a current APPROVE from each required review
  lens.
- **`pr_comment_watcher.py`**, **`gdocs_comment_watcher.py`**, and
  **`review_round_watcher.py`** — stateless pollers that route human feedback
  and completed review rounds back to the owning task.
- **`jules_pr_reviewer.py`** — dispatches Codex review tasks for new
  Jules-authored PRs in `leveled-inc/leveled`.

## Read-only status view

`python3 dept/dept.py status` is a read-only live view of Zigzag execution
state. It reads the relay's `GET /v1/agents?state=running` and cursor event
endpoint, plus the Mac-local durable audit directory beside `events.json`.
It never invokes a control endpoint or reads agent output.

Use `--once` for a non-interactive snapshot. The full-screen view refreshes on
its interval and accepts `↑`/`↓` (or `j`/`k`) to select an execution and `q` to
quit. `*` beside an observed total and the detail-pane marker both mean a
clock boundary was encountered: the display shows timestamps but does not
invent a Mac/VM transit duration. Missing pairs read **not observed**.


`events.py` supplies the schema-v1, idempotent event helper used by department
transition owners; the status view itself remains observational only.

## Deployment and runtime state

`sop.md`, `writing.md`, and `relay-announce.md` are the standard task-prompt
components. `ledger.jsonl`, `reported.json`, `pr_sessions.json`, `prompts/`,
`task-prompts/`, and `review_rounds/` are per-deployment runtime state and are
gitignored.
