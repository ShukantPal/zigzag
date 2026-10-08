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
  `start` and `resume` accept `--model <name>` for a per-task override and
  `--read-only` for a sandboxed task without approval bypass.
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
  simplicity, tests) each showing APPROVE on that head, plus a formal
  current-head approval from a separately authenticated human reviewer.
  Stale-head, lower-round verdicts and shared-automation approvals don't count.
- **`pr_comment_watcher.py`** — stateless poller (runs on a 5-minute cron):
  watches listed PRs for new review comments / review bodies from Shukant
  and resumes the PR's owning worker session to address them. Watermark in
  the configured runtime state directory.
- **`gdocs_comment_watcher.py`** — Mac-side Drive watcher for Google Docs:
  lists the shared folder plus the individually shared document, acknowledges
  new feedback with a marked reply (the Drive API has no emoji reactions), and
  resumes the document's owning session from a Drive-scoped service-account
  token minted with the GUI-login keychain key.
- **`review_round_watcher.py`** — validates and publishes constrained verdicts
  from a seeded 3-lens review round. It never forwards raw reviewer output into
  a write-capable owner session; model verdicts are marked advisory so an
  injected reviewer cannot manufacture a gate-satisfying approval. A distinct
  actor listed in `approval_gate.human_review_actors` must submit a formal
  GitHub approval after inspecting the findings.
- **`dispatch_review_round.py`** — seeds independent read-only correctness,
  simplicity, and tests reviewers from the checked-out diff over non-GUI SSH
  (no keychain). PR metadata is excluded from their prompts and trusted watcher
  code publishes only validated verdict fields.
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
`resume <task-dir>`. The Google Docs watcher runs on the logged-in Mac and
needs the provisioned `zigzag-sa` keychain item; it uses the `drive` OAuth
scope directly, with no Google Workspace CLI dependency. Its `owners`
configuration maps a discovered document to its owning project and Codex
session; it is ownership metadata, not the watched-document list. Docs in the
shared folder are discovered automatically. Drive watcher state lives in
`~/.zigzag/dept/dept.db` by default and it exits silently during 22:00–07:00
PT quiet hours (use `--force` only for an intentional manual poll).

Before enabling automation, run the offline checks from the repository root:

```
python3 -m unittest discover -s dept -p 'test_*.py'
python3 -c "import pathlib; [compile(p.read_text(), str(p), 'exec') for p in pathlib.Path('dept').glob('*.py')]"
```

Use `--help`/`--dry-run` where available, seed watcher watermarks before their
first live poll, and verify one `dept.py status --once` call against the
configured relay before dispatching real work. `dept.py status TASK_ID` keeps
the manager's per-task lookup behavior. Configure the Drive watcher's stable
`trusted_author_permission_ids` (preferred) or `trusted_author_emails`
before enabling it; display names are never trusted task input.

## Scheduled deployment

Run the PR, Jules, and review-round watchers on the configured VM/automation
host. The Google Docs watcher is different: install it as a per-user Mac
LaunchAgent in the GUI login session, where the service-account keychain item
is available. It owns its SQLite state at `~/.zigzag/dept/dept.db` and exits
during quiet hours. Do not deploy it on the VM and do not configure a Workspace
CLI for it.

VM cron entries use absolute paths and durable logs, for example:

```
*/5 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/pr_comment_watcher.py >>/var/log/codex-dept/pr-watcher.log 2>&1
*/10 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/jules_pr_reviewer.py >>/var/log/codex-dept/jules-watcher.log 2>&1
*/10 * * * * cd /absolute/path/to/zigzag && /usr/bin/python3 dept/review_round_watcher.py >>/var/log/codex-dept/review-rounds.log 2>&1
```

Use a log directory writable by the cron account (or replace it with the
platform's job logger). Export `HTTPS_PROXY`, `CODEX_DEPT_CONFIG`, and any
credential environment in the cron service environment; cron does not inherit
an interactive shell's setup.
