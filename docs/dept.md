# Python department tooling

`dept/config.py` is versioned declarative configuration and materializes
`dept/config.materialized.json`; CI requires it to be fresh.
`dept/dept_config.py` reads ignored deployment-local `config.json` (or
`CODEX_DEPT_CONFIG`) for SSH, proxy, and state-directory settings. Its state
root is configured `state_dir`, then `CODEX_DEPT_STATE_DIR`, then `dept/runtime`.

## `dept.py`: manager and dispatcher

These status forms are intentionally different:

```text
python3 dept/dept.py status [--once ...]       # no task ID: read-only TUI
python3 dept/dept.py status t-abcdef           # manager liveness/exit
```

Commands are `start`, `resume`, `status TASK`, `list`, `result TASK`,
`tokens TASK`, `check`, and `kill TASK`. `start PROJECT PROMPT_FILE` and
`resume [PROJECT] SESSION_ID PROMPT_FILE` accept `--no-sop`, `--writing`,
`--ssh`, `--model MODEL`, `--task-id t-xxxxxx`, and `--read-only`.

Code tasks use `dept/sop.md` by default; `--writing` uses `writing.md` and
`--no-sop` uses neither. Setup writes prompt/project and optional
resume/model/read-only markers under `~/.codex/dept/t-xxxxxx` over SSH.
Normal launch is allowlisted `codex-launch run|resume TASK_DIR`. `--ssh`
bypasses the relay and GUI-Keychain path, records PID/exit state itself, and
is used for read-only isolated reviewers.

Use `result` first for stuck or completed jobs: it presents final output,
relay/remote stderr tail, and token summary. `check` is cron-oriented and
records reported completions in `reported.json`; `kill` follows the recorded
relay or SSH transport.

## `status.py`

The read-only execution UI merges relay audit JSONL, `GET /v1/events`, and
agent snapshots. It never calls a control route.

```text
python3 dept/status.py [--url URL] [--state-file PATH] [--token-file PATH]
                       [--interval SECONDS] [--once] [--all]
```

Defaults are the Mac `~/.codex/zigzag/events.json` and token. `--once` is
script-friendly. Interactively: arrows select, left/right or `h`/`l` scroll,
Enter/`t` opens transcript/output, and `q` exits. Loss, audit/log degradation,
and cross-clock warnings are diagnostic evidence.

## Watchers and supporting scripts

| Script | Role |
| --- | --- |
| `approval_gate.py OWNER/REPO PR` | Queries authoritative relay review-gate result; does not recreate policy. |
| `dispatch_review_round.py` | Seeds read-only reviewer lenses from an immutable commit/diff snapshot. |
| `review_round_watcher.py` | Collects exact-head verdicts; invalid results become `attention`. |
| `pr_comment_watcher.py` | VM poller for configured trusted-human PR feedback; resumes owner tasks safely. |
| `gdocs_comment_watcher.py` | GUI-session Mac Google Drive poller; stores SQLite state at `~/.zigzag/dept/dept.db`. |
| `jules_pr_reviewer.py` | Polls configured Jules PRs and dispatches/re-dispatches a review task; never merges/pushes. |
| `events.py` | Builds/posts stable schema-v1 `review_*` and `human_wait_*` events. |
| `codex-launch.sh` | Relay allowlist launch wrapper for manager-owned work. |

Use VM cron (normally 5–10 minute cadence) with absolute paths, configured
proxy/environment, and durable logs for PR, Jules, and review-round watchers.
The Google Docs watcher is a Mac GUI-session LaunchAgent, not VM cron. See
`dept/README.md` for cron examples.

## The shell `zigzag` wrapper

`~/.local/bin/zigzag` is not the Rust binary and may shadow it in `PATH`.
Bare `zigzag`/`zigzag status` invokes `dept/status.py`; `zigzag prs` runs
`zigzag-prs`; and its `zigzag timeline` arm is stale because current `dept.py`
has no timeline command. Invoke the release `zigzag` binary explicitly for
timeline or update controls.
