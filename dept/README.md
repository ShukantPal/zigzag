# Codex department tooling (`dept/`)

This directory holds the tooling that runs Shukant's Codex engineering
department: a fleet of `codex exec` agents on his MacBook Pro, driven from
Muse's VM over Tailscale SSH (and, for GUI-session work, through the zigzag
relay's `/v1/spawn` endpoint and `/v1/proc/<handle>` polling).

It is versioned here — inside the zigzag repo — because this is the repo for
Muse-owned agent infrastructure. It is deliberately a separate system from
the relay itself: the relay executes commands on the Mac; `dept/` is the
management layer that decides what to run.

## Scripts

- **`dept.py`** — the department manager. `start` launches a tracked task
  (detached `codex exec` on the Mac, state in `~/.codex/dept/<task-id>/`);
  `resume` continues an existing Codex session (e.g. one started in the
  Codex Desktop app) with a new prompt; `status` / `list` / `result` /
  `tokens` / `check` / `kill` inspect and manage tasks. Task state is kept
  in the configured state directory (local runtime state, gitignored).
  `start` and `resume` accept `--model <name>` for a per-task override.
- **`codex-launch.sh`** — the Mac GUI-session relay wrapper. It reads each
  task's optional `model.txt` override and writes wrapper diagnostics to the
  task directory before invoking `codex exec`.
- **`dept.py status`** — with no task id (or with status-view options), this
  is the read-only live view of Zigzag execution state. It reads the relay's
  `GET /v1/agents?state=running` and cursor event endpoint plus the Mac-local
  durable audit directory beside `events.json`; it never invokes a control
  endpoint or reads agent output. Use `--once` for a non-interactive snapshot.
- **`approval_gate.py`** — the merge gate for Muse-owned PRs: required CI
  green on the latest head **and** a 3-lens review team (correctness,
  simplicity, tests) each showing APPROVE on that head. Stale-head approvals
  don't count.
- **`pr_comment_watcher.py`** — stateless poller (runs on a 5-minute cron):
  watches listed PRs for new review comments / review bodies from Shukant
  and resumes the PR's owning worker session to address them. Watermark in
  the configured runtime state directory.
- **`gdocs_comment_watcher.py`** — same idea for Google Docs comments:
  acknowledges with a marked reply (the Drive API has no emoji reactions)
  and resumes the doc's owning session.
- **`review_round_watcher.py`** — when a seeded 3-lens review round finishes,
  resumes the owning worker session with the reviewers' findings batched.
- **`dispatch_review_round.py`** — seeds the independent correctness,
  simplicity, and tests reviewers for a PR; their prompts include the PR body
  as design rationale and use explicit repository scoping.
- **`jules_pr_reviewer.py`** — dispatches a Codex review task for each new
  Jules-authored PR in `leveled-inc/leveled` (Jules owns revisions there;
  Codex reviews only).

## Docs

- **`sop.md`** — the standard operating procedure prepended to every code
  task: branch, commit, push, open PR, spawn subagent reviews, address
  findings, get CI green, report. (Bypass with `dept.py --no-sop`.)
- **`writing.md`** — the writing standard prepended for research/writing
  tasks (`--writing`): strategic, hierarchical, verified, shareable.
- **`relay-announce.md`** — reference for the relay's completion-announce
  protocol.

## Install, configuration, and smoke tests

The checked-in `dept/` directory is the tooling root: scripts locate their
SOP, announcement text, manager, and default state relative to `__file__`,
not through `~/workspace/codex-dept`. Clone this repository on the execution
host and run scripts from that checkout. No symlink to an older checkout is
needed.

Copy `config.example.json` to ignored `config.json` (or set
`CODEX_DEPT_CONFIG` to an absolute path) and fill in the deployment values.
`state_dir` may instead be overridden by `CODEX_DEPT_STATE_DIR`; when neither
is set it is `dept/runtime/`. It contains `ledger.jsonl`, `reported.json`,
`pr_sessions.json`, `prompts/`, `task-prompts/`, `review_rounds/`, and watcher
watermarks. All are runtime data and are ignored by Git.

The host running `dept.py`, the PR watcher, and the review-round watcher needs
GitHub CLI authentication, `HTTPS_PROXY`, the configured Tailscale proxy helper
and SSH key, plus reachability to the configured Mac. The relay path also
needs a readable Zigzag bearer token and a relay allowlist entry for the
configured launcher (normally `codex-launch`); that launcher must be installed
on the Mac GUI login session and understand `run <task-dir>` and
`resume <task-dir>`. The Google Docs watcher additionally needs its configured
Google Workspace CLI. PR/Doc lists, projects, session mappings, and the Jules
batch mapping are deliberately deployment configuration, not source code.

Before enabling automation, run the offline checks from the repository root:

```
python3 -m unittest discover -s dept -p 'test_*.py'
python3 -c "import pathlib; [compile(p.read_text(), str(p), 'exec') for p in pathlib.Path('dept').glob('*.py')]"
```

Use `--help`/`--dry-run` where available, seed watcher watermarks before their
first live poll, and verify one `dept.py status --once` call against the
configured relay before dispatching real work. `dept.py status TASK_ID` keeps
the manager's per-task lookup behavior.

## Cron deployment

Run the PR, Google Docs, Jules, and review-round watchers on the configured
VM/automation host, not on the target Mac: that host owns the state directory,
SSH/GitHub credentials, and (for Docs) the Workspace CLI. Install cron entries
with absolute paths and route both streams to durable logs, for example:

```
*/5 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/pr_comment_watcher.py >>/var/log/codex-dept/pr-watcher.log 2>&1
*/5 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/gdocs_comment_watcher.py >>/var/log/codex-dept/gdocs-watcher.log 2>&1
*/10 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/jules_pr_reviewer.py >>/var/log/codex-dept/jules-watcher.log 2>&1
*/10 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/review_round_watcher.py >>/var/log/codex-dept/review-rounds.log 2>&1
```

Use a log directory writable by the cron account (or replace it with the
platform's job logger). Export `HTTPS_PROXY`, `CODEX_DEPT_CONFIG`, and any
credential environment in the cron service environment; cron does not inherit
an interactive shell's setup.
