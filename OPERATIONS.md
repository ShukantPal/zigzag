# Zigzag: comprehensive architecture and operations reference

> This file is the comprehensive, single-document operational reference requested for PR #70.
> The focused files under `docs/` are useful entry points; this file remains the exhaustive reference.


This is the operating manual for an agent working on Zigzag. It describes the code that exists at this revision, the deployed Mac layout, and the older VM department tooling that still participates in the system. It deliberately distinguishes those two worlds: the Rust relay is Mac-local and authoritative for relay state; much of `dept/` is VM/automation-host tooling which reaches the Mac over Tailscale/SSH or the relay API.

The two checkout spellings used on this machine, `/Users/shukant/Workspace/ShukantPal/zigzag` and `/Users/shukant/Workspace/shukantpal/zigzag`, resolve to the same directory. Use the capitalized canonical spelling in new operational commands.

## First five minutes

Do these read-only checks before trying to restart, kill, or re-dispatch anything:

```sh
# Mac: LaunchAgent registration and recent daemon diagnostics
launchctl print "gui/$(id -u)/com.shukantpal.zigzag"
tail -n 100 ~/.codex/zigzag/zigzag.error.log

# Mac or a host with the relay token: authenticated liveness and live agents
zzapi health
zzapi agents list --state running

# Mac status view: durable audit history plus live relay state
python3 dept/status.py --once

# VM/department-manager task lookup (requires a task id)
python3 dept/dept.py status t-abcdef
python3 dept/dept.py result t-abcdef
```

`launchctl print` says whether macOS is supervising the process. `zzapi health` says whether the HTTP service is responding with the correct bearer secret. They are intentionally different checks. Do not print or copy the token in `~/.codex/zigzag/zigzag.token`; it grants access to all relay logs and normal API routes.

## System in one picture

```text
VM / automation host                         Mac GUI login session
---------------------                        ---------------------
dept.py / watchers -- SSH -----------------> ~/.codex/dept/t-*/
       |                                         | codex-launch.sh
       | HTTP (Tailscale, bearer token)           v
       +---------------------------------> zigzag relay (LaunchAgent)
                                             |  event store + audit archive
                                             |  agent registry + log spools
                                             +--> Codex process groups/worktrees

status.py <----- events + agents + audit ------+
zzapi ---------- all supported relay routes ---+
```

There are two ways to create work:

1. `POST /v1/agents` (normally `zzapi agents create`) is the newer, relay-native path. The relay creates/uses a permitted worktree, starts a fixed Codex provider command, persists an agent record, captures logs, and persists a JSONL transcript for API-created agents.
2. `dept.py start` / `resume` is the department-manager path. It first writes a task directory on the Mac under `~/.codex/dept/<task-id>/`. By default it asks `POST /v1/spawn` to run the allowlisted `codex-launch`; with `--ssh` it starts `codex exec` over SSH plus `nohup` instead. It records the attempt in the department ledger on the host where `dept.py` ran.

The paths have different identifiers. A department `t-xxxxxx` is a logical task ID; a relay spawn returns a `proc`, which is also the relay agent ID during the compatibility migration; an `execution_id` identifies one concrete attempt. Do not assume retrying the same task creates the same execution.

## Repository map

| Location | Entry point / role |
| --- | --- |
| `zzd/src/main.rs` | `zigzag` daemon: config/control subcommands or HTTP relay server. |
| `zzd/src/{server,http,auth,exec,proc,events}.rs` | HTTP dispatch/authentication, restricted execution, process supervision, durable event/audit lifecycle. |
| `zzd/src/routes/` | Implemented API route handlers for agents, events, exec/spawn, procs, providers, and worktrees. |
| `zzd/src/{github,review_loop,provider,session,update}.rs` | Legacy GitHub watch, Mac review loop, Codex/OpenCode providers, GUI/Tailscale checks, signed self-update. |
| `zz/` | Shared durable JSON store, agent registry, JSON parser, and secret-file support. |
| `zzapi/src/main.rs` | `zzapi`, a typed relay REST client. |
| `dept/` | Python department manager, status UI, event helpers, policy/config, and scheduled watchers. |
| `launchd/` | LaunchAgent template and installation/cutover instructions. |
| `scripts/` | Release signing/verification, hook installation, reviewer launcher, and full-stack E2E script. |
| `openapi.yaml` | Useful API contract, but not the complete implementation inventory; see the endpoint table below. |

## Relay daemon

### Launch and security boundary

The deployed daemon is a per-user **LaunchAgent**, not a system daemon: `com.shukantpal.zigzag`. Its installed plist normally runs the release binary from the canonical checkout with at least `--secret-file` and `--state-file`, keeps it alive, writes stdout to `~/.codex/zigzag/zigzag.log`, and writes stderr to `~/.codex/zigzag/zigzag.error.log`. The checked-in template is `launchd/com.shukantpal.zigzag.plist`; use `launchd/INSTALL.md` for installation or review-loop cutover, rather than inventing another launchd job.

The daemon binds port 8765 on both `127.0.0.1` and the Mac Tailscale IPv4 address. Every HTTP route requires `Authorization: Bearer <relay token>` except that the legacy generic kill route requires a separate control secret and is absent when that secret was not configured. Tailscale ACLs must still limit who can reach port 8765. HTTP is intentionally plain HTTP inside that network boundary; an approved proxy supplies orchestrator-facing TLS when used.

The daemon loads its executable allowlist only from a GUI login session because it is stored through macOS Keychain. Consequently, do not bootstrap/restart this service through an SSH session: Keychain access can hang and `/v1/exec` will stop working even if the process exists. Run the documented release script from an interactive Mac terminal.

### Server arguments and subcommands

The normal server invocation requires `--secret-file PATH` (or `ZIGZAG_SECRET_FILE`) and `--state-file PATH` (or `ZIGZAG_STATE_FILE`).

| Option | Meaning |
| --- | --- |
| `--control-secret-file PATH` | Enables legacy `POST /v1/proc/{id}/kill` with this distinct secret. |
| `--port N` | Port; defaults to 8765. |
| `--tailscale-ip IP` | Test override; must be a `100.64.0.0/10` IPv4 address. Otherwise the daemon asks `tailscale ip -4`. |
| `--max-events N` | Bounded live-event retention; default 1000 and must be positive. |
| `--watch-repo OWNER/REPO`, `--watch-interval S` | Legacy GitHub PR watch; interval is 30–3600 seconds. It starts only while the Rust review loop is not authoritative. |
| `--update-dir PATH`, `--update-interval S`, `--update-policy enabled|paused|pin:VERSION` | Signed relay-update controls. Interval 0 disables scheduled checks. |
| `--update-ready-file PATH` | Internal replacement/watchdog readiness handoff. |

The same binary also has these operational subcommands:

```sh
zigzag config get-allowlist
zigzag config set-allowlist --file PATH
zigzag timeline TASK_ID --state-file ~/.codex/zigzag/events.json
zigzag updates --dir ~/.codex/zigzag/relay status|pause|pin VERSION|unpin
```

`config` requires the GUI session and changes the execution policy; treat it as a privileged Mac-only operation. `timeline` reads persisted event/audit state; it does not query the network. `update-watchdog` is an internal subcommand, not an operator interface.

### Relay-native agents and worktrees

The relay agent path is intentionally narrower than `/v1/exec`: callers select a provider-level request, not arbitrary binary/arguments. `POST /v1/agents` accepts a prompt (inline or file path), `project_dir`, and one of `branch`, `no_branch`, or `pr`, plus optional `worktree`, `model`, `approval_mode`, and `timeout_secs`. Branch and PR modes run in a supervised worktree (default under `~/.zigzag/worktrees/<branch-slug>/`). Set `ZIGZAG_WORKTREE_BASE` to choose another default base; `ZIGZAG_WORKTREE_ROOTS` sets the allowed roots and, when used alone, its first root becomes the default base. No-branch mode runs directly in `project_dir` without creating a worktree or checking out a branch; the CLI requires explicit `--no-branch` to select it. Capacity is bounded (default 16 agents).

The registry reaper records terminal exit. On relay restart, the daemon verifies each live agent's recorded PID birth identity and continues reporting verified processes as `running`; it tails their durable output files and records an unexpected exit if the process later disappears. A dead former group becomes `lost_after_restart`. Terminal registry entries and spool metadata are pruned after seven days. The generic spawn compatibility path does not create the same full transcript file; API-created agents do.

`POST /v1/worktrees` and `DELETE /v1/worktrees` are explicit worktree helpers. They canonicalize paths, restrict them to approved roots (by default `~/.zigzag/worktrees` or `/Users/shukant/.codex/worktrees`), reject traversal/unusable paths, and refuse to remove a worktree used by a live agent. `ZIGZAG_WORKTREE_ROOTS` replaces the allowed-root list. Stopping an agent leaves its worktree; cleanup is a separate, deliberate action.

### Event and completion model

The live store in `events.json` is durable but bounded. Posting the same `id` is idempotent only while that event is retained; consumers must deduplicate. `GET /v1/events` returns `epoch`, `reset`, `lost`, `events`, and `next`. Consumers pass `next` as `after` and echo `epoch`; `reset` means the store was replaced and `lost` means the requested sequence was evicted.

Schema-v1 lifecycle facts use `id`, `task_id`, `execution_id`, `kind`, `source`, `occurred_at`, `clock`, and object `payload`. Accepted sources are `vm-department`, `mac-relay`, and `vm-poller`. Facts with an execution ID are also appended to per-execution JSONL audit logs in `events.audit/`; that archive is independent from live retention, capped at 20 MiB total, and removes oldest execution logs first. It excludes prompts, command arguments, and raw output.

Relay events such as spawn/exit plus department transition events let `status.py` project execution phase. A task is not completed merely because its event appeared: use the relay agent/proc exit state or `dept.py status` for liveness, then inspect final output. The optional completion announcement in `dept/relay-announce.md` posts `<task-id>-done` best-effort after the agent has actually finished; it is a notification, not an authoritative exit record.

## HTTP endpoint reference

All entries below are authenticated with the relay bearer token unless noted. Use `openapi.yaml` for wire schemas, but trust `zzd/src/routes/` for routes introduced after the specification: the current implementation includes `/v1/providers` and `/v1/agents/{id}/transcript`, which the OpenAPI file does not yet enumerate.

| Method and path | Purpose / key inputs |
| --- | --- |
| `GET /v1/health` | Authenticated liveness: `{"status":"ok"}`. |
| `POST /v1/events` | Persist JSON object with non-empty `id`; 201 new, 200 duplicate. |
| `GET /v1/events?after=&epoch=&timeout=` | Long-poll event feed; timeout is 0–55 seconds (server default 50). Preserve `epoch` and `next`. |
| `POST /v1/exec` | Run an allowlisted command synchronously: `{id,bin,args}`. Denials deliberately return an opaque `{"error":"denied"}` response. |
| `POST /v1/spawn` | Start an allowlisted background process: exec fields plus optional safe `execution_id`; returns `{id,proc}`. During updates it can return 503 while draining. |
| `GET /v1/proc/{id}` | Compatibility poll for a spawned process: running, exit code, captured output/truncation. |
| `POST /v1/proc/{id}/kill` | Generic process-group kill. Requires the **control** secret, and is 404 when control is disabled. Prefer agent stop for agents. |
| `GET /v1/agents?state=&task_id=` | List durable agent records, optionally filtered. |
| `POST /v1/agents` | Relay-native Codex create/start; see above. |
| `GET /v1/agents/{id}` | Durable agent lifecycle record. |
| `DELETE /v1/agents/{id}` | Graceful SIGTERM then SIGKILL if needed; deregisters agent but leaves worktree. |
| `POST /v1/agents/{id}/pause` / `resume` | SIGSTOP/SIGCONT the process group. Paused state is reapplied where possible after restart. |
| `GET /v1/agents/{id}/logs?stream=stdout|stderr|both&after=&tail=&follow=0|1` | Bounded, redacted diagnostic spools. Use returned `next_cursor`; if `dropped_before` exceeds your cursor, earlier output is gone. `follow=1` long-polls. |
| `GET /v1/agents/{id}/transcript?tail=` | API-created Codex JSONL transcript rendered with metadata/prompt. It is not available for every generic spawned process. This is the `/transcript` facility; there is no top-level `/transcript`. |
| `GET /v1/providers` | List installed/available agent providers and their supported model choices. |
| `POST` / `DELETE /v1/worktrees` | Create `{path,branch,repo}` or remove `{path}` subject to approved-root and live-agent checks. |
| `GET /v1/review-gate?repository=OWNER%2FREPO&pull_request=N` | Daemon-side review/CI/human-approval decision based on `~/.zigzag/config.yaml` policy. |

## Command-line surfaces

### `zzapi`

`zzapi` is the portable Rust client for the relay. It is the best normal API diagnostic client because it handles authentication, request construction, tables, unique agent-ID prefixes, and relay/proxy configuration. Build it with `cargo build -p zzapi` or use the installed release binary.

Configuration precedence: `--hostname` / `ZIGZAG_HOSTNAME` (default `100.101.237.83:8765`), `ZIGZAG_TOKEN` or `--token-file` / `ZIGZAG_TOKEN_FILE`, and optional `ZIGZAG_PROXY`. Without an explicit file it tries `~/.codex/zigzag/zigzag.token` then `~/.codex/zigzag.token`; a token file must be current-user-owned and mode 0600. Global `--json` prints raw JSON.

```text
zzapi health
zzapi agents list [--state running] [--task-id TASK]
zzapi agents get|pause|resume|stop ID
zzapi agents create --prompt TEXT --project-dir DIR
                    (--branch BRANCH | --no-branch | --pr NUMBER)
                    [--worktree PATH] [--model MODEL]
                    [--approval-mode MODE] [--timeout-secs N]
zzapi agents logs ID [--stream stdout|stderr|both] [--after N] [--tail N]
                    [--follow] [--prefix]
zzapi worktrees create --path PATH --branch BRANCH --repo DIR
zzapi worktrees delete --path PATH
zzapi exec --bin BIN [--id ID] --args ...
zzapi spawn --bin BIN [--id ID] [--execution-id ID] --args ...
zzapi proc get ID
zzapi events [--after N] [--epoch E] [--timeout N] [--follow]
zzapi review-gate --repo OWNER/REPO --pr N
```

It exposes only the listed common route set; use an authenticated HTTP client for provider or transcript endpoints until the CLI grows corresponding verbs.

### The installed `~/.local/bin/zigzag` wrapper

This is a **different shell command** from the Rust `zigzag` binary and it shadows the binary when `~/.local/bin` comes first in `PATH`. Its exact current dispatch is:

| Wrapper command | Runs |
| --- | --- |
| `zigzag status [flags]` (or bare `zigzag`) | `python3 .../dept/status.py` |
| `zigzag timeline TASK` | `python3 .../dept/dept.py timeline TASK` |
| `zigzag prs [flags]` | `~/.local/bin/zigzag-prs` |

`zigzag-prs` lists configured cross-repository PRs using `gh`, CI status, mergeability, and the optional local activity file. The wrapper's `timeline` arm is stale: current `dept.py` has no `timeline` manager command. It will report `unknown command: timeline`. To use the implemented Rust timeline or update controls, invoke the real binary explicitly (for example `/Users/shukant/Workspace/ShukantPal/zigzag/target/release/zigzag timeline …`) or correct the wrapper in a separate change. Do not confuse a successful `zigzag status` with relay-binary health; it starts the Python UI.

## Python department tooling

`dept/config.py` is versioned declarative configuration and materializes `dept/config.materialized.json`; CI checks byte-for-byte freshness. `dept/dept_config.py` reads deployment-local ignored `config.json` (or `CODEX_DEPT_CONFIG`) and produces the SSH/proxy/state-dir configuration. Its state root is `state_dir` in that config, `CODEX_DEPT_STATE_DIR`, or finally `dept/runtime`.

### `dept.py`: manager and dispatcher

The manager has two intentionally distinct status meanings:

```text
python3 dept/dept.py status [--once ...]       # no task id: read-only TUI
python3 dept/dept.py status t-abcdef           # task manager liveness/exit
```

Manager commands are `start`, `resume`, `status TASK`, `list`, `result TASK`, `tokens TASK`, `check`, and `kill TASK`. `start PROJECT PROMPT_FILE` and `resume [PROJECT] SESSION_ID PROMPT_FILE` accept `--no-sop`, `--writing`, `--ssh`, `--model MODEL`, `--task-id t-xxxxxx`, and `--read-only`.

By default a code task is decorated with `dept/sop.md`; `--writing` uses `writing.md` instead; `--no-sop` avoids both. Task setup writes prompt, project, optional resume/model/read-only markers to `~/.codex/dept/t-xxxxxx` over SSH before launch. The normal relay launch is `codex-launch run|resume TASK_DIR`, so the relay allowlist must include that launcher. `--ssh` bypasses the relay and GUI-keychain path; it records PID/exit state itself and is used for read-only isolated reviewers.

`result` is the first manager tool to use for a stuck/completed task: it prints the final message, relay stderr tail when applicable, remote stderr tail, and token summary. `check` is designed for cron and records reported completions in `reported.json`. `kill` follows the recorded transport: relay control kill for a relay proc, or the remote SSH wrapper/child for an SSH task.

### `status.py`: read-only execution UI

`status.py` merges the local relay audit directory with `GET /v1/events` and agent snapshots, showing phases, durations, flags, and selected output. It never invokes a control route. It uses the Mac defaults `~/.codex/zigzag/events.json` and `~/.codex/zigzag/zigzag.token` unless overridden.

```text
python3 dept/status.py [--url URL] [--state-file PATH] [--token-file PATH]
                       [--interval SECONDS] [--once] [--all]
```

`--once` is script-friendly. The interactive view uses arrows to select, left/right (or h/l) to scroll, Enter or `t` for transcript/output, and `q` to quit. It exposes relay loss, audit/log degradation, and cross-clock timings; those flags are diagnostic evidence, not cosmetic warnings.

### Watchers and gates

| Script | Invocation / responsibility | State and safety boundary |
| --- | --- | --- |
| `approval_gate.py OWNER/REPO PR` | Fetches the authoritative daemon `review-gate` report through the authenticated proxy path and exits success/failure accordingly. | Does not reimplement review policy; policy lives in Rust plus `~/.zigzag/config.yaml`. |
| `dispatch_review_round.py PR --repo OWNER/REPO --project-dir DIR [--max-rounds N] [--security]` | Seeds 3 fresh read-only reviewers (`correctness`, `simplicity`, `tests`; add `security` for sensitive changes). Makes immutable source archive + `origin/main...HEAD` diff first. | Persists a dispatching round before first launch; caps active rounds; snapshots live under Mac `~/.codex/dept/review-snapshots/`. |
| `review_round_watcher.py` | Cron-style collector for seeded rounds. Verifies task zero exit and exactly one verdict/head, then posts advisory results. | Review state is atomically written under `review_rounds/`; malformed/missing/failed output becomes `attention`; raw findings are never injected into a write-capable owner. |
| `pr_comment_watcher.py [--burst [MIN]|--burst-off|--burst-poll]` | VM poller for configured PR review/comments from the trusted human; resumes the owning task. | Watermarks, locks, PR session mapping, and burst state are in department runtime state; avoids duplicate/live-worker dispatch. |
| `gdocs_comment_watcher.py [--seed] [--force]` | Mac GUI-session Google Drive poller; discovers configured docs, acknowledges trusted feedback, resumes document owner. | SQLite `~/.zigzag/dept/dept.db`; uses service-account keychain secret; obeys 22:00–07:00 PT quiet hours unless forced. |
| `jules_pr_reviewer.py` | Polls configured Jules-authored PRs and dispatches / re-dispatches a Codex review task. | `jules-review-state.json`, lock, prompts in configured department state; it reviews but must not merge/push. |
| `events.py` | Builds/posts schema-v1 department transitions (`review_*`, `human_wait_*`) with stable retry IDs. | Use it instead of ad hoc envelopes when adding VM department lifecycle facts. |

The expected scheduler is VM cron for PR, Jules, and review-round watchers (typically 5–10 minute cadence), with absolute paths, config/proxy environment, and durable logs. The Google Docs watcher is a Mac GUI-session LaunchAgent, not a VM cron job. `dept/README.md` contains example cron entries.

## Review round workflow

This is the required interpretation of the Python review tooling; it is important because an advisory model comment is not a human approval.

1. After pushing a PR head and checking CI, seed a round: `python3 dept/dispatch_review_round.py PR --repo OWNER/REPO --project-dir DIR`. Add `--security` for auth, credentials, network, crypto, or PII.
2. The dispatcher locks the PR round, resolves the exact GitHub head, writes state as `dispatching`, archives that commit and a binary base diff on the Mac, writes one prompt per lens, then calls fresh `dept.py start` reviewers with `--no-sop --read-only --ssh --task-id`.
3. Read-only reviewers must emit exactly one `VERDICT: APPROVE` or `VERDICT: CHANGES REQUESTED` and one full `HEAD: <40-hex>` for the snapshotted head. They do not receive GitHub credentials or a mutable working tree.
4. `review_round_watcher.py` polls manager status. Only a successful exit and unambiguous exact-head fields are published as `ATTESTATION: MODEL_ADVISORY` comments. Any failed/missing/ambiguous reviewer puts the round in `attention`; investigate its local task result instead of trusting an incomplete comment.
5. Fix every requested change, push a new head, and seed a **new** round. A verdict for an old head cannot pass the gate.
6. Finally run `python3 dept/approval_gate.py OWNER/REPO PR`. Passing requires current-head matching approvals from every lens in the latest complete round, all configured matching CI checks green, and a formal approval by an actor in `approval_gate.human_review_actors`. Shared automation identity and advisory Codex comments never satisfy that human requirement.

The Rust review loop can replace the VM side when enabled in `~/.zigzag/config.yaml`. During `ZIGZAG_REVIEW_LOOP_SHADOW=1` it observes and compares but suppresses dispatch, owner resume, and kill side effects. Do not run both sides authoritatively: that creates duplicate reviewers/comments.

## State, logs, and artifacts

| Path | Contents / owner |
| --- | --- |
| `~/.codex/zigzag/zigzag.token` | Relay bearer token, private mode 0600. Never log it. |
| `~/.codex/zigzag/events.json` | Bounded durable live event store. |
| `~/.codex/zigzag/events.audit/` | Per-execution append-only JSONL archive; independent bounded retention. |
| `~/.codex/zigzag/events.agents.json` | Durable agent registry derived from the configured state-file name. |
| `~/.codex/zigzag/events.agents.agent-logs/` | Owner-only agent stdout/stderr spools (bounded). |
| `~/.codex/zigzag/events.reviews.json` | Rust review-loop state. |
| `~/.codex/zigzag/relay/` | Verified updater release images/current symlink and update status. |
| `~/.codex/zigzag/zigzag.log`, `zigzag.error.log` | LaunchAgent stdout/stderr. |
| `~/.codex/dept/t-xxxxxx/` | Department task prompt, project/session/model markers, PID/child/exit data, events JSONL, stderr, final message. |
| `~/.codex/sessions/**/rollout-*.jsonl` | Codex session metadata; `dept.py resume` searches this to recover a session CWD. |
| `/private/tmp/<branch-slug>/` | Default relay-native agent worktree; inspect state before cleanup. |
| configured department `state_dir` | VM/manager `ledger.jsonl`, `reported.json`, `pr_sessions.json`, locks, prompts, rounds, and watcher watermarks. Default fallback is `dept/runtime`, while the example deployment uses `~/.local/state/codex-dept`. |
| `~/.zigzag/dept/dept.db` | Google Docs watcher SQLite database. |
| `~/.zigzag/config.yaml` | Personal Rust review-loop policy; invalid config disables reviews but does not stop HTTP relay service. |

For any configured state file other than `events.json`, derive adjacent names instead of hard-coding: registry is `<state>.agents.json`, review state is `<state>.reviews.json`, and audit directory is `<state>.audit/`.

## Stuck-task runbook

Start with the least invasive layer and preserve evidence before killing work.

1. **Identify transport and task.** `python3 dept/dept.py list` and `python3 dept/dept.py status TASK` use the local ledger to choose relay or SSH checks. If the ledger is absent, inspect `~/.codex/dept/TASK/` and the relay agent list; do not invent a kill handle.
2. **Check relay health before blaming the task.** Use `launchctl print`, `zzapi health`, and the last 100 lines of `zigzag.error.log`. A running LaunchAgent with failing authenticated health is a daemon/startup/token/network problem, not a completed task.
3. **For a relay task, inspect durable state and output.** Run `zzapi agents list --state running`, `zzapi agents get ID`, and `zzapi agents logs ID --stream stderr --tail 1500`. Then use `dept.py result TASK` for task directory final message and token diagnostics. `orphaned`, `lost_after_restart`, `audit_degraded`, `log_degraded`, or a `dropped_before` advance are data-loss/restart clues, not successful completion.
4. **For an SSH task, inspect the remote directory.** `dept.py result TASK` includes `stderr.log`; check `pid`, `child-pid.txt`, `exit-code.txt`, `events.jsonl`, and `last-message.txt` under `~/.codex/dept/TASK`. SSH liveness only means the wrapper/child has a PID; inspect output and exit code separately.
5. **Check event history.** `python3 dept/status.py --once --all` merges audit state even when the live relay is unavailable. For exact event chronology, use the real relay binary's `timeline` subcommand, not the stale shell wrapper. Treat different `clock` values as cross-machine timestamps, not a reliable elapsed duration.
6. **Only then stop it.** Use `zzapi agents stop ID` for relay-native agents; use `dept.py kill TASK` for manager-owned tasks. Do not call generic proc kill unless you explicitly have the control secret and understand its process-group effect. Stopping does not clean up a worktree.

### Common failures

| Symptom | Likely cause / safe response |
| --- | --- |
| `launchctl` is running but `/v1/health` fails | Read `zigzag.error.log`; verify port/Tailscale bind and bearer token file permissions. Restart only from Mac GUI session using documented deploy flow. |
| VM cannot reach relay but Mac health works | Tailscale ACL, current Tailscale IP, proxy, or token transport issue. Confirm remote `ZIGZAG_PROXY`/config and ACL; do not open the port broadly. |
| `/v1/exec` hangs or policy load fails | Daemon likely started in non-GUI session / Keychain unavailable. Deploy/restart interactively, not via SSH. |
| `denied` from exec/spawn | Allowlist/body policy rejection is deliberately opaque. Inspect/adjust policy only through `zigzag config` in GUI session; never weaken it to debug blindly. |
| Agent shows `orphaned` after restart | This is a legacy state from older relay versions. Inspect process group/output, then stop or restart it through the agent API. |
| Agent log has a gap | `dropped_before` advanced due to 32 MiB bounded spool. Use durable task transcript/audit where available; the missing portion cannot be recovered from the spool. |
| Status says relay events were lost/reset | Bounded store eviction or replacement. Rebuild from audit JSONL and registry; do not infer missing lifecycle transitions. |
| `dept.py status` reports `MISSING` | Missing task files/PID or task was not launched through that manager ledger. Cross-check agent registry and `~/.codex/dept/TASK` before re-dispatch. |
| Review round is `attention` | Failed/missing reviewer or invalid/mismatched verdict/head. Read each reviewer result, fix dispatch/config, then seed a new exact-head round. |
| Review gate fails despite advisory approvals | CI, lens, round, head, or human approval requirements are not met. Use its report; advisory comments never substitute for the formal allowlisted-human review. |
| `zigzag timeline` says unknown command | The shell wrapper shadows the Rust binary and calls stale `dept.py timeline`; invoke the release binary path. |

## Build, test, deploy, and source-of-truth rules

From the repository root, the baseline validation is:

```sh
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
python3 dept/config.py --check
python3 -m unittest discover -s dept -p 'test_*.py'
bash scripts/e2e-full-stack.sh target/debug/zigzag target/debug/zzapi
```

`scripts/sign-release.sh` is the interactive Mac deploy helper: it builds, signs with the stable identity, verifies, and restarts the LaunchAgent. It refuses SSH because code signing and Keychain must use the GUI login session. `scripts/sign-ci-release.sh` signs a supplied CI release binary; `scripts/verify-release.sh` verifies release material; `scripts/install-hooks.sh` installs the config-materialization pre-commit hook; and `scripts/codex-review-launch.sh` is the hardened read-only reviewer launcher used by CI/isolated review tasks. `scripts/e2e-full-stack.sh` wires a real relay, `zzapi`, status view, and fake Codex executable together.

The Rust route code is the implementation source of truth, `openapi.yaml` is the intended public contract, and `README.md` plus `launchd/INSTALL.md` provide deployment/security policy. When they differ, document the difference, update the specification in the same change if behavior is intentional, and never assume a design document describes an already-enabled control loop.
+
## Source symbol index

This generated navigation index is deliberately included so an AI operator can
locate every top-level Rust/Python declaration at this revision.  The narrative
sections explain the operationally significant behavior; this appendix prevents
a “documented module” from hiding an undocumented entry point.  Test declarations
are included because they define edge-case behavior and safe fixture patterns.

## Department tooling: complete Python reference

All `dept/` programs are intentionally small, standard-library-first scripts.
They are not a single daemon.  Most are designed to run from cron and use a
lock file so an overlapping invocation exits rather than races persistent state.
Run them from a checkout containing `dept/`, or ensure their sibling imports
remain resolvable.  Do not copy an individual watcher somewhere else without its
configuration and asset files.

### `dept_config.py` — deployment-local connection configuration

`load_config` resolves the ignored deployment file.  Its order is
`CODEX_DEPT_CONFIG`, then `dept/config.json` adjacent to the script.  It returns
an empty/default object when no file is provisioned where callers can safely
operate without connection credentials, and validates object shape when found.

This configuration is intentionally not the personal Rust review-loop YAML.  It
contains host-specific connection/dept/watcher settings, for example:

```json
{
  "connection": {
    "zigzag_url": "http://100.x.y.z:8765",
    "zigzag_token_file": "~/.codex/zigzag.token",
    "ssh_host": "mac-host"
  },
  "dept": {"state_dir": "~/.zigzag/dept"},
  "watcher": {"state_root": "~/.zigzag/dept"}
}
```

The actual accepted deployment keys are read by their consumer.  Do not commit
this file or token paths carrying secrets.  `config.example.json` is a template;
it is safe to inspect but not a source of live credentials.

### `dept.py` — department task manager

This is the compatibility dispatcher and task-inspection CLI.  It owns a local
append-only JSONL ledger, but task payload and most runtime files live on the
Mac.  It selects the relay transport by default and only uses direct SSH/nohup
when the caller passes `--ssh`.

#### Common environment and paths

| Name | Purpose |
| --- | --- |
| `CODEX_DEPT_CONFIG` | Override ignored deployment connection configuration. |
| `CODEX_DEPT_STATE_DIR` | Override local manager state root when config has no state dir. |
| `ZIGZAG_URL` | Override relay URL for relay-based dispatch/status. |
| `ZIGZAG_TOKEN_FILE` | Override bearer-token file for relay calls. |
| `HTTPS_PROXY` | Input from which relay proxy behavior may be derived. |
| `~/.codex/dept/` | Mac task directory root (`REMOTE_DEPT`). |
| `<state>/ledger.jsonl` | Manager append-only local task ledger. |
| `<state>/reported.json` | `check` completion-notification watermark. |

The ledger row records `id`, project, transport (`via: relay` and `proc`, or
`via: ssh` and `pid`), initial status, read-only marker, redacted/truncated
prompt head, and local start time.  Never hand-edit a ledger to make a process
look finished; use it to find the actual transport then inspect that transport.

#### `start`

Syntax:

```sh
python3 dept/dept.py start PROJECT_DIR PROMPT_FILE \
  [--no-sop] [--writing] [--ssh] [--model MODEL] \
  [--task-id t-abcdef] [--read-only]
```

`PROJECT_DIR` must name a directory meaningful on the Mac, because the actual
agent will execute there.  `PROMPT_FILE` is read locally and cannot be empty.

| Flag | Effect |
| --- | --- |
| `--no-sop` | Send just the task prompt rather than prepending `dept/sop.md`. |
| `--writing` | Prepend `dept/writing.md` instead of code SOP. |
| `--ssh` | Bypass relay and run remote SSH/nohup wrapper. No GUI Keychain policy. |
| `--model MODEL` | Validate/pass a conservative model identifier. |
| `--task-id ID` | Use a preallocated `t-` plus six lowercase hex ID. |
| `--read-only` | Use Codex read-only sandbox/no approval bypass. |

Without `--no-sop` or `--writing`, `decorated_prompt` prepends the standard
code operating procedure.  For non-read-only tasks, it also appends
`relay-announce.md` with the task ID substituted so a completion announcement
can be emitted best effort.

Normal example:

```sh
python3 dept/dept.py start \
  /Users/shukant/Workspace/ShukantPal/zigzag /tmp/task.md --model gpt-5
```

Read-only reviewer example:

```sh
python3 dept/dept.py start /Users/shukant/Workspace/ShukantPal/zigzag \
  /tmp/review.md --no-sop --read-only --ssh
```

The `--ssh` route composes literal shell quoting and a trap-owning wrapper.  It
writes `pid`, `child-pid.txt`, `exit-code.txt`, `events.jsonl`, `stderr.log`, and
`last-message.txt` under the Mac task directory.  It is not a way to access the
relay Keychain policy; it is a separate transport with different recovery data.

#### `resume`

Syntax:

```sh
python3 dept/dept.py resume [PROJECT_DIR] SESSION_ID PROMPT_FILE \
  [--no-sop] [--writing] [--ssh] [--model MODEL] [--task-id ID] [--read-only]
```

If `PROJECT_DIR` is omitted, `resolve_session_cwd` searches Codex rollout JSONL
on the Mac for a matching session’s `session_meta.payload.cwd`.  It requires one
unambiguous CWD and verifies the directory exists remotely.  Supply the project
explicitly if session discovery fails.  The new task execution is not the old
task: preserve both identities in incident notes.

#### `status`

Two grammars deliberately coexist:

```sh
python3 dept/dept.py status t-abcdef       # manager task liveness
python3 dept/dept.py status --once         # delegates to read-only status UI
```

For a manager task, relay transport queries agent/proc outcome; SSH transport
checks PID/exit-code files on Mac.  It prints `RUNNING`, `DONE (exit N)`,
`MISSING`, or pruned information.  A pruned relay compatibility process means
inspect durable agent/audit data rather than retrying automatically.

#### `list`

```sh
python3 dept/dept.py list
```

Walks ledger entries and prints current liveness for entries still marked
running, transport, project, and prompt prefix.  It is diagnostic and does not
mutate task state.

#### `result`

```sh
python3 dept/dept.py result t-abcdef
```

Prints manager status, `last-message.txt` if any, relay exit/stderr tail for a
relay task, SSH `stderr.log` tail where applicable, then token summary.  This is
the first high-signal command after a completion/failure indication.

#### `tokens`

```sh
python3 dept/dept.py tokens t-abcdef
```

Extracts latest total/input token counters and line count from the task’s
`events.jsonl`.  It is an approximate usage display, not billing truth.

#### `check`

```sh
python3 dept/dept.py check
```

Designed for cron.  It scans unreported ledger tasks, prints rich `COMPLETED`
blocks for newly terminal ones, and records their ID in `reported.json`.  When
nothing is newly complete it prints `NOTHING_NEW`.  This command mutates only
the local reporting watermark; it does not modify the task/relay.

#### `kill`

```sh
python3 dept/dept.py kill t-abcdef
```

Finds the latest ledger entry.  Relay tasks call the compatibility kill route
through `zigzag_kill`; SSH tasks use the remote wrapper/child PID sequence.  It
does not delete task directories, worktrees, sessions, or ledger history.
Capture `result` and relevant logs first whenever possible.

#### Internal helpers worth knowing

`_zigzag_call` is the authenticated JSON HTTP primitive; `_zigzag_opener`
installs any proxy behavior; `zigzag_spawn`, `relay_task_outcome`,
`relay_stderr_tail`, and `zigzag_kill` are its relay adapters.  `setup_task_dir`
must finish before launch so a retry sees a coherent payload.  `ssh_worker_script`
and `ssh_kill_script` make exit/kill ownership explicit.  `session_meta_cwd` and
`single_cwd` intentionally refuse ambiguity.  `remote_status_detail` preserves
relay pruned/unavailable detail instead of collapsing it to “done.”

### `status.py` — read-only status TUI and snapshot printer

Syntax:

```sh
python3 dept/status.py [--url URL] [--state-file PATH] [--token-file PATH] \
  [--interval SECONDS] [--once] [--all]
```

Defaults: URL `http://127.0.0.1:8765`, state file
`~/.codex/zigzag/events.json`, token file `~/.codex/zigzag/zigzag.token`, and
two-second refresh.  `ZIGZAG_URL`, `ZIGZAG_STATE_FILE`, and
`ZIGZAG_SECRET_FILE` provide environment defaults.

| Flag | Operational use |
| --- | --- |
| `--url` | Point at a non-default relay/proxy. |
| `--state-file` | Read local durable events from a chosen state file. |
| `--token-file` | Read bearer token for live agent/event snapshots. |
| `--interval` | TUI refresh delay; must be positive. |
| `--once` | Print tabular snapshot and exit; use for scripts. |
| `--all` | Include internal relay maintenance/update executions normally hidden. |

It merges persisted audit/event data with authenticated agent API snapshots.
It makes no control-route request.  The interactive screen supports arrows to
select, h/l or left/right to horizontally scroll, Enter/`t` to toggle selected
transcript/output, and `q` to quit.  Its `Execution` projection marks
cross-clock durations, audit degradation, log degradation, event loss, and
missing agents.  Those flags are evidence to investigate, not transient UI
noise to suppress.

### `events.py` — Python lifecycle event helpers

This module mirrors the schema-v1 fact envelope for department-side producers
and consumers.  It constructs safe timestamped event objects, reads/writes
JSONL-style material, and centralizes field spelling used by status/tests.
Use it rather than inventing a one-off event JSON shape in a watcher.  It does
not own HTTP delivery; callers send events through their configured transport.

### `config.py` — versioned department configuration materializer

Syntax:

```sh
python3 dept/config.py --check
python3 dept/config.py --write
python3 dept/config.py --stdout
```

The module holds versioned declarative configuration and materializes
`dept/config.materialized.json`.  `--check` verifies the checked-in materialized
file matches the canonical rendering and is part of CI/pre-commit.  `--write`
updates that tracked generated file intentionally.  `--stdout` is a safe review
mode.  It also validates known repository/service-account naming and returns
nonzero on invalid source configuration.

Treat `config.materialized.json` as generated-but-reviewed source, not a place
to store a personal Keychain secret.  The Rust review policy deliberately lives
elsewhere in `~/.zigzag/config.yaml`.

### `approval_gate.py` — VM wrapper for Mac canonical decision

Syntax:

```sh
python3 dept/approval_gate.py OWNER/REPO PR_NUMBER
```

It reads URL/token path from deployment config or `ZIGZAG_URL` /
`ZIGZAG_TOKEN_FILE`, builds the authenticated `/v1/review-gate` query, and
prints the daemon response as indented JSON.  It exits `0` only when `pass` is
true; otherwise it prints a synthetic failure reason on transport error and
exits `1`.

It intentionally does not parse YAML policy, GitHub approval comments, CI, or
lens results itself.  If this wrapper and Rust gate differ, Rust is authoritative
and the wrapper should remain thin.

### `dispatch_review_round.py` — seed isolated reviewer round

Syntax:

```sh
python3 dept/dispatch_review_round.py PR \
  --repo OWNER/REPO --project-dir MAC_PROJECT_DIR \
  [--max-rounds N] [--security]
```

Required `PR` is numeric.  `--repo` and `--project-dir` are required because a
reviewer must be anchored to one actual repository checkout.  `--max-rounds`
defaults to 3 and prevents endless re-dispatch.  `--security` adds the security
lens for authentication, credentials, network, cryptography, or PII changes.

The default lenses are `correctness`, `simplicity`, and `tests`.  The script
snapshots immutable review input and the `origin/main...HEAD` diff before
launching read-only tasks.  It records a dispatching round before the first
reviewer starts, writes per-lens task mapping, and transitions to collecting
only after all seed launches succeed.  A partial launch is saved for operator
attention rather than silently claimed complete.

### `review_round_watcher.py` — collect/publish compatibility rounds

Syntax:

```sh
python3 dept/review_round_watcher.py
```

This no-flag cron worker locks `ROUNDS_DIR`, scans `*.json` round state files,
and processes each independently.  It waits for reviewer task zero exits,
parses exactly one verdict attributable to the expected PR head/lens, records
malformed/missing/nonzero results as `attention`, and publishes validated
advisory result comments.  It saves after every meaningful transition so a
crash/retry does not repost the same lens.

State lives beneath the configured review-round root (normally under
`~/.zigzag/dept`); review snapshots are on the Mac under
`~/.codex/dept/review-snapshots/`.  Do not turn an `attention` round into
`published` by editing JSON: repair evidence and seed a fresh exact-head round.

### `pr_comment_watcher.py` — PR feedback dispatch watcher

Syntax:

```sh
python3 dept/pr_comment_watcher.py
python3 dept/pr_comment_watcher.py --burst [MIN]
python3 dept/pr_comment_watcher.py --burst-off
python3 dept/pr_comment_watcher.py --burst-poll
```

This watcher polls configured GitHub PR comments/reviews for fresh comments by
the trusted owner.  First run seeds a watermark and dispatches nothing, avoiding
a historical-comment storm.  It filters worker marker replies, serializes one
task per PR, builds a focused prompt for fresh comments, and starts/resumes a
department task.  It stores active task association so overlapping polls do not
relaunch work.

`--burst [MIN]` enters a thirty-second polling cadence for the default 30
minutes or a supplied duration.  `--burst-off` clears it.  `--burst-poll` does
nothing quietly outside an active burst, for a separate frequent cron entry.
Fresh trusted-owner activity extends the burst window.  The normal cron can run
less often; both variants take the same lock.

### `gdocs_comment_watcher.py` — Google Docs feedback dispatcher

Syntax:

```sh
python3 dept/gdocs_comment_watcher.py [--seed] [--force]
```

The watcher loads the configured Drive folder/additional docs, reads the
`zigzag-sa` GUI-login Keychain service-account item, mints Drive OAuth, and
polls comments.  It records comments in SQLite, ignores resolved/untrusted
ones, posts an “eyes” acknowledgement before dispatch, and routes fresh
comments to the configured owning Codex session when not busy.

`--seed` records the current watermark without dispatch.  `--force` bypasses
the normal 22:00–07:00 Pacific quiet-hours exit.  A lock prevents duplicate
polls.  Its default state root is `~/.zigzag/dept`; the database is usually
`dept.db`.  The service-account key can be raw JSON or hex-encoded JSON but
must identify the expected service account.  Never put it in repo config/logs.

### `jules_pr_reviewer.py` — Jules PR review coordinator

Syntax:

```sh
python3 dept/jules_pr_reviewer.py
```

This no-flag locked cron worker finds open configured Jules PRs, dispatches a
department review task once per PR, tracks its task/head/status in JSON state,
and starts a re-review when a completed reviewed PR’s head SHA changes.  It is
separate from the formal Rust review-loop gate and should not be cited as an
authoritative approval signal.

### `round_state.py` — review-round atomic state helpers

Defines the small persistent-state API used by round dispatch/collection:
derive round paths, read/validate JSON, and atomically write durable updates.
Use these helpers instead of raw `open(..., "w")` to preserve restart-safe
round transitions.

### Test and fixture scripts

`test_dept.py`, `test_config.py`, `test_approval_gate.py`,
`test_dispatch_review_round.py`, `test_review_round_watcher.py`,
`test_pr_comment_watcher.py`, `test_gdocs_comment_watcher.py`, and
`test_codex_launch.py` are executable unittest specifications.  `dept/tests/`
adds event/status projection coverage.  Run them with discovery (shown in the
validation section) after behavior/document changes; they make HTTP/SSH/Drive
side effects fake rather than contacting personal services.

## `zzapi`: exhaustive CLI reference

`zzapi` is the typed CLI for the relay REST API.  It is the preferred command
line diagnostic interface when a relay token is available because it resolves
secure defaults, constructs requests consistently, formats status, and accepts
unique agent-ID prefixes where raw HTTP does not.

### Global options and credential lookup

```text
zzapi [--hostname HOST[:PORT]] [--token-file PATH] [--json] COMMAND ...
```

| Global option | Environment | Default/behavior |
| --- | --- | --- |
| `--hostname` | `ZIGZAG_HOSTNAME` | `100.101.237.83:8765` in this revision. Supply host without scheme. |
| `--token-file` | `ZIGZAG_TOKEN_FILE` | Explicit bearer-token file. |
| `--json` | none | Print raw JSON instead of tables/text. |

The client also honors `ZIGZAG_TOKEN` for a direct token and `ZIGZAG_PROXY` for
the configured proxy path.  Without an explicit token source it tries, in
order, `~/.codex/zigzag/zigzag.token`, then `~/.codex/zigzag.token`.  A token
file must be current-user owned and mode 0600; fix permissions rather than
passing a world-readable token around command lines.

### `zzapi health`

```sh
zzapi health
zzapi --json health
```

Calls `GET /v1/health`.  This has no flags and is the first authenticated
connectivity check.  It does not inspect any agent.

### `zzapi agents list`

```sh
zzapi agents list [--state STATE] [--task-id TASK_ID]
```

Calls `GET /v1/agents`.  `--state` passes a server-side exact state filter;
`--task-id` passes the department/logical task ID filter.  Combine them to find
the current execution of one task.  Use `--json` to obtain every record field.

### `zzapi agents get`

```sh
zzapi agents get ID_OR_UNIQUE_PREFIX
```

Resolves a unique prefix client-side, then calls `GET /v1/agents/{id}`.  If a
prefix is ambiguous, add characters; never guess which task a prefix means.

### `zzapi agents create`

```sh
zzapi agents create --prompt TEXT_OR_FILE --project-dir PATH \
  (--branch BRANCH | --no-branch | --pr NUMBER) \
  [--worktree PATH] [--model MODEL] [--approval-mode MODE] [--timeout-secs N]
```

Calls native `POST /v1/agents`. `--prompt` and `--project-dir` are required.
Choose `--branch` for a worktree, `--pr` to resolve an existing PR's head
branch with `gh pr view`, or explicit `--no-branch` to run directly in the
project directory. Exactly one mode is required; `--branch` and `--no-branch`
cannot be combined. `--worktree` chooses an explicit permitted worktree in
branch or PR mode; otherwise the relay derives its default private-temp path.
`--model` and
`--approval-mode` request provider behavior accepted by the server.  `--timeout-secs`
sets an agent deadline where the route/provider supports it.  The CLI does not
make an unsafe shell command: values remain JSON fields.

### `zzapi agents pause`

```sh
zzapi agents pause ID_OR_UNIQUE_PREFIX
```

Calls `POST /v1/agents/{id}/pause`.  It sends SIGSTOP to the managed group.
Use only for an intentional temporary suspension and confirm head/worktree
state before resuming.

### `zzapi agents resume`

```sh
zzapi agents resume ID_OR_UNIQUE_PREFIX
```

Calls `POST /v1/agents/{id}/resume`.  It is not a task-manager session resume;
it merely SIGCONTs a paused current process group.

### `zzapi agents stop`

```sh
zzapi agents stop ID_OR_UNIQUE_PREFIX
```

Calls `DELETE /v1/agents/{id}`.  This gracefully stops then may force-kill the
native process group.  It deliberately leaves the worktree.  Capture logs and
result context first, then use `zzapi worktrees delete` only when safe.

### `zzapi agents logs`

```sh
zzapi agents logs ID_OR_UNIQUE_PREFIX \
  [--stream stdout|stderr|both] [--after CURSOR] [--tail BYTES] [--follow] [--prefix]
```

Calls the log endpoint.  `--stream` defaults to both.  `--after` is an opaque
numeric spool cursor; persist the returned next cursor for incremental readers.
`--tail` requests newest bounded bytes.  `--follow` repeats long-poll reads.
`--prefix` formats stream/cursor context in human-readable output.  In JSON
mode inspect `dropped_before`, `complete`, and `log_degraded` before trusting
the display as complete.

### `zzapi worktrees create`

```sh
zzapi worktrees create --path PATH --branch BRANCH --repo REPO_PATH
```

Calls `POST /v1/worktrees`; all three flags are required.  It is a mutating Git
operation subject to relay allowed-root/workspace restrictions.  Use native
agent create’s automatic worktree behavior when you do not explicitly need to
manage isolation yourself.

### `zzapi worktrees delete`

```sh
zzapi worktrees delete --path PATH
```

Calls `DELETE /v1/worktrees` with the JSON body.  It is destructive to a dirty
worktree and refuses active/orphaned references.  Inspect `git -C PATH status`
and agent list first.

### `zzapi exec`

```sh
zzapi exec --bin ALLOWLISTED_BINARY [--id CORRELATION_ID] --args ARG...
```

Calls synchronous `POST /v1/exec`.  `--args` consumes everything remaining,
including hyphenated arguments, so place `--id` before it.  An omitted ID is
generated.  “allowlisted binary” means the Keychain policy must authorize this
exact program/command combination; a CLI flag never expands policy.

### `zzapi spawn`

```sh
zzapi spawn --bin ALLOWLISTED_BINARY [--id CORRELATION_ID] \
  [--execution-id ATTEMPT_ID] --args ARG...
```

Calls asynchronous `POST /v1/spawn`.  As with `exec`, place ID flags before
`--args`.  `--execution-id` is a caller correlation for a particular attempt,
not a process handle.  Save the returned process handle and poll it with
`zzapi proc get`; do not assume a human-facing task ID is a handle.

### `zzapi proc get`

```sh
zzapi proc get PROC_HANDLE
```

Calls `GET /v1/proc/{handle}`.  It is only for compatibility spawned processes.
Finished handles are pruned, so a 404 after historical completion should be
investigated through agent registry/audit/department result rather than treated
as a current launch failure.

### `zzapi events`

```sh
zzapi events [--after SEQUENCE] [--epoch EPOCH] [--timeout SECONDS] [--follow]
```

Calls `GET /v1/events`.  Defaults are `after=0`, empty epoch, and ten-second
timeout.  The relay permits at most 55 seconds.  `--follow` repeats reads.  The
CLI preserves/prints returned cursor details, but a long-lived consumer must
also handle `reset` and `lost` by rebuilding its projection.

### `zzapi review-gate`

```sh
zzapi review-gate --repo OWNER/REPO --pr NUMBER
```

Calls canonical `GET /v1/review-gate`.  Both flags are required.  Its pass/fail
does not launch a reviewer or merge anything; it reports the current head’s
gate evidence.  Use `--json` when automating reasons/round details.

### CLI omissions and deliberate alternatives

At this revision `zzapi` does not expose dedicated `providers` or `transcript`
verbs even though relay routes exist.  Use a narrowly authenticated HTTP client
for those endpoints, or use `agents logs`/`--json` and the status UI for normal
operations.  Do not substitute the unrelated shell wrapper named `zigzag`.

## Configuration and security catalogue

### Relay daemon flags and environment

| CLI flag | Environment default | Required/default/range | Effect |
| --- | --- | --- | --- |
| `--secret-file PATH` | `ZIGZAG_SECRET_FILE` | Required | Normal bearer-token file. |
| `--state-file PATH` | `ZIGZAG_STATE_FILE` | Required | Bounded event store path; derives registry/review files. |
| `--control-secret-file PATH` | `ZIGZAG_CONTROL_SECRET_FILE` | Optional | Enables legacy generic proc kill with separate secret. |
| `--port N` | none | default 8765; valid u16 | Loopback and Tailscale listener port. |
| `--tailscale-ip IP` | none | optional validated 100.64.0.0/10 IPv4 | Test/explicit bind override. |
| `--max-events N` | none | default 1000; positive | Live-store retention count. |
| `--watch-repo OWNER/REPO` | none | repeatable | Legacy GitHub PR-watch repository. |
| `--watch-interval S` | none | default 30; 30–3600 | Legacy PR-watch cadence. |
| `--update-dir PATH` | `ZIGZAG_UPDATE_DIR` | derived beside state file | Update staging/bootstrap directory. |
| `--update-interval S` | `ZIGZAG_UPDATE_INTERVAL` | default 3600; 0–86400 | Scheduled check cadence; zero disables checks. |
| `--update-policy VALUE` | `ZIGZAG_UPDATE_POLICY` | enabled/paused/pin:VERSION | Update manager policy. |
| `--update-ready-file PATH` | none | internal | New-process watchdog readiness handoff. |

Companion paths derived from `--state-file` are `STATE.agents.json` for durable
agent registry and `STATE.reviews.json` for review-loop durable state.  The
update directory defaults to `relay/` next to the state file.  Keep all three
on a durable local volume; do not use ephemeral `/tmp` in production.

### Personal review-loop YAML

Path: `~/.zigzag/config.yaml`.

Top level:

```yaml
review_loop:
  enabled: true
  intervals:
    discovery_seconds: 30
    review_seconds: 30
    merge_seconds: 30
  repositories:
    - repository: OWNER/REPO
      full_rounds_max: 2
      verification_rounds_max: 2
      lenses: [correctness, simplicity, tests]
      require_security_lens: false
      required_ci_checks:
        - label: CI
          name_pattern: ci
      trusted_verdict_identity: ShukantPal
      result_limits:
        max_findings_per_lens: 20
        max_bytes_per_lens: 16384
```

The embedded JSON schema is authoritative for exact required fields/types and
validation messages.  Startup loads it once; edit the file, validate/restart
through the GUI-session operational flow, then inspect daemon logs for config
violations.  An invalid file disables the review loop rather than guessing a
policy.

| YAML field | Meaning |
| --- | --- |
| `review_loop.enabled` | Explicit opt-in to starting Rust loop. |
| `intervals.discovery_seconds` | Cadence for PR discovery/head change sensing. |
| `intervals.review_seconds` | Cadence for reviewer/result collection. |
| `intervals.merge_seconds` | Cadence for merge/closed transition processing. |
| `repositories[].repository` | Exact GitHub owner/repository policy scope. |
| `full_rounds_max` | Maximum substantive review rounds for one head/PR policy path. |
| `verification_rounds_max` | Maximum verification/retry round count. |
| `lenses` | Required independent review perspectives. |
| `require_security_lens` | Requires security verdict/coverage for gate. |
| `required_ci_checks` | Label/pattern requirements against current-head checks. |
| `trusted_verdict_identity` | GitHub identity accepted for generated verdict provenance. |
| `result_limits` | Maximum accepted findings and bytes per lens. |

### Keychain allowlist policy

The allowlist is stored in the GUI login Keychain item:

| Keychain property | Value |
| --- | --- |
| Service | `zigzag` |
| Account | `exec-allowlist` |
| Reader/writer | The Rust relay/config command in a local GUI session. |

Read it with:

```sh
/path/to/real/zigzag config get-allowlist
```

Set it only after reviewing a policy file:

```sh
/path/to/real/zigzag config set-allowlist --file /path/to/policy.json
```

A policy binary entry pins a resolved executable and its metadata identity,
then declares accepted argv patterns/commands.  `gh` can have a constrained
read-only repository set.  The policy parser rejects unknown/malformed fields,
unsafe binary names, excessive/invalid command configuration, and unavailable
binaries fail closed at spawn time.  Never broaden it to a shell binary just to
make an ad hoc task work.  Add the narrow executable/subcommand, test it, and
preserve read-only GitHub scoping.

### Filesystem state map

| Path | Owner | Contents | Safe operator treatment |
| --- | --- | --- | --- |
| `~/.codex/zigzag/zigzag.token` | Mac relay/client | Normal bearer secret. | 0600; never print/log. |
| `~/.codex/zigzag/events.json` | zz Store | Bounded live event store. | Read/timeline; do not hand-edit. |
| `~/.codex/zigzag/events.agents.json` | AgentRegistry | Durable agent records. | Read via API; do not hand-edit. |
| `~/.codex/zigzag/events.reviews.json` | review loop | Durable review-loop rounds/state. | Backup before forensic examination; do not edit live. |
| `~/.codex/zigzag/events.audit/` | zz Store | Per-execution redacted audit JSONL. | Use for gaps/recovery. |
| `~/.codex/zigzag/agents/codex/` | proc route | Native Codex transcript material. | Read through API first. |
| `~/.codex/zigzag/zigzag.log` | LaunchAgent | stdout log. | Tail/read. |
| `~/.codex/zigzag/zigzag.error.log` | LaunchAgent | stderr/startup errors. | Tail/read. |
| `~/.codex/dept/t-xxxxxx/` | dept manager | Prompt, markers, output, exit diagnostics. | Keep through investigation. |
| `~/.zigzag/config.yaml` | operator | Personal review policy. | Edit deliberately; restart to apply. |
| `~/.zigzag/dept/` | Python watchers | DBs, watermarks, locks, review-round state. | Back up before repair; respect locks. |
| `~/.zigzag/review-workspaces/` | Rust review loop | Private review workspace material. | Treat as sensitive temporary evidence. |
| `~/.zigzag/review-tasks/` | Rust review loop | Private reviewer task files. | Do not publish prompts/results wholesale. |
| `~/.codex/sessions/**/rollout-*.jsonl` | Codex | Session metadata/events. | Used to resolve resume CWD; sensitive transcript data. |

### Network and authentication boundary

The relay binds loopback plus one discovered/validated Tailscale IPv4.  It speaks
plain HTTP on the tailnet trust boundary, so ACLs/proxy configuration remain
security controls.  Normal secrets authorize API read/create operations; a
separate control secret gates generic compatibility kill.  Keychain policy is a
third boundary that restricts the commands a normal bearer can cause to run.
All three must be correctly configured—network reachability is not authorization.

### Installed `~/.local/bin/zigzag` wrapper

This shell wrapper is not the Rust binary.  Its current code pins:

```sh
DEPT_DIR="/Users/shukant/Workspace/shukantpal/zigzag/dept"
```

Dispatch table:

| Invocation | Actual behavior |
| --- | --- |
| `zigzag` | Runs `python3 "$DEPT_DIR/status.py"`. |
| `zigzag status [flags]` | Runs status UI with following flags. |
| `zigzag timeline TASK ...` | Runs `dept.py timeline`; this is stale because current manager has no timeline command. |
| `zigzag prs [flags]` | Execs `~/.local/bin/zigzag-prs`. |
| Other | Prints `usage: zigzag [status|timeline <task-id>|prs]` and exits 1. |

When `~/.local/bin` precedes the Rust release directory in `PATH`, a bare
`zigzag config`, `zigzag updates`, or `zigzag timeline` will not invoke the
daemon binary.  Use an absolute release binary path for relay control until the
wrapper is corrected in a separate intentionally-scoped change.

## Review-round system and approval gate

### Why reviewers are isolated

Review worker output is untrusted input to the system.  A PR body, comment, diff,
or reviewer response can contain instructions.  The review system therefore
binds a worker to an immutable exact-head snapshot, a constrained lens, and a
read-only sandbox.  It requires a small machine-readable verdict format, validates
it against expected repository/PR/head/lens state, and publishes an advisory
comment rather than giving that text directly to a write-capable owner process.

### Compatibility Python round sequence

1. `dispatch_review_round.py` takes a per-repository/PR flock.
2. It lists stored rounds and refuses when non-superseded count reaches
   `--max-rounds`.
3. It asks GitHub for exact `headRefOid` through the Mac credential path.
4. It selects monotonically increasing round number and allocates one task ID
   per lens.
5. It writes a JSON round record with status `dispatching` **before** creating
   snapshot/starting any worker.
6. It creates a temporary immutable source archive, binary diff against
   `origin/main...HEAD`, and `review-head`, then atomically renames it.
7. It writes one lens-specific prompt whose instructions prohibit modification,
   network/GitHub tools, untrusted metadata, and free-form verdict shapes.
8. It calls `dept.py start ... --no-sop --read-only --ssh --task-id` for each
   worker, so no reviewer needs GUI Keychain/GitHub credentials.
9. It changes status to `collecting` only after all launches work.  Any exception
   persists `attention` plus reason.
10. `review_round_watcher.py` later checks each worker’s terminal status/output.
11. It accepts only exactly one expected verdict/head, saves every posted lens,
   and sets `published` when all admissible results are published.

### Lens semantics

| Lens | Question it answers | Should report | Should not report |
| --- | --- | --- | --- |
| `correctness` | Does changed behavior implement intent safely? | Concrete changed-code defects, regressions, missing handling. | Generic style preference. |
| `simplicity` | Is changed design unnecessarily complicated/risky? | Simplification that corrects a maintainability or correctness risk. | Cosmetic refactors unrelated to change. |
| `tests` | Is changed behavior adequately verified? | Missing test that would expose a plausible introduced defect. | Demands for unrelated test rewrites. |
| `security` | Did sensitive change introduce a real security issue? | Auth, credential, network, crypto, PII/control-boundary findings. | Security theatre or unsupported speculation. |

No lens may read external PR metadata or invoke network tools in the compatibility
reviewer prompt.  “Approve” means the constrained worker saw no concrete finding
within its snapshot/lens; it does not mean formal approval gate passed.

### Rust authoritative loop sequence

1. Daemon startup loads and validates personal YAML; valid enabled config starts
   the loop and records durable review state.
2. Discovery fetches configured PR snapshots/comments and recognizes open/current
   head state.  New head supersedes old active rounds and stages stale agent/
   comment cleanup.
3. Missing reviewer lenses are dispatched with deterministic task/verdict IDs
   and private workspace/task files.
4. Collection parses embedded schema-validated reviewer result objects, applies
   source/head/lens/attempt limits, and identifies latest admissible verdicts.
5. Publication writes only trusted generated comments with marker identity and
   retries cleanup/deletion debt without confusing it for a new result.
6. Gate evaluation checks an active current-head round, all required lens
   outcomes (including security policy), required CI checks, trusted verdict
   identity, and configured human approval requirements.
7. If an owner continuation is appropriate, the loop locates matching
   department/Codex owner context and validates a spawn fence against repository,
   branch, head, and current round generation before resuming/spawning it.
8. Merge polling terminalizes closed/merged PRs and cleans managed agents/comments
   safely.  All state transitions persist atomically.

### Gate interpretation

`pass:true` means the daemon’s current inputs satisfy the current policy for the
current PR head.  It is not a perpetual certificate: head updates, CI changes,
round supersession, deleted comments, or policy change can turn it false.  Always
query immediately before the action that relies on it.

Typical `reasons` classes:

| Gate reason class | Operator response |
| --- | --- |
| No active/current-head round | Dispatch/wait for proper exact-head review. |
| Required lens missing/non-approve | Inspect validated verdict; correct code or re-run exact lens. |
| Security lens required | Add/re-run security review under current head. |
| CI check absent/failing | Fix/re-run required CI; do not waive via comment. |
| Trusted identity/provenance fails | Investigate generated comment/result source; do not copy it manually. |
| Human approval missing | Obtain policy-valid review; model/reviewer approval is not a substitute. |
| Review-loop/config/remote failure | Treat as fail closed, inspect relay logs/config/GitHub reachability. |

### Shadow mode

Set `ZIGZAG_REVIEW_LOOP_SHADOW=1` only during an explicitly planned rollout.
The loop may observe/project/report in shadow mode, but `authoritative_mode`
guards authoritative actions.  A gate report in shadow must be presented as
observational evidence, not as permission to merge or resume owner work.

## Rust module-by-module operating notes

The repository map at the start is deliberately concise.  This index supplies
the per-module ownership, key types/functions, and direct neighbors needed to
operate or safely modify every non-test relay module.  For exact declarations,
use the source-symbol index; it includes line locations for every top-level
Rust declaration in this revision.

| Module | Responsibility | Key types/functions | Main interactions |
| --- | --- | --- | --- |
| `main.rs` | Process entry, server composition, startup/recovery ordering. | `main`, `run`. | Calls config, opens zz state, starts reaper/review/update/server. |
| `auth.rs` | Normal/control bearer-token validation. | `authorized`. | Called by `server` before any route. |
| `config.rs` | Strict daemon flags/env parsing and Keychain config command. | `Config`, `server_config`, `run_config`, `valid_github_repo`. | Supplies main; invokes session/exec for allowlist control. |
| `events.rs` | Canonical relay lifecycle-fact creation and recovered fact replay. | `relay_event`, `new_execution_id`, `persist_first_output`, `replay_recovered_lifecycle`. | Used by main, routes, proc, update; persists through Store. |
| `exec.rs` | Keychain-held allowlist parse, binary identity pinning, bounded synchronous run. | `Policy`, `ExecRequest`, `ExecResult`, `parse_request`, `load_policy`, `run`. | `server` loads policy; exec/spawn/agents verify before process creation. |
| `github.rs` | Legacy PR discovery and HTTP review-gate adapter. | `github_watch_loop`, `github_open_pull_requests`, `review_gate_request`. | Uses allowlisted `gh`; delegates decisions to review_loop. |
| `http.rs` | Direct bounded HTTP parser, query decoder, JSON response codec. | `Request`, `ReadRequestError`, `read_request`, `get_query`, `reply`. | Used exclusively by server/routes for wire behavior. |
| `logging.rs` | Process/request response logging with per-request guard context. | `init`, `exec_route`, `begin_request`, `RequestGuard`. | Main initializes; server/routes log safely. |
| `proc.rs` | Child process groups, output spools/transcripts, reaper and restart identity checks. | `ProcEntry`, `spawn_proc`, `drain_to_capture`, `start_reaper`, `kill_process_group`. | Routes create/query/kill; registry/store preserve lifecycle. |
| `provider.rs` | Literal noninteractive CLI recipes and installed-provider discovery. | `Provider`, `AgentOpts`, provider structs, `provider_from_name`. | Agent route chooses recipe; exec policy remains authorization boundary. |
| `review_loop.rs` | Personal policy validation, durable review state, dispatch/results/gate/owner continuation. | `PersonalConfig`, `ReviewLoopConfig`, `StateStore`, `start`, `gate_report`. | Main starts it; GitHub route asks its gate report; proc/registry own workers. |
| `server.rs` | Socket accept cap, auth choice, shared state, complete route dispatch. | `Server`, `Supervisor`, `ConnectionLimiter`, `handle_with_services`, `post`, `get`. | Main constructs it; calls all route modules. |
| `session.rs` | GUI login-session enforcement and Tailnet bind-address discovery. | `require_gui_login_session`, `resolve_tailscale_ip`, `is_tailscale_ipv4`. | Config control and server policy load; main listener binding. |
| `update.rs` | Signed update fetch/verify/stage/replacement/rollback/control. | `Policy`, `Config`, `Manager`, `Status`, `run_control`, `run_watchdog`. | Main schedules/audits it; spawn/agents respect drain state. |
| `routes/mod.rs` | Declares route-module namespace. | module declarations only. | Server imports individual endpoint modules. |
| `routes/agents.rs` | Native agent create/list/status/log/transcript/pause/resume/delete. | `AgentRoute`, `agent_route`, `agent_request`, `agent_post_request`, `agent_delete`. | Calls provider/exec/proc/registry/worktree logic. |
| `routes/events.rs` | Offline timeline command and same-clock phase calculations. | `run_timeline`, `timeline_output`, `phase_events`. | Main subcommand; opens zz Store. |
| `routes/exec.rs` | REST adapter for synchronous execution/background spawn. | `exec_request`, `spawn_request`. | Uses exec policy, proc, events, update state. |
| `routes/procs.rs` | Compatibility proc poll/kill and terminal transition. | `ProcRoute`, `poll_proc`, `kill_proc`, `update_proc_status_with_handle`. | Manipulates supervisor procs, registry, store. |
| `routes/providers.rs` | Provider capability response. | `providers_request`. | Calls provider recipes/availability only. |
| `routes/worktrees.rs` | Allowed-root/workspace Git worktree create/remove. | `WorktreeError`, `worktree_create_plan`, `worktree_delete_plan`. | Agents and explicit worktree endpoint; uses Git subprocesses. |
| `e2e.rs` | In-process end-to-end behavior coverage. | `e2e_*` tests. | Exercises server/routes with temp repositories/policy. |
| `tests.rs` | Unit/integration fixtures and edge-case assertions. | `test_server`, request helpers, route/state tests. | Documents expected security/recovery behavior. |

### How to navigate a bug by boundary

| Symptom | First implementation locations | Why |
| --- | --- | --- |
| Auth 401 or unexpected control-kill auth | `server.rs`, then `auth.rs` | Server selects normal vs control secret based on method/route. |
| Request 400/431/body problem | `http.rs`, then route module | Parser limits fail before route parsing. |
| Opaque `denied` from exec/spawn | `routes/exec.rs`, `exec.rs`, `session.rs` | Intentional fail-closed policy/GUI boundary hides details from client. |
| Agent create/worktree failure | `routes/agents.rs`, `routes/worktrees.rs`, `provider.rs` | Validation precedes worktree/Git/provider process start. |
| Agent never becomes terminal | `proc.rs`, `routes/procs.rs`, `zz` registry | Reaper requires child/group/output convergence. |
| Lost output/transcript | `proc.rs`, `routes/agents.rs`, agent registry | Spool cursor/retention differs from transcript path. |
| Bad timeline duration | `routes/events.rs`, `events.rs` | Durations require matching clock and execution ID. |
| Gate/round decision | `review_loop.rs`, then `github.rs` | Gate semantics are core state machine; route merely adapts it. |
| Update drain/replacement | `update.rs`, `main.rs`, `routes/exec.rs`, `routes/agents.rs` | Updater controls admission and watchdog lifecycle. |

### Important non-module artifacts

| Artifact | Use |
| --- | --- |
| `zzd/src/config-v1.json` | Embedded schema for `~/.zigzag/config.yaml`; validates review-loop policy. |
| `zzd/src/reviewer-result-v1.json` | Embedded schema for isolated reviewer result payloads. |
| `zzd/trust/sigstore-trusted-root.json` | Embedded trust root used by signed updater verification. |
| `openapi.yaml` | Public API intent and wire schema aid; implementation routes remain current authority. |
| `zz/src/lib.rs` | Shared JSON, Store/audit/epoch, secret reader, agent registry implementation. |
| `launchd/com.shukantpal.zigzag.plist` | LaunchAgent template; inspect before operational installation/cutover. |
| `launchd/INSTALL.md` | Canonical launchd installation and review-loop cutover procedure. |
| `scripts/sign-release.sh` | GUI-session build/sign/verify/restart deployment helper. |
| `scripts/codex-review-launch.sh` | Hardened reviewer launcher used by review flows. |


- `zzd/src/logging.rs:18` — `pub fn init() {`
- `zzd/src/logging.rs:28` — `pub fn exec_route(bin: &str, args: &[String]) -> String {`
- `zzd/src/logging.rs:37` — `struct RequestInfo {`
- `zzd/src/logging.rs:49` — `pub struct RequestGuard;`
- `zzd/src/logging.rs:61` — `pub fn begin_request(method: &str, path: &str, source: &str) -> RequestGuard {`
- `zzd/src/logging.rs:75` — `pub fn log_response(status: u16, source: &str) {`
- `zzd/src/proc.rs:19` — `pub(crate) const MAX_FINISHED_PROCS: usize = 128;`
- `zzd/src/proc.rs:20` — `pub(crate) const FINISHED_PROC_RETENTION: Duration = Duration::from_secs(60 * 60);`
- `zzd/src/proc.rs:21` — `pub(crate) const COMPAT_OUTPUT_CAP: usize = 2 * 1024 * 1024;`
- `zzd/src/proc.rs:22` — `pub(crate) struct ProcEntry {`
- `zzd/src/proc.rs:37` — `pub(crate) struct SpawnedProc {`
- `zzd/src/proc.rs:43` — `pub(crate) struct AgentSpawnDetails {`
- `zzd/src/proc.rs:56` — `pub(crate) fn agent_transcript_path(agent_id: &str) -> Option<PathBuf> {`
- `zzd/src/proc.rs:67` — `fn create_agent_transcript(agent_id: &str) -> Result<File, String> {`
- `zzd/src/proc.rs:90` — `pub(crate) fn spawn_proc(`
- `zzd/src/proc.rs:239` — `pub(crate) fn drain_to_capture(`
- `zzd/src/proc.rs:278` — `pub(crate) fn output_is_complete(entry: &ProcEntry) -> bool {`
- `zzd/src/proc.rs:282` — `pub(crate) fn proc_json(entry: &ProcEntry) -> Json {`
- `zzd/src/proc.rs:313` — `pub(crate) fn kill_process_group(process_group: i32) -> bool {`
- `zzd/src/proc.rs:317` — `pub(crate) fn force_kill_process_group(process_group: i32) -> bool {`
- `zzd/src/proc.rs:324` — `pub(crate) fn process_group_running(process_group: i32) -> bool {`
- `zzd/src/proc.rs:328` — `pub(crate) fn process_identity(pid: i32) -> Option<String> {`
- `zzd/src/proc.rs:344` — `pub(crate) fn process_identity(pid: i32) -> Option<String> {`
- `zzd/src/proc.rs:351` — `pub(crate) fn process_identity(_pid: i32) -> Option<String> {`
- `zzd/src/proc.rs:354` — `pub(crate) fn recovered_agent_identity_matches(agent: &AgentRecord) -> bool {`
- `zzd/src/proc.rs:362` — `pub(crate) fn managed_agent_running(agent: &AgentRecord) -> bool {`
- `zzd/src/proc.rs:365` — `pub(crate) fn managed_agent_running_with(`
- `zzd/src/proc.rs:375` — `pub(crate) fn unique_handle(entries: &HashMap<String, ProcEntry>) -> Result<String, String> {`
- `zzd/src/proc.rs:383` — `pub(crate) fn prune_procs(entries: &mut HashMap<String, ProcEntry>, now: Instant) {`
- `zzd/src/proc.rs:401` — `pub(crate) fn start_reaper(state: Arc<Server>) {`
- `zzd/src/main.rs:34` — `fn main() {`
- `zzd/src/main.rs:41` — `fn run() -> Result<(), String> {`
- `zzd/src/provider.rs:15` — `fn bin_on_path(bin: &str) -> bool {`
- `zzd/src/provider.rs:36` — `pub(crate) struct AgentOpts {`
- `zzd/src/provider.rs:72` — `pub(crate) fn provider_from_name(name: &str) -> Result<Box<dyn Provider>, String> {`
- `zzd/src/provider.rs:83` — `pub(crate) const PROVIDER_NAMES: &[&str] = &["codex", "gemini", "opencode", "grok"];`
- `zzd/src/provider.rs:86` — `pub(crate) const DEFAULT_PROVIDER: &str = "codex";`
- `zzd/src/provider.rs:90` — `pub(crate) struct CodexProvider;`
- `zzd/src/provider.rs:128` — `pub(crate) struct GeminiProvider;`
- `zzd/src/provider.rs:153` — `pub(crate) struct OpenCodeProvider;`
- `zzd/src/provider.rs:184` — `pub(crate) struct GrokProvider;`
- `zzd/src/http.rs:7` — `pub(crate) const MAX_BODY: usize = 64 * 1024;`
- `zzd/src/http.rs:9` — `pub(crate) const MAX_HEADER_BLOCK_BYTES: usize = 8 * 1024;`
- `zzd/src/http.rs:11` — `pub(crate) const MAX_HEADER_COUNT: usize = 100;`
- `zzd/src/http.rs:12` — `pub(crate) struct Request {`
- `zzd/src/http.rs:19` — `pub(crate) enum ReadRequestError {`
- `zzd/src/http.rs:24` — `pub(crate) fn denied(stream: &mut TcpStream, id: &str) -> Result<(), String> {`
- `zzd/src/http.rs:28` — `pub(crate) fn denial_response(id: &str) -> (u16, Json) {`
- `zzd/src/http.rs:31` — `pub(crate) fn denial_json(id: &str) -> Json {`
- `zzd/src/http.rs:37` — `pub(crate) fn get_query(target: &str) -> Result<(u64, u64, String), String> {`
- `zzd/src/http.rs:58` — `pub(crate) fn read_json(result: ReadResult) -> Json {`
- `zzd/src/http.rs:76` — `pub(crate) fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {`
- `zzd/src/http.rs:162` — `pub(crate) fn read_header_line(`
- `zzd/src/http.rs:189` — `pub(crate) fn query(target: &str) -> Result<HashMap<String, String>, String> {`
- `zzd/src/http.rs:209` — `pub(crate) fn percent_decode(input: &str) -> Result<String, String> {`
- `zzd/src/http.rs:233` — `pub(crate) fn error(message: &str) -> Json {`
- `zzd/src/http.rs:236` — `pub(crate) fn reply(stream: &mut TcpStream, code: u16, value: Json) -> Result<(), String> {`
- `zzd/src/github.rs:12` — `pub(crate) fn should_start_legacy_watch(`
- `zzd/src/github.rs:18` — `pub(crate) fn github_watch_loop(state: Arc<Server>, repos: Vec<String>, interval: Duration) {`
- `zzd/src/github.rs:55` — `pub(crate) fn github_open_pull_requests(repo: &str) -> Result<Vec<u64>, String> {`
- `zzd/src/github.rs:76` — `pub(crate) fn parse_github_open_pull_requests(output: &str) -> Result<Vec<u64>, String> {`
- `zzd/src/github.rs:101` — `pub(crate) fn review_gate_request<G>(`
- `zzd/src/github.rs:147` — `pub(crate) fn review_gate_parameters(target: &str) -> Result<(String, u64), ()> {`
- `zzd/src/exec.rs:18` — `const KEYCHAIN_SERVICE: &str = "zigzag";`
- `zzd/src/exec.rs:19` — `const KEYCHAIN_ACCOUNT: &str = "exec-allowlist";`
- `zzd/src/exec.rs:22` — `pub const EXEC_TIMEOUT: Duration = Duration::from_secs(300);`
- `zzd/src/exec.rs:23` — `pub const OUTPUT_CAP: usize = 1024 * 1024;`
- `zzd/src/exec.rs:24` — `const MAX_ARGS: usize = 64;`
- `zzd/src/exec.rs:25` — `const MAX_ARGS_BYTES: usize = 8 * 1024;`
- `zzd/src/exec.rs:26` — `const MAX_ID_BYTES: usize = 128;`
- `zzd/src/exec.rs:29` — `pub struct Policy {`
- `zzd/src/exec.rs:44` — `struct BinaryIdentity {`
- `zzd/src/exec.rs:53` — `struct BinPolicy {`
- `zzd/src/exec.rs:65` — `fn resolve_binary_identity(path: &str) -> Option<BinaryIdentity> {`
- `zzd/src/exec.rs:84` — `fn verify_binary_identity(identity: &BinaryIdentity) -> Result<PathBuf, String> {`
- `zzd/src/exec.rs:105` — `pub struct ExecRequest {`
- `zzd/src/exec.rs:111` — `pub struct ExecResult {`
- `zzd/src/exec.rs:141` — `pub enum VerifyError {`
- `zzd/src/exec.rs:339` — `fn is_read_only_gh_command(args: &[String], repos: &BTreeSet<String>) -> bool {`
- `zzd/src/exec.rs:358` — `fn is_read_only_gh_api(args: &[String], repos: &BTreeSet<String>) -> bool {`
- `zzd/src/exec.rs:381` — `fn gh_repo_argument(args: &[String]) -> Option<&str> {`
- `zzd/src/exec.rs:402` — `fn pr_watchdog_read_endpoint_repo(endpoint: &str) -> Option<String> {`
- `zzd/src/exec.rs:439` — `fn valid_git_oid(value: &str) -> bool {`
- `zzd/src/exec.rs:443` — `fn valid_github_name(value: &str) -> bool {`
- `zzd/src/exec.rs:450` — `fn valid_github_repo(value: &str) -> bool {`
- `zzd/src/exec.rs:457` — `fn object_fields<'a>(json: &'a Json, name: &str) -> Result<&'a [(String, Json)], String> {`
- `zzd/src/exec.rs:464` — `fn field<'a>(fields: &'a [(String, Json)], name: &str) -> Option<&'a Json> {`
- `zzd/src/exec.rs:471` — `fn require_only(fields: &[(String, Json)], allowed: &[&str], name: &str) -> Result<(), String> {`
- `zzd/src/exec.rs:495` — `fn require_allowed(fields: &[(String, Json)], allowed: &[&str], name: &str) -> Result<(), String> {`
- `zzd/src/exec.rs:510` — `fn valid_bin_name(name: &str) -> bool {`
- `zzd/src/exec.rs:517` — `pub fn parse_request(body: &Json) -> Result<ExecRequest, String> {`
- `zzd/src/exec.rs:548` — `pub fn request_id(body: &Json) -> String {`
- `zzd/src/exec.rs:556` — `pub fn load_policy() -> Result<Policy, String> {`
- `zzd/src/exec.rs:564` — `pub fn store_policy(policy: &Policy) -> Result<(), String> {`
- `zzd/src/exec.rs:570` — `fn keychain_entry() -> Result<Entry, String> {`
- `zzd/src/exec.rs:575` — `pub fn run(path: &Path, request: ExecRequest) -> ExecResult {`
- `zzd/src/exec.rs:579` — `fn run_with_timeout(path: &Path, request: ExecRequest, timeout: Duration) -> ExecResult {`
- `zzd/src/exec.rs:643` — `fn drain_limited(mut pipe: impl Read) -> (Vec<u8>, bool) {`
- `zzd/src/routes/procs.rs:12` — `pub(crate) enum ProcRoute<'a> {`
- `zzd/src/routes/procs.rs:16` — `pub(crate) fn poll_proc(`
- `zzd/src/routes/procs.rs:42` — `pub(crate) fn kill_proc(`
- `zzd/src/routes/procs.rs:84` — `pub(crate) fn update_proc_status_with_handle(`
- `zzd/src/routes/procs.rs:166` — `pub(crate) fn proc_route(path: &str) -> Option<ProcRoute<'_>> {`
- `zzd/src/events.rs:5` — `pub(crate) fn unix_timestamp() -> String {`
- `zzd/src/events.rs:12` — `pub(crate) fn relay_timestamp() -> String {`
- `zzd/src/events.rs:15` — `pub(crate) fn relay_clock() -> String {`
- `zzd/src/events.rs:38` — `pub(crate) fn random_hex_128() -> Result<String, String> {`
- `zzd/src/events.rs:45` — `pub(crate) fn new_execution_id() -> Result<String, String> {`
- `zzd/src/events.rs:48` — `pub(crate) fn relay_event(kind: &str, task_id: &str, execution_id: &str, payload: Json) -> Json {`
- `zzd/src/events.rs:51` — `pub(crate) fn relay_event_at(`
- `zzd/src/events.rs:76` — `pub(crate) fn persist_first_output(store: &Store, agent: &AgentRecord) -> Result<(), String> {`
- `zzd/src/events.rs:105` — `pub(crate) fn replay_recovered_lifecycle(store: &Store, agent: &AgentRecord) -> Result<(), String> {`
- `zzd/src/auth.rs:1` — `pub(crate) fn authorized(supplied: &str, secret: &str) -> bool {`
- `zzd/src/routes/providers.rs:12` — `pub(crate) fn providers_request(stream: &mut TcpStream) -> Result<(), String> {`
- `zzd/src/config.rs:9` — `pub(crate) struct Config {`
- `zzd/src/config.rs:25` — `pub(crate) fn server_config(arguments: Vec<String>) -> Result<Config, String> {`
- `zzd/src/config.rs:132` — `pub(crate) fn valid_github_repo(repo: &str) -> bool {`
- `zzd/src/config.rs:144` — `pub(crate) fn run_config(arguments: &[String]) -> Result<(), String> {`
- `zzd/src/config.rs:159` — `pub(crate) fn is_get_allowlist(arguments: &[String]) -> bool {`
- `zzd/src/config.rs:162` — `pub(crate) fn allowlist_file(arguments: &[String]) -> Result<&str, String> {`
- `zzd/src/routes/exec.rs:12` — `pub(crate) struct SpawnRequest {`
- `zzd/src/routes/exec.rs:16` — `pub(crate) fn exec_request(`
- `zzd/src/routes/exec.rs:82` — `pub(crate) fn spawn_request(`
- `zzd/src/routes/exec.rs:215` — `pub(crate) fn parse_exec_request(body: &[u8]) -> Result<exec::ExecRequest, Json> {`
- `zzd/src/routes/exec.rs:225` — `pub(crate) fn parse_spawn_request(body: &[u8]) -> Result<SpawnRequest, Json> {`
- `dept/dispatch_review_round.py:20` — `CONFIG = load_config()`
- `dept/dispatch_review_round.py:21` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/dispatch_review_round.py:22` — `STATE_DIR = state_dir(CONFIG)`
- `dept/dispatch_review_round.py:23` — `ROUNDS_DIR = Path(STATE_DIR) / "review_rounds"`
- `dept/dispatch_review_round.py:24` — `PROMPT_DIR = Path(STATE_DIR) / "task-prompts"`
- `dept/dispatch_review_round.py:25` — `DEPT = os.path.join(ROOT, "dept.py")`
- `dept/dispatch_review_round.py:26` — `LENSES = ("correctness", "simplicity", "tests")`
- `dept/dispatch_review_round.py:27` — `SSH_BASE = ssh_base(CONNECTION)`
- `dept/dispatch_review_round.py:28` — `SNAPSHOT_ROOT = CONNECTION.get(`
- `dept/dispatch_review_round.py:31` — `FULL_TEMPLATE = """# Independent PR review — {lens}`
- `dept/dispatch_review_round.py:52` — `def run(*args):`
- `dept/dispatch_review_round.py:56` — `def mac(command):`
- `dept/dispatch_review_round.py:65` — `def pr_info(pr, repo):`
- `dept/dispatch_review_round.py:73` — `def repo_key(repo):`
- `dept/dispatch_review_round.py:79` — `def stored_rounds(repo, pr):`
- `dept/dispatch_review_round.py:93` — `def round_files(repo, pr):`
- `dept/dispatch_review_round.py:99` — `def next_round_number(repo, pr):`
- `dept/dispatch_review_round.py:104` — `def task_id(output):`
- `dept/dispatch_review_round.py:112` — `def round_seed_lock(repo, pr):`
- `dept/dispatch_review_round.py:121` — `def create_review_snapshot(project_dir, head, repo, pr, round_number):`
- `dept/dispatch_review_round.py:147` — `def dispatch_reviewer(project_dir, prompt_file, planned_task_id):`
- `dept/dispatch_review_round.py:159` — `def seed(pr, repo, project_dir, max_rounds=3, lenses=LENSES):`
- `dept/dispatch_review_round.py:164` — `def _seed_locked(pr, repo, project_dir, max_rounds, lenses):`
- `dept/dispatch_review_round.py:207` — `def main(argv=None):`
- `zzd/src/routes/worktrees.rs:12` — `pub(crate) const WORKTREE_ALLOWED_ROOTS: [&str; 2] =`
- `zzd/src/routes/worktrees.rs:16` — `pub(crate) const WORKTREE_REPO_ROOT: &str = "/Users/shukant/Workspace/";`
- `zzd/src/routes/worktrees.rs:21` — `pub(crate) struct WorktreeError {`
- `zzd/src/routes/worktrees.rs:27` — `pub(crate) fn canonical_worktree_roots() -> Vec<PathBuf> {`
- `zzd/src/routes/worktrees.rs:41` — `pub(crate) fn configured_worktree_repo_root() -> PathBuf {`
- `zzd/src/routes/worktrees.rs:48` — `pub(crate) fn canonical_path_under_roots(`
- `zzd/src/routes/worktrees.rs:68` — `pub(crate) fn resolve_new_worktree_path(`
- `zzd/src/routes/worktrees.rs:93` — `pub(crate) fn resolve_existing_worktree_path(`
- `zzd/src/routes/worktrees.rs:108` — `pub(crate) fn resolve_worktree_repo(raw: &str, repo_root: &Path) -> Result<PathBuf, WorktreeError> {`
- `zzd/src/routes/worktrees.rs:132` — `pub(crate) fn valid_worktree_branch(branch: &str) -> bool {`
- `zzd/src/routes/worktrees.rs:150` — `pub(crate) fn git_output(`
- `zzd/src/routes/worktrees.rs:169` — `pub(crate) fn worktree_branch_exists(repo: &Path, branch: &str) -> Result<bool, WorktreeError> {`
- `zzd/src/routes/worktrees.rs:190` — `pub(crate) fn worktree_branch_checked_out(`
- `zzd/src/routes/worktrees.rs:208` — `pub(crate) fn worktree_create_plan(`
- `zzd/src/routes/worktrees.rs:276` — `pub(crate) fn worktree_delete_plan(`
- `zzd/src/routes/worktrees.rs:355` — `pub(crate) fn worktree_request_fields(`
- `zzd/src/routes/worktrees.rs:384` — `pub(crate) fn worktree_string_field(`
- `zzd/src/routes/worktrees.rs:399` — `pub(crate) fn worktree_create(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {`
- `zzd/src/routes/worktrees.rs:419` — `pub(crate) fn worktree_delete(`
- `zzd/src/routes/events.rs:5` — `pub(crate) fn run_timeline(arguments: &[String]) -> Result<(), String> {`
- `zzd/src/routes/events.rs:37` — `pub(crate) fn timeline_output(task_id: &str, events: &[Json]) -> String {`
- `zzd/src/routes/events.rs:78` — `pub(crate) fn event_text<'a>(event: &'a Json, field: &str) -> &'a str {`
- `zzd/src/routes/events.rs:81` — `pub(crate) fn phase_events<'a>(`
- `zzd/src/routes/events.rs:99` — `pub(crate) fn same_clock_duration(left: &Json, right: &Json) -> Option<u64> {`
- `zzd/src/routes/events.rs:107` — `pub(crate) fn timestamp_millis(value: &str) -> Option<u64> {`
- `zzd/src/routes/events.rs:110` — `pub(crate) fn format_duration(millis: u64) -> String {`
- `zzd/src/review_loop.rs:23` — `const CONFIG_SCHEMA: &str = include_str!("config-v1.json");`
- `zzd/src/review_loop.rs:24` — `const REVIEWER_RESULT_SCHEMA: &str = include_str!("reviewer-result-v1.json");`
- `zzd/src/review_loop.rs:25` — `const REVIEWER_BIN: &str = "codex-review-launch";`
- `zzd/src/review_loop.rs:26` — `const OWNER_BIN: &str = "codex-launch";`
- `zzd/src/review_loop.rs:27` — `const GITHUB_CONNECTION_LIMIT: usize = 100;`
- `zzd/src/review_loop.rs:28` — `const COMPARE_FILES_JQ: &str = r#"{file_count: (.files | length), files: (.files | map({filename, previous_filename, status, additions, deletions, patch}))}"#;`
- `zzd/src/review_loop.rs:32` — `pub struct PersonalConfig {`
- `zzd/src/review_loop.rs:39` — `pub struct ReviewLoopConfig {`
- `zzd/src/review_loop.rs:47` — `pub struct Intervals {`
- `zzd/src/review_loop.rs:55` — `pub struct RepositoryPolicy {`
- `zzd/src/review_loop.rs:70` — `pub struct RequiredCheck {`
- `zzd/src/review_loop.rs:77` — `pub struct ResultLimits {`
- `zzd/src/review_loop.rs:83` — `pub struct ConfigViolation {`
- `zzd/src/review_loop.rs:98` — `pub fn default_config_path() -> Result<PathBuf, ConfigViolation> {`
- `zzd/src/review_loop.rs:108` — `pub fn load_config(path: &Path) -> Result<PersonalConfig, Vec<ConfigViolation>> {`
- `zzd/src/review_loop.rs:176` — `pub fn gate_report(`
- `zzd/src/review_loop.rs:188` — `fn gate_report_with(`
- `zzd/src/review_loop.rs:212` — `fn reject_yaml_tags(value: &serde_yaml::Value, path: &str, errors: &mut Vec<ConfigViolation>) {`
- `zzd/src/review_loop.rs:238` — `fn json_path(pointer: &str) -> String {`
- `zzd/src/review_loop.rs:258` — `enum RoundPhase {`
- `zzd/src/review_loop.rs:270` — `struct ReviewerState {`
- `zzd/src/review_loop.rs:276` — `struct AdmittedVerdict {`
- `zzd/src/review_loop.rs:284` — `enum Verdict {`
- `zzd/src/review_loop.rs:291` — `struct ReviewerResult {`
- `zzd/src/review_loop.rs:300` — `struct OwnerContext {`
- `zzd/src/review_loop.rs:308` — `struct OwnerSpawnFence<'a> {`
- `zzd/src/review_loop.rs:316` — `struct OwnerResolution<IsCurrent, Resolve> {`
- `zzd/src/review_loop.rs:322` — `struct ReviewRound {`
- `zzd/src/review_loop.rs:346` — `struct DurableState {`
- `zzd/src/review_loop.rs:353` — `fn state_schema_version() -> u64 {`
- `zzd/src/review_loop.rs:357` — `fn initial_generation() -> u64 {`
- `zzd/src/review_loop.rs:361` — `struct StateStore {`
- `zzd/src/review_loop.rs:409` — `struct PullRequestSnapshot {`
- `zzd/src/review_loop.rs:423` — `struct Comment {`
- `zzd/src/review_loop.rs:432` — `struct RestComment {`
- `zzd/src/review_loop.rs:441` — `struct Author {`
- `zzd/src/review_loop.rs:446` — `struct Check {`
- `zzd/src/review_loop.rs:455` — `struct CreatedComment {`
- `zzd/src/review_loop.rs:460` — `struct GateDecision {`
- `zzd/src/review_loop.rs:465` — `pub fn start(`
- `zzd/src/review_loop.rs:512` — `fn validate_review_transport(`
- `zzd/src/review_loop.rs:527` — `pub(crate) fn authoritative_mode(shadow: bool) -> bool {`
- `zzd/src/review_loop.rs:531` — `fn supersede_rounds_for_head(`
- `zzd/src/review_loop.rs:566` — `fn round_agent_ids(round: &ReviewRound) -> Vec<String> {`
- `zzd/src/review_loop.rs:577` — `fn planned_attempt(reviewer: Option<&ReviewerState>, maximum: u64) -> Option<u64> {`
- `zzd/src/review_loop.rs:584` — `fn mark_reviewer_exhausted(round: &mut ReviewRound, lens: &str, maximum: u64) {`
- `zzd/src/review_loop.rs:592` — `fn apply_gate_decision(round: &mut ReviewRound, decision: &GateDecision) {`
- `zzd/src/review_loop.rs:601` — `fn comparison_matches(round: &ReviewRound, snapshot: &PullRequestSnapshot) -> bool {`
- `zzd/src/review_loop.rs:605` — `fn pull_request_is_open(snapshot: &PullRequestSnapshot) -> bool {`
- `zzd/src/review_loop.rs:609` — `fn pull_request_is_merged(snapshot: &PullRequestSnapshot) -> bool {`
- `zzd/src/review_loop.rs:613` — `fn open_comparison_matches(`
- `zzd/src/review_loop.rs:623` — `fn stage_agent_cleanup(round: &mut ReviewRound, cleanup_required: bool) -> Vec<String> {`
- `zzd/src/review_loop.rs:631` — `fn complete_agent_cleanup(store: &mut StateStore, agents: &[String]) -> Result<(), String> {`
- `zzd/src/review_loop.rs:644` — `fn mark_round_superseded(`
- `zzd/src/review_loop.rs:657` — `fn supersede_stale_round(`
- `zzd/src/review_loop.rs:682` — `fn terminalize_closed_round(`
- `zzd/src/review_loop.rs:715` — `fn save_or_restore(store: &mut StateStore, previous: DurableState) -> Result<(), String> {`
- `zzd/src/review_loop.rs:723` — `fn resolve_reviewer_agent(`
- `zzd/src/review_loop.rs:734` — `fn dispatch_reviewer_attempt(`
- `zzd/src/review_loop.rs:791` — `fn discover(`
- `zzd/src/review_loop.rs:807` — `fn discover_with(`
- `zzd/src/review_loop.rs:927` — `fn replacement_comment_state(`
- `zzd/src/review_loop.rs:939` — `fn dispatch_missing_reviewers(`
- `zzd/src/review_loop.rs:1035` — `fn stage_unadmitted_generated_comments(`
- `zzd/src/review_loop.rs:1063` — `fn trusted_generated_comment(`
- `zzd/src/review_loop.rs:1077` — `fn retry_pending_comment_deletions(`
- `zzd/src/review_loop.rs:1088` — `fn retry_pending_comment_deletions_with(`
- `zzd/src/review_loop.rs:1121` — `fn retry_all_pending_comment_deletions(config: &ReviewLoopConfig, store: &mut StateStore) {`
- `zzd/src/review_loop.rs:1144` — `fn prepare_inactive_comment_cleanup(`
- `zzd/src/review_loop.rs:1166` — `fn prepare_superseded_comment_cleanup(`
- `zzd/src/review_loop.rs:1187` — `fn poll_reviews(`
- `zzd/src/review_loop.rs:1198` — `fn poll_reviews_with(`
- `zzd/src/review_loop.rs:1386` — `fn poll_merges(`
- `zzd/src/review_loop.rs:1397` — `fn poll_merges_with(`
- `zzd/src/review_loop.rs:1460` — `fn fetch_pr(repository: &str, number: u64) -> Result<PullRequestSnapshot, String> {`
- `zzd/src/review_loop.rs:1476` — `fn parse_pr_snapshot(result: &str) -> Result<PullRequestSnapshot, String> {`
- `zzd/src/review_loop.rs:1487` — `fn fetch_pr_comments(repository: &str, number: u64) -> Result<Vec<Comment>, String> {`
- `zzd/src/review_loop.rs:1502` — `fn parse_paginated_comments(result: &str) -> Result<Vec<Comment>, String> {`
- `zzd/src/review_loop.rs:1522` — `fn fetch_pr_diff(`
- `zzd/src/review_loop.rs:1546` — `fn bounded_compare_material(response: &str) -> Result<String, String> {`
- `zzd/src/review_loop.rs:1586` — `fn run_allowed(bin: &str, args: Vec<String>, id: String) -> Result<String, String> {`
- `zzd/src/review_loop.rs:1606` — `fn latest_verdicts(`
- `zzd/src/review_loop.rs:1672` — `fn parse_verdict_comment(`
- `zzd/src/review_loop.rs:1720` — `fn parse_reviewer_result(`
- `zzd/src/review_loop.rs:1745` — `fn reviewer_result_validator() -> &'static jsonschema::Validator {`
- `zzd/src/review_loop.rs:1756` — `fn collect_completed_reviewers(`
- `zzd/src/review_loop.rs:1833` — `fn publish_reviewer_result_with(`
- `zzd/src/review_loop.rs:1887` — `fn post_verdict_comment(`
- `zzd/src/review_loop.rs:1962` — `fn delete_verdict_comment(`
- `zzd/src/review_loop.rs:2018` — `fn evaluate_gate(`
- `zzd/src/review_loop.rs:2073` — `fn active_gate_round<'a>(`
- `zzd/src/review_loop.rs:2089` — `fn gate_report_for_mode(`
- `zzd/src/review_loop.rs:2146` — `fn resume_owner(`
- `zzd/src/review_loop.rs:2182` — `fn resume_owner_with<IsCurrent, Resolve>(`
- `zzd/src/review_loop.rs:2274` — `fn spawn_codex_task(`
- `zzd/src/review_loop.rs:2362` — `fn reusable_managed_agent_with_fence<F>(`
- `zzd/src/review_loop.rs:2380` — `fn validate_owner_spawn_fence(fence: &OwnerSpawnFence<'_>) -> Result<(), String> {`
- `zzd/src/review_loop.rs:2391` — `fn write_private(path: PathBuf, bytes: &[u8]) -> Result<(), String> {`
- `zzd/src/review_loop.rs:2407` — `fn emit_decision(`
- `zzd/src/review_loop.rs:2437` — `fn find_owner_context(repository: &str, branch: &str, head: &str) -> Option<OwnerContext> {`
- `zzd/src/review_loop.rs:2491` — `fn git_output(directory: &Path, args: &[&str]) -> Option<String> {`
- `zzd/src/review_loop.rs:2504` — `fn git_matches_repository(directory: &Path, repository: &str) -> bool {`
- `zzd/src/review_loop.rs:2512` — `fn git_matches_owner_checkout(`
- `zzd/src/review_loop.rs:2523` — `fn owner_context_is_current(owner: &OwnerContext, repository: &str, head: &str) -> bool {`
- `zzd/src/review_loop.rs:2535` — `fn department_dir() -> Option<PathBuf> {`
- `zzd/src/review_loop.rs:2539` — `fn owner_context_matches_task(department: &Path, owner: &OwnerContext) -> bool {`
- `zzd/src/review_loop.rs:2556` — `fn canonical_github_repository(remote: &str) -> Option<String> {`
- `zzd/src/review_loop.rs:2582` — `fn session_from_events(path: &Path) -> Option<String> {`
- `zzd/src/review_loop.rs:2599` — `fn latest_agent_for_task(registry: &AgentRegistry, task_id: &str) -> Option<String> {`
- `zzd/src/review_loop.rs:2607` — `fn registered_agent_running(registry: &AgentRegistry, agent_id: &str) -> bool {`
- `zzd/src/review_loop.rs:2614` — `fn latest_managed_agent_for_task(registry: &AgentRegistry, task_id: &str) -> Option<String> {`
- `zzd/src/review_loop.rs:2623` — `fn kill_agents_with(`
- `zzd/src/review_loop.rs:2650` — `fn kill_agents(server: &Server, agent_ids: &[String]) {`
- `zzd/src/review_loop.rs:2687` — `fn retry_pending_agent_cleanup(server: &Server, store: &mut StateStore) -> Result<(), String> {`
- `zzd/src/review_loop.rs:2702` — `fn home_dir() -> Option<PathBuf> {`
- `zzd/src/review_loop.rs:2706` — `fn review_workspace(task_id: &str) -> Result<PathBuf, String> {`
- `zzd/src/review_loop.rs:2712` — `fn review_task_dir(task_id: &str) -> Result<PathBuf, String> {`
- `zzd/src/review_loop.rs:2718` — `fn reviewer_prompt(repository: &str, number: u64, head: &str, lens: &str) -> String {`
- `zzd/src/review_loop.rs:2724` — `fn escape_prompt_markup(value: &str) -> String {`
- `zzd/src/review_loop.rs:2731` — `fn owner_prompt(`
- `zzd/src/review_loop.rs:2747` — `fn round_key(repository: &str, number: u64, head: &str) -> String {`
- `zzd/src/review_loop.rs:2751` — `fn reviewer_task_id(`
- `zzd/src/review_loop.rs:2766` — `fn verdict_id(`
- `zzd/src/review_loop.rs:2783` — `fn owner_task_id(repository: &str, number: u64, head: &str, generation: u64) -> String {`
- `zzd/src/review_loop.rs:2791` — `fn stable_fragment(value: &str, maximum: usize) -> String {`
- `zzd/src/review_loop.rs:2805` — `fn stable_identifier(value: &str, maximum: usize) -> String {`
- `zzd/src/e2e.rs:28` — `fn unique_id(prefix: &str) -> String {`
- `zzd/src/e2e.rs:33` — `fn json_array(value: &Json) -> &[Json] {`
- `zzd/src/e2e.rs:40` — `fn agent_create_test_repo(id: &str) -> PathBuf {`
- `zzd/src/e2e.rs:67` — `fn e2e_spawn_to_completion_records_logs_and_events() {`
- `zzd/src/e2e.rs:191` — `fn e2e_agent_create_list_pause_resume_delete_without_exec_policy() {`
- `zzd/src/e2e.rs:292` — `fn e2e_agent_create_persists_transcript_and_serves_it() {`
- `zzd/src/e2e.rs:358` — `fn e2e_failed_spawn_records_exit_code() {`
- `zzd/src/e2e.rs:396` — `fn e2e_exec_honors_allowlist() {`
- `zzd/src/e2e.rs:464` — `fn e2e_auth_rejects_bad_tokens() {`
- `zzd/src/e2e.rs:512` — `fn e2e_spawn_without_policy_fails_closed() {`
- `zzd/src/e2e.rs:558` — `fn e2e_proc_kill_lifecycle() {`
- `zzd/src/e2e.rs:617` — `fn e2e_worktree_create_delete_roundtrip() {`
- `zzd/src/e2e.rs:668` — `fn e2e_update_policy_parses() {`
- `dept/events.py:17` — `DEPARTMENT_KINDS = frozenset(`
- `dept/events.py:28` — `def utc_millis(now: dt.datetime | None = None) -> str:`
- `dept/events.py:33` — `def department_clock() -> str:`
- `dept/events.py:43` — `def transition_event(`
- `dept/events.py:71` — `def post_event(`
- `zzd/src/routes/agents.rs:25` — `const AGENT_STOP_GRACE: Duration = Duration::from_secs(10);`
- `zzd/src/routes/agents.rs:27` — `const AGENT_KILL_GRACE: Duration = Duration::from_secs(5);`
- `zzd/src/routes/agents.rs:30` — `fn agent_worktree_roots() -> Vec<PathBuf> {`
- `zzd/src/routes/agents.rs:35` — `fn agent_worktree_roots() -> Vec<PathBuf> {`
- `zzd/src/routes/agents.rs:44` — `fn agent_worktree_repo_root() -> PathBuf {`
- `zzd/src/routes/agents.rs:49` — `fn agent_worktree_repo_root() -> PathBuf {`
- `zzd/src/routes/agents.rs:59` — `fn agent_codex_path() -> &'static Path {`
- `zzd/src/routes/agents.rs:64` — `fn agent_codex_path() -> &'static Path {`
- `zzd/src/routes/agents.rs:83` — `pub(crate) enum AgentRoute<'a> {`
- `zzd/src/routes/agents.rs:92` — `pub(crate) fn agent_route<'a>(method: &'a str, path: &'a str) -> Option<AgentRoute<'a>> {`
- `zzd/src/routes/agents.rs:115` — `pub(crate) fn agent_request(`
- `zzd/src/routes/agents.rs:221` — `pub(crate) fn agent_post_request(`
- `zzd/src/routes/agents.rs:235` — `fn pause_agent_request(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {`
- `zzd/src/routes/agents.rs:270` — `fn resume_agent_request(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {`
- `zzd/src/routes/agents.rs:296` — `fn transcript_json(`
- `zzd/src/routes/agents.rs:366` — `fn agent_logs_json(`
- `zzd/src/routes/agents.rs:428` — `fn dept_task_dir(task_id: &str) -> Option<std::path::PathBuf> {`
- `zzd/src/routes/agents.rs:437` — `pub(crate) struct AgentCreateRequest {`
- `zzd/src/routes/agents.rs:447` — `pub(crate) fn parse_agent_create_request(body: &[u8]) -> Result<AgentCreateRequest, &'static str> {`
- `zzd/src/routes/agents.rs:494` — `pub(crate) fn default_agent_worktree(branch: &str) -> String {`
- `zzd/src/routes/agents.rs:499` — `pub(crate) fn valid_agent_model(model: &str) -> bool {`
- `zzd/src/routes/agents.rs:507` — `pub(crate) enum AgentWorktreeFailure {`
- `zzd/src/routes/agents.rs:512` — `pub(crate) fn agent_create_worktree(`
- `zzd/src/routes/agents.rs:556` — `fn agent_create_request(`
- `zzd/src/routes/agents.rs:761` — `pub(crate) fn agent_delete(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {`
- `zzd/src/routes/agents.rs:818` — `fn stop_process_group(id: &str, pgid: i32, supervisor: &Supervisor) -> (bool, bool) {`
- `zzd/src/routes/agents.rs:837` — `fn wait_for_group_exit(id: &str, pgid: i32, timeout: Duration, supervisor: &Supervisor) -> bool {`
- `zzd/src/session.rs:6` — `pub(crate) const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;`
- `zzd/src/session.rs:8` — `pub(crate) const SESSION_IS_REMOTE: u32 = 0x1000;`
- `zzd/src/session.rs:10` — `pub(crate) struct CappedOutput {`
- `zzd/src/session.rs:32` — `pub(crate) fn is_local_gui_session(status: i32, attributes: u32) -> bool {`
- `zzd/src/session.rs:41` — `pub(crate) fn require_gui_login_session() -> Result<(), String> {`
- `zzd/src/session.rs:63` — `pub(crate) fn require_gui_login_session() -> Result<(), String> {`
- `zzd/src/session.rs:66` — `pub(crate) fn resolve_tailscale_ip() -> Result<IpAddr, String> {`
- `zzd/src/session.rs:80` — `pub(crate) fn is_tailscale_ipv4(address: IpAddr) -> bool {`
- `dept/dept_config.py:11` — `ROOT = os.path.dirname(os.path.abspath(__file__))`
- `dept/dept_config.py:14` — `def config_path():`
- `dept/dept_config.py:19` — `def load_config():`
- `dept/dept_config.py:30` — `def state_dir(config):`
- `dept/dept_config.py:36` — `def ssh_base(connection):`
- `dept/dept_config.py:51` — `def ssh_env():`
- `zzd/src/update.rs:19` — `pub const REPOSITORY: &str = "ShukantPal/zigzag";`
- `zzd/src/update.rs:20` — `const WORKFLOW: &str = "ShukantPal/zigzag/.github/workflows/ci.yml";`
- `zzd/src/update.rs:21` — `const TARGET: &str = "aarch64-apple-darwin";`
- `zzd/src/update.rs:22` — `const BINARY: &str = "zigzag-macos-aarch64";`
- `zzd/src/update.rs:23` — `const MANIFEST: &str = "zigzag-macos-aarch64.manifest.json";`
- `zzd/src/update.rs:24` — `const TRUST_ROOT: &str = include_str!("../trust/sigstore-trusted-root.json");`
- `zzd/src/update.rs:32` — `const TEAM_ID: &str = "7YZK8D3B48";`
- `zzd/src/update.rs:35` — `pub enum Policy {`
- `zzd/src/update.rs:66` — `pub struct Config {`
- `zzd/src/update.rs:73` — `pub struct Manager {`
- `zzd/src/update.rs:79` — `type ActiveWork = dyn Fn() -> bool + Send + Sync;`
- `zzd/src/update.rs:80` — `type Audit = dyn Fn(&str, Json) + Send + Sync;`
- `zzd/src/update.rs:82` — `struct WatchdogConfig {`
- `zzd/src/update.rs:94` — `struct CommandOutput {`
- `zzd/src/update.rs:112` — `struct SystemRuntime;`
- `zzd/src/update.rs:196` — `pub struct Status {`
- `zzd/src/update.rs:204` — `struct Manifest {`
- `zzd/src/update.rs:545` — `pub fn run_control(arguments: &[String]) -> Result<(), String> {`
- `zzd/src/update.rs:577` — `pub fn persistent_server_args(arguments: &[String]) -> Vec<String> {`
- `zzd/src/update.rs:590` — `pub fn run_watchdog(arguments: &[String]) -> Result<(), String> {`
- `zzd/src/update.rs:600` — `fn run_watchdog_with(`
- `zzd/src/update.rs:627` — `fn promote_candidate(config: &WatchdogConfig) -> Result<(), String> {`
- `zzd/src/update.rs:637` — `fn release_asset_url(release: &Json, expected: &str) -> Result<String, String> {`
- `zzd/src/update.rs:655` — `fn parse_manifest(text: &str) -> Result<Manifest, String> {`
- `zzd/src/update.rs:680` — `fn verify_codesign(runtime: &dyn Runtime, candidate: &Path) -> Result<(), String> {`
- `zzd/src/update.rs:717` — `fn verify_attestation(`
- `zzd/src/update.rs:749` — `fn download(runtime: &dyn Runtime, url: &str, output: &Path) -> Result<(), String> {`
- `zzd/src/update.rs:774` — `fn command_output<'a>(`
- `zzd/src/update.rs:782` — `fn command_status<'a>(`
- `zzd/src/update.rs:791` — `fn update_payload(old: &Option<String>, new: &str, digest: &str) -> Json {`
- `zzd/src/update.rs:802` — `fn failure_payload(old: &Option<String>, new: &str, digest: &str, reason: &str) -> Json {`
- `zzd/src/update.rs:810` — `fn parse_control(arguments: &[String]) -> Result<(PathBuf, Vec<String>), String> {`
- `zzd/src/update.rs:822` — `fn parse_watchdog(arguments: &[String]) -> Result<WatchdogConfig, String> {`
- `zzd/src/update.rs:872` — `fn healthy(port: u16, token: &str) -> bool {`
- `zzd/src/update.rs:888` — `fn status_path(directory: &Path) -> PathBuf {`
- `zzd/src/update.rs:891` — `fn read_status(directory: &Path) -> Result<Status, String> {`
- `zzd/src/update.rs:920` — `fn save_status(directory: &Path, status: &Status) -> Result<(), String> {`
- `zzd/src/update.rs:954` — `fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {`
- `zzd/src/update.rs:976` — `fn replace_symlink(target: &Path, link: &Path) -> Result<(), String> {`
- `zzd/src/update.rs:992` — `fn valid_version(value: &str) -> bool {`
- `zzd/src/update.rs:998` — `fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {`
- `zzd/src/update.rs:1008` — `fn valid_sha256(value: &str) -> bool {`
- `zzd/src/update.rs:1011` — `fn valid_commit(value: &str) -> bool {`
- `zzd/src/update.rs:1015` — `fn sha256_file(path: &Path) -> Result<String, String> {`
- `zzd/src/server.rs:29` — `pub(crate) const MAX_CONNECTIONS: usize = 32;`
- `zzd/src/server.rs:30` — `pub(crate) struct Server {`
- `zzd/src/server.rs:42` — `pub(crate) struct Supervisor {`
- `zzd/src/server.rs:49` — `pub(crate) struct ConnectionLimiter {`
- `zzd/src/server.rs:54` — `pub(crate) struct ConnectionPermit {`
- `zzd/src/server.rs:90` — `pub(crate) fn serve(listener: TcpListener, state: Arc<Server>, limiter: Arc<ConnectionLimiter>) {`
- `zzd/src/server.rs:110` — `pub(crate) fn handle(stream: TcpStream, state: Arc<Server>) -> Result<(), String> {`
- `zzd/src/server.rs:115` — `pub(crate) fn handle_with_policy<F>(`
- `zzd/src/server.rs:125` — `pub(crate) fn handle_with_services<F, G>(`
- `zzd/src/server.rs:243` — `pub(crate) fn post(stream: &mut TcpStream, state: &Server, body: Vec<u8>) -> Result<(), String> {`
- `zzd/src/server.rs:291` — `pub(crate) fn get(stream: &mut TcpStream, state: &Server, target: &str) -> Result<(), String> {`
- `dept/jules_pr_reviewer.py:16` — `CONFIG = load_config()`
- `dept/jules_pr_reviewer.py:17` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/jules_pr_reviewer.py:18` — `WATCHER = CONFIG.get("jules_pr_reviewer", {})`
- `dept/jules_pr_reviewer.py:19` — `STATE_DIR = state_dir(CONFIG)`
- `dept/jules_pr_reviewer.py:20` — `STATE_FILE = os.path.join(STATE_DIR, "jules-pr-reviews.json")`
- `dept/jules_pr_reviewer.py:21` — `PROMPT_DIR = os.path.join(STATE_DIR, "prompts")`
- `dept/jules_pr_reviewer.py:22` — `DEPT = os.path.join(ROOT, "dept.py")`
- `dept/jules_pr_reviewer.py:23` — `PROJECT = WATCHER.get("project", "")`
- `dept/jules_pr_reviewer.py:24` — `REPO = WATCHER.get("repo", "")`
- `dept/jules_pr_reviewer.py:25` — `SSH = ssh_base(CONNECTION)`
- `dept/jules_pr_reviewer.py:26` — `ENV = ssh_env()`
- `dept/jules_pr_reviewer.py:27` — `LOCK_FILE = os.path.join(STATE_DIR, "jules-pr-reviews.lock")`
- `dept/jules_pr_reviewer.py:32` — `JULES_SESSIONS = WATCHER.get("jules_sessions", {})`
- `dept/jules_pr_reviewer.py:34` — `REVIEW_PROMPT = """# Review Jules PR #{pr} and post findings as a PR comment`
- `dept/jules_pr_reviewer.py:63` — `REREVIEW_PROMPT = """# Re-review Jules PR #{pr} (new commits pushed) and post findings as a PR comment`
- `dept/jules_pr_reviewer.py:91` — `def load_state():`
- `dept/jules_pr_reviewer.py:99` — `def save_state(state):`
- `dept/jules_pr_reviewer.py:105` — `def open_jules_prs():`
- `dept/jules_pr_reviewer.py:129` — `def task_status(task_id):`
- `dept/jules_pr_reviewer.py:140` — `def dispatch(prompt_template, pr, extra):`
- `dept/jules_pr_reviewer.py:158` — `def main():`
- `dept/dept.py:24` — `CONFIG = load_config()`
- `dept/dept.py:25` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/dept.py:26` — `STATE_DIR = state_dir(CONFIG)`
- `dept/dept.py:27` — `MAC = os.environ.get("CODEX_DEPT_MAC", CONNECTION.get("mac", ""))`
- `dept/dept.py:28` — `SSH_KEY = os.path.expanduser(CONNECTION.get("ssh_key", ""))`
- `dept/dept.py:29` — `PROXY_HELPER = os.path.expanduser(CONNECTION.get("proxy_helper", ""))`
- `dept/dept.py:31` — `REMOTE_DEPT = CONNECTION.get("remote_dept", "")`
- `dept/dept.py:32` — `LEDGER = os.path.join(STATE_DIR, "ledger.jsonl")`
- `dept/dept.py:33` — `ZIGZAG_URL = os.environ.get("ZIGZAG_URL", CONNECTION.get("zigzag_url", ""))`
- `dept/dept.py:34` — `ZIGZAG_TOKEN_FILE = os.path.expanduser(CONNECTION.get("zigzag_token_file", ""))`
- `dept/dept.py:37` — `RELAY_LAUNCHER = CONNECTION.get("relay_launcher", "codex-launch")`
- `dept/dept.py:40` — `def asset_path(name):`
- `dept/dept.py:44` — `def tunnel_proxy():`
- `dept/dept.py:52` — `def ssh(*remote_cmd, stdin_data=None, timeout=60):`
- `dept/dept.py:70` — `def _zigzag_opener():`
- `dept/dept.py:77` — `def _zigzag_call(method, path, body=None, timeout=30):`
- `dept/dept.py:101` — `def zigzag_spawn(bin_name, args, ident):`
- `dept/dept.py:112` — `def relay_events_snapshot():`
- `dept/dept.py:128` — `def relay_task_outcome(tid):`
- `dept/dept.py:155` — `def relay_stderr_tail(agent_id):`
- `dept/dept.py:178` — `def zigzag_kill(handle):`
- `dept/dept.py:187` — `def ledger_append(entry):`
- `dept/dept.py:193` — `def ledger_read():`
- `dept/dept.py:204` — `def dispatch_args(args, resume=False):`
- `dept/dept.py:230` — `def decorated_prompt(ns):`
- `dept/dept.py:247` — `MODEL_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]*\Z")`
- `dept/dept.py:248` — `TASK_ID_RE = re.compile(r"t-[0-9a-f]{6}\Z")`
- `dept/dept.py:251` — `def validate_model(model):`
- `dept/dept.py:256` — `def ssh_worker_script(rdir, codex):`
- `dept/dept.py:271` — `def ssh_kill_script(rdir):`
- `dept/dept.py:283` — `def setup_task_dir(tid, project_dir, prompt, session_id=None, model=None, read_only=False):`
- `dept/dept.py:303` — `def dispatch_task(project_dir, prompt, use_ssh, session_id=None, model=None,`
- `dept/dept.py:356` — `def cmd_start(args):`
- `dept/dept.py:362` — `def shq(s):`
- `dept/dept.py:366` — `def session_meta_cwd(lines):`
- `dept/dept.py:380` — `def single_cwd(candidates):`
- `dept/dept.py:386` — `def resolve_session_cwd(session_id):`
- `dept/dept.py:426` — `def remote_isdir(path):`
- `dept/dept.py:431` — `def remote_status_detail(entry):`
- `dept/dept.py:453` — `def remote_status(entry):`
- `dept/dept.py:457` — `def cmd_kill(args):`
- `dept/dept.py:472` — `def cmd_status(args):`
- `dept/dept.py:486` — `def cmd_list(args):`
- `dept/dept.py:493` — `def cmd_result(args):`
- `dept/dept.py:520` — `def token_summary(tid):`
- `dept/dept.py:528` — `def cmd_tokens(args):`
- `dept/dept.py:532` — `def reported_path():`
- `dept/dept.py:536` — `def load_reported():`
- `dept/dept.py:546` — `def save_reported(r):`
- `dept/dept.py:550` — `def cmd_resume(args):`
- `dept/dept.py:567` — `def cmd_check(args):`
- `dept/dept.py:595` — `def management_main(argv):`
- `dept/dept.py:610` — `def main(argv=None):`
- `dept/gdocs_comment_watcher.py:28` — `SERVICE_ACCOUNT = "zigzag@shukant.iam.gserviceaccount.com"`
- `dept/gdocs_comment_watcher.py:29` — `KEYCHAIN_SERVICE = "zigzag-sa"`
- `dept/gdocs_comment_watcher.py:30` — `FOLDER_ID = "1W_iTcpdYGVXj_NTmkfcgOm_GGREk1Nj3"`
- `dept/gdocs_comment_watcher.py:32` — `EXTRA_DOCUMENT_ID = "1PF8O_BoLwKetxmcuPmQRYXq4weQGXwd6vABSd62atV4"`
- `dept/gdocs_comment_watcher.py:33` — `DRIVE_SCOPE = "https://www.googleapis.com/auth/drive"`
- `dept/gdocs_comment_watcher.py:34` — `GOOGLE_DOC_MIME = "application/vnd.google-apps.document"`
- `dept/gdocs_comment_watcher.py:35` — `MARKER = "Muse (AI assistant)"`
- `dept/gdocs_comment_watcher.py:36` — `EYES = "\U0001F440"`
- `dept/gdocs_comment_watcher.py:37` — `ACK_TEXT = f"{EYES} {MARKER} — picked up, addressing it now."`
- `dept/gdocs_comment_watcher.py:39` — `CONFIG = load_config()`
- `dept/gdocs_comment_watcher.py:40` — `WATCHER = CONFIG.get("gdocs_comment_watcher", {})`
- `dept/gdocs_comment_watcher.py:41` — `STATE_ROOT = os.path.expanduser(WATCHER.get("state_root", "~/.zigzag/dept"))`
- `dept/gdocs_comment_watcher.py:42` — `DATABASE = os.path.join(STATE_ROOT, "dept.db")`
- `dept/gdocs_comment_watcher.py:43` — `PROMPT_DIR = os.path.join(STATE_ROOT, "prompts")`
- `dept/gdocs_comment_watcher.py:44` — `LOCK_FILE = os.path.join(STATE_ROOT, "drive-comment-watcher.lock")`
- `dept/gdocs_comment_watcher.py:45` — `DEPT = os.path.join(ROOT, "dept.py")`
- `dept/gdocs_comment_watcher.py:48` — `class DriveError(RuntimeError):`
- `dept/gdocs_comment_watcher.py:52` — `def b64url(value):`
- `dept/gdocs_comment_watcher.py:56` — `def read_service_account_key():`
- `dept/gdocs_comment_watcher.py:91` — `def secure_directory(path):`
- `dept/gdocs_comment_watcher.py:96` — `def secure_file(path):`
- `dept/gdocs_comment_watcher.py:103` — `def service_account_token(key, now=None):`
- `dept/gdocs_comment_watcher.py:144` — `class DriveClient:`
- `dept/gdocs_comment_watcher.py:213` — `def connect_database(path=None):`
- `dept/gdocs_comment_watcher.py:236` — `def database_initialized(db):`
- `dept/gdocs_comment_watcher.py:240` — `def mark_initialized(db):`
- `dept/gdocs_comment_watcher.py:245` — `def comment_seen(db, doc_id, comment_id):`
- `dept/gdocs_comment_watcher.py:250` — `def mark_seen(db, doc_id, comment_id):`
- `dept/gdocs_comment_watcher.py:255` — `def feedback_items(comments):`
- `dept/gdocs_comment_watcher.py:264` — `def configured_values(name):`
- `dept/gdocs_comment_watcher.py:269` — `def trusted_author(item):`
- `dept/gdocs_comment_watcher.py:276` — `def stored_ack(db, doc_id, comment_id):`
- `dept/gdocs_comment_watcher.py:283` — `def save_ack(db, doc_id, comment_id, ack_reply_id):`
- `dept/gdocs_comment_watcher.py:290` — `def owner_for(doc_id):`
- `dept/gdocs_comment_watcher.py:297` — `def task_running(task_id):`
- `dept/gdocs_comment_watcher.py:312` — `def session_busy(db, session_id):`
- `dept/gdocs_comment_watcher.py:324` — `PROMPT_TEMPLATE = """# Google Doc feedback — address Shukant's new comments`
- `dept/gdocs_comment_watcher.py:339` — `def write_prompt(doc, acked):`
- `dept/gdocs_comment_watcher.py:355` — `def dispatch(owner, prompt):`
- `dept/gdocs_comment_watcher.py:365` — `def quiet_hours(now=None):`
- `dept/gdocs_comment_watcher.py:370` — `def watched_documents(client):`
- `dept/gdocs_comment_watcher.py:379` — `def main(argv=None):`
- `dept/test_dispatch_review_round.py:11` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_dispatch_review_round.py:17` — `class DispatchReviewRoundTest(unittest.TestCase):`
- `dept/config.py:20` — `REPOS = {`
- `dept/config.py:25` — `QUIET_HOURS = {"start": "22:00", "end": "07:00"}`
- `dept/config.py:27` — `SA_EMAIL = "zigzag@shukant.iam.gserviceaccount.com"`
- `dept/config.py:28` — `DESIGN_DOCS_FOLDER_ID = "1W_iTcpdYGVXj_NTmkfcgOm_GGREk1Nj3"`
- `dept/config.py:33` — `class LoopOwnership:`
- `dept/config.py:39` — `class DocRoute:`
- `dept/config.py:45` — `class WatcherConfig:`
- `dept/config.py:52` — `LOOP_OWNERSHIP = [`
- `dept/config.py:59` — `DOC_ROUTES = [`
- `dept/config.py:66` — `WATCHERS = [`
- `dept/config.py:73` — `DOC_ROUTER_WATCH_LIST = sorted(route.doc_id for route in DOC_ROUTES)`
- `dept/config.py:77` — `MATERIALIZED_PATH = pathlib.Path(__file__).with_name("config.materialized.json")`
- `dept/config.py:80` — `def validate() -> list[str]:`
- `dept/config.py:93` — `def materialize() -> str:`
- `dept/config.py:111` — `def main(argv: list[str] | None = None) -> int:`
- `dept/test_gdocs_comment_watcher.py:15` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_gdocs_comment_watcher.py:20` — `class FakeResponse(io.StringIO):`
- `dept/test_gdocs_comment_watcher.py:28` — `class FakeDrive:`
- `dept/test_gdocs_comment_watcher.py:45` — `def human_comment(comment_id, text="feedback"):`
- `dept/test_gdocs_comment_watcher.py:55` — `class GdocsFeedbackTest(unittest.TestCase):`
- `dept/test_gdocs_comment_watcher.py:247` — `class WatcherStateMachineTest(unittest.TestCase):`
- `zz/src/lib.rs:14` — `pub struct AgentRecord {`
- `zz/src/lib.rs:107` — `pub struct AgentRegistry {`
- `zz/src/lib.rs:113` — `const AGENT_LOG_CAP: u64 = 32 * 1024 * 1024;`
- `zz/src/lib.rs:114` — `const AGENT_RETENTION: u64 = 7 * 24 * 60 * 60;`
- `zz/src/lib.rs:485` — `fn agent_json(entry: &AgentRecord) -> Json {`
- `zz/src/lib.rs:602` — `fn decode_agents(text: &str) -> (std::collections::BTreeMap<String, AgentRecord>, Vec<String>) {`
- `zz/src/lib.rs:634` — `fn decode_agent_record(value: &Json, agent_id: &str) -> Result<AgentRecord, String> {`
- `zz/src/lib.rs:710` — `fn redact(text: &mut String) -> bool {`
- `zz/src/lib.rs:738` — `pub enum Json {`
- `zz/src/lib.rs:792` — `pub fn parse_json(input: &str) -> Result<Json, String> {`
- `zz/src/lib.rs:798` — `pub fn quote(value: &str) -> String {`
- `zz/src/lib.rs:802` — `fn json_value(value: &Json) -> serde_json::Value {`
- `zz/src/lib.rs:821` — `fn json_from_value(value: serde_json::Value) -> Json {`
- `zz/src/lib.rs:840` — `pub fn read_secret_file(path: &Path) -> Result<String, String> {`
- `zz/src/lib.rs:865` — `pub struct Event {`
- `zz/src/lib.rs:887` — `pub struct ReadResult {`
- `zz/src/lib.rs:895` — `pub struct Store {`
- `zz/src/lib.rs:906` — `const AUDIT_CAP_BYTES: u64 = 20 * 1024 * 1024;`
- `zz/src/lib.rs:908` — `struct AuditStore {`
- `zz/src/lib.rs:1061` — `fn audit_filename(execution_id: &str) -> String {`
- `zz/src/lib.rs:1069` — `struct Inner {`
- `zz/src/lib.rs:1225` — `fn available(inner: &Inner, after: u64) -> bool {`
- `zz/src/lib.rs:1234` — `fn event_id(payload: &Json) -> Result<String, String> {`
- `zz/src/lib.rs:1242` — `fn validate_audit_envelope(payload: &Json) -> Result<(), String> {`
- `zz/src/lib.rs:1280` — `pub fn parse_rfc3339_millis(value: &str) -> Option<u64> {`
- `zz/src/lib.rs:1337` — `fn days_in_month(year: i64, month: i64) -> i64 {`
- `zz/src/lib.rs:1348` — `pub fn rfc3339_timestamp() -> String {`
- `zz/src/lib.rs:1364` — `fn civil_date(days_since_epoch: u64) -> (i64, u32, u32) {`
- `zz/src/lib.rs:1377` — `fn new_epoch() -> String {`
- `zz/src/lib.rs:1385` — `fn state_json(inner: &Inner) -> Json {`
- `zz/src/lib.rs:1415` — `fn decode_state(contents: &str, limit: usize) -> Result<Inner, String> {`
- `zz/src/lib.rs:1469` — `fn create_private(path: &Path) -> std::io::Result<File> {`
- `zz/src/lib.rs:1478` — `fn create_private(path: &Path) -> std::io::Result<File> {`
- `dept/test_dept.py:14` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_dept.py:19` — `class SessionResolutionTest(unittest.TestCase):`
- `dept/test_dept.py:52` — `class ResumeCliTest(unittest.TestCase):`
- `dept/test_dept.py:97` — `class CommandDispatchTest(unittest.TestCase):`
- `dept/approval_gate.py:20` — `CONFIG = load_config()`
- `dept/approval_gate.py:21` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/approval_gate.py:22` — `ZIGZAG_URL = os.environ.get("ZIGZAG_URL", CONNECTION.get("zigzag_url", ""))`
- `dept/approval_gate.py:23` — `ZIGZAG_TOKEN_FILE = os.path.expanduser(CONNECTION.get("zigzag_token_file", ""))`
- `dept/approval_gate.py:26` — `def relay_call(path):`
- `dept/approval_gate.py:47` — `def relay_proxy_url(https_proxy):`
- `dept/approval_gate.py:51` — `def fetch_gate(repo, pr):`
- `dept/approval_gate.py:56` — `def main():`
- `zzd/src/tests.rs:2` — `pub(crate) fn test_updater() -> Arc<update::Manager> {`
- `zzd/src/tests.rs:12` — `pub(crate) fn test_policy() -> exec::Policy {`
- `zzd/src/tests.rs:64` — `fn malformed_get_query_is_a_bad_request() {`
- `zzd/src/tests.rs:76` — `fn percent_decode_handles_form_encoding_edge_cases() {`
- `zzd/src/tests.rs:107` — `fn query_decoding_matches_url_crate_form_decoding() {`
- `zzd/src/tests.rs:140` — `fn review_gate_query_requires_one_repository_and_positive_pr() {`
- `zzd/src/tests.rs:159` — `fn authenticated_review_gate_route_forwards_mode_and_state_path() {`
- `zzd/src/tests.rs:237` — `fn bearer_comparison_requires_the_full_token() {`
- `zzd/src/tests.rs:250` — `fn explicit_bind_address_must_be_tailscale_ipv4() {`
- `zzd/src/tests.rs:257` — `fn get_allowlist_matches_only_its_exact_arguments() {`
- `zzd/src/tests.rs:269` — `fn allowlist_updater_accepts_only_its_exact_arguments() {`
- `zzd/src/tests.rs:296` — `fn gui_session_check_rejects_remote_or_non_graphical_sessions() {`
- `zzd/src/tests.rs:307` — `fn exec_denials_are_opaque_and_never_include_policy_data() {`
- `zzd/src/tests.rs:344` — `fn oversized_exec_request_is_an_opaque_denial_before_body_allocation() {`
- `zzd/src/tests.rs:365` — `fn truncated_exec_request_is_an_opaque_denial() {`
- `zzd/src/tests.rs:385` — `fn connection_limiter_admits_up_to_the_cap_and_releases_on_drop() {`
- `zzd/src/tests.rs:397` — `fn connection_limiter_never_exceeds_the_cap_concurrently() {`
- `zzd/src/tests.rs:421` — `fn more_than_one_hundred_headers_are_rejected() {`
- `zzd/src/tests.rs:441` — `fn exactly_one_hundred_headers_still_parse() {`
- `zzd/src/tests.rs:459` — `fn header_block_over_eight_kib_is_rejected() {`
- `zzd/src/tests.rs:479` — `fn unterminated_header_line_cannot_outgrow_the_budget() {`
- `zzd/src/tests.rs:503` — `fn reply_reports_431_as_request_header_fields_too_large() {`
- `zzd/src/tests.rs:524` — `fn github_watch_repo_validation_rejects_unscoped_or_malformed_names() {`
- `zzd/src/tests.rs:533` — `fn review_state_and_startup_mode_keep_shadow_observational_until_cutover() {`
- `zzd/src/tests.rs:573` — `fn github_pr_scan_requires_positive_numbers() {`
- `zzd/src/tests.rs:588` — `fn process_handles_are_unique_128_bit_hex_values() {`
- `zzd/src/tests.rs:602` — `fn spawn_is_rejected_while_update_drain_is_active() {`
- `zzd/src/tests.rs:621` — `fn captured_output_stops_at_the_exec_output_cap() {`
- `zzd/src/tests.rs:631` — `fn timeline_durations_never_cross_clock_boundaries() {`
- `zzd/src/tests.rs:678` — `fn recovery_replays_a_persisted_first_output_fact() {`
- `zzd/src/tests.rs:723` — `fn spawn_request_accepts_only_safe_execution_correlation_ids() {`
- `zzd/src/tests.rs:745` — `fn prune_drops_finished_entries_past_the_retention_window() {`
- `zzd/src/tests.rs:781` — `fn prune_keeps_at_most_128_finished_processes() {`
- `zzd/src/tests.rs:791` — `fn detached_process_endpoints_authenticate_and_manage_process_trees() {`
- `zzd/src/tests.rs:975` — `pub(crate) fn test_server() -> (Arc<Server>, PathBuf) {`
- `zzd/src/tests.rs:1003` — `fn test_review_config() -> review_loop::ReviewLoopConfig {`
- `zzd/src/tests.rs:1015` — `fn completed_entry() -> ProcEntry {`
- `zzd/src/tests.rs:1045` — `pub(crate) fn request_once(`
- `zzd/src/tests.rs:1062` — `fn request_once_with_gate<G>(`
- `zzd/src/tests.rs:1090` — `pub(crate) fn request_once_with_gate_token<G>(`
- `zzd/src/tests.rs:1127` — `pub(crate) fn response_json(response: String) -> Json {`
- `zzd/src/tests.rs:1131` — `pub(crate) fn spawn_for_test(`
- `zzd/src/tests.rs:1151` — `pub(crate) fn poll_until_complete(`
- `zzd/src/tests.rs:1172` — `pub(crate) fn worktree_test_base(prefix: &str) -> PathBuf {`
- `zzd/src/tests.rs:1182` — `pub(crate) fn worktree_test_roots(base: &Path) -> Vec<PathBuf> {`
- `zzd/src/tests.rs:1189` — `pub(crate) fn worktree_test_repo(base: &Path) -> PathBuf {`
- `zzd/src/tests.rs:1212` — `fn worktree_new_path_rejects_traversal_and_outside_roots() {`
- `zzd/src/tests.rs:1242` — `fn worktree_existing_path_requires_existence_under_roots() {`
- `zzd/src/tests.rs:1265` — `fn worktree_branch_validation_rejects_hostile_names() {`
- `zzd/src/tests.rs:1281` — `fn worktree_repo_must_live_under_workspace_root() {`
- `zzd/src/tests.rs:1306` — `fn worktree_create_and_delete_roundtrip() {`
- `zzd/src/tests.rs:1373` — `fn worktree_endpoints_reject_outside_roots_over_http() {`
- `zzd/src/tests.rs:1437` — `fn agent_create_request_parsing_and_helpers() {`
- `zzd/src/tests.rs:1469` — `fn agent_route_selects_create_only_for_post_collection() {`
- `zzd/src/tests.rs:1482` — `fn agent_create_route_passes_the_body_to_request_validation() {`
- `zzd/src/tests.rs:1499` — `fn agent_create_worktree_roundtrip() {`
- `zzd/src/tests.rs:1516` — `fn agent_record(id: &str) -> AgentRecord {`
- `zzd/src/tests.rs:1547` — `fn agent_delete_unknown_agent_is_404() {`
- `zzd/src/tests.rs:1567` — `fn agent_delete_terminal_agent_is_409() {`
- `zzd/src/tests.rs:1595` — `fn agent_delete_dead_process_deregisters_without_signalling() {`
- `zzd/src/tests.rs:1622` — `fn agent_delete_terminates_process_group_and_keeps_worktree() {`
- `dept/test_config.py:13` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_config.py:14` — `CONFIG_SOURCE = DEPT_DIR / "config.py"`
- `dept/test_config.py:15` — `ARTIFACT_PATH = DEPT_DIR / "config.materialized.json"`
- `dept/test_config.py:16` — `HOOK_SOURCE = DEPT_DIR.parent / "scripts" / "githooks" / "pre-commit"`
- `dept/test_config.py:17` — `INSTALLER_SOURCE = DEPT_DIR.parent / "scripts" / "install-hooks.sh"`
- `dept/test_config.py:22` — `class ConfigValidationTest(unittest.TestCase):`
- `dept/test_config.py:48` — `class ConfigMaterializationTest(unittest.TestCase):`
- `dept/test_config.py:82` — `class ConfigCheckTest(unittest.TestCase):`
- `dept/test_config.py:98` — `class HookTest(unittest.TestCase):`
- `dept/test_review_round_watcher.py:11` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_review_round_watcher.py:16` — `class ProjectBusyTest(unittest.TestCase):`
- `dept/test_codex_launch.py:8` — `LAUNCHER = pathlib.Path(__file__).with_name("codex-launch.sh")`
- `dept/test_codex_launch.py:11` — `class LauncherTest(unittest.TestCase):`
- `zzapi/src/main.rs:25` — `const DEFAULT_HOSTNAME: &str = "100.101.237.83";`
- `zzapi/src/main.rs:26` — `const DEFAULT_PORT: u16 = 8765;`
- `zzapi/src/main.rs:27` — `const DEFAULT_TOKEN_FILE: &str = ".codex/zigzag.token";`
- `zzapi/src/main.rs:30` — `const EXEC_CLIENT_TIMEOUT_SECS: u64 = 330;`
- `zzapi/src/main.rs:37` — `struct ApiError {`
- `zzapi/src/main.rs:43` — `enum Fail {`
- `zzapi/src/main.rs:65` — `struct Cli {`
- `zzapi/src/main.rs:83` — `enum Commands {`
- `zzapi/src/main.rs:156` — `enum AgentsCmd {`
- `zzapi/src/main.rs:233` — `enum WorktreesCmd {`
- `zzapi/src/main.rs:255` — `enum ProcCmd {`
- `zzapi/src/main.rs:267` — `struct Client {`
- `zzapi/src/main.rs:374` — `fn resolve_token(token_file: Option<&str>) -> Result<String, Fail> {`
- `zzapi/src/main.rs:411` — `fn read_private_token(path: &str) -> Result<String, Fail> {`
- `zzapi/src/main.rs:442` — `fn make_client(cli: &Cli) -> Result<Client, Fail> {`
- `zzapi/src/main.rs:462` — `fn relay_base(hostname: &str) -> String {`
- `zzapi/src/main.rs:477` — `fn print_table(headers: &[&str], rows: &[Vec<String>]) {`
- `zzapi/src/main.rs:509` — `fn fmt_ts(v: Option<&serde_json::Value>) -> String {`
- `zzapi/src/main.rs:524` — `fn fmt_epoch(secs: i64) -> String {`
- `zzapi/src/main.rs:543` — `fn s(v: &serde_json::Value, key: &str) -> String {`
- `zzapi/src/main.rs:552` — `fn after_u64(v: Option<&serde_json::Value>) -> Option<u64> {`
- `zzapi/src/main.rs:560` — `fn emit(client: &Client, payload: &serde_json::Value, table: impl FnOnce()) {`
- `zzapi/src/main.rs:570` — `fn json_output(payload: &serde_json::Value, streaming: bool) -> String {`
- `zzapi/src/main.rs:578` — `fn new_id() -> String {`
- `zzapi/src/main.rs:599` — `fn resolve_agent_id(client: &Client, prefix: &str) -> Result<String, ApiError> {`
- `zzapi/src/main.rs:643` — `fn cmd_agents_list(`
- `zzapi/src/main.rs:687` — `fn cmd_agents_get(client: &Client, id: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:713` — `fn cmd_agents_create(`
- `zzapi/src/main.rs:748` — `fn cmd_agents_pause(client: &Client, id: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:760` — `fn cmd_agents_resume(client: &Client, id: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:772` — `fn cmd_agents_stop(client: &Client, id: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:782` — `fn cmd_agents_logs(`
- `zzapi/src/main.rs:839` — `fn log_retention_lost(response: &serde_json::Value, after: u64) -> bool {`
- `zzapi/src/main.rs:847` — `fn cmd_worktrees_create(client: &Client, path: &str, branch: &str, repo: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:860` — `fn cmd_worktrees_delete(client: &Client, path: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:882` — `fn cmd_exec(client: &Client, bin: &str, args: &[String], id: Option<&str>) -> Result<(), Fail> {`
- `zzapi/src/main.rs:916` — `fn exec_succeeded(response: &serde_json::Value) -> bool {`
- `zzapi/src/main.rs:924` — `fn cmd_spawn(`
- `zzapi/src/main.rs:950` — `fn cmd_proc_get(client: &Client, id: &str) -> Result<(), Fail> {`
- `zzapi/src/main.rs:973` — `fn cmd_events(`
- `zzapi/src/main.rs:1024` — `struct EventProgress {`
- `zzapi/src/main.rs:1034` — `fn event_progress(response: &serde_json::Value, after: u64, epoch: &str) -> EventProgress {`
- `zzapi/src/main.rs:1057` — `fn cmd_health(client: &Client) -> Result<(), Fail> {`
- `zzapi/src/main.rs:1069` — `fn cmd_review_gate(client: &Client, repo: &str, pr: u64) -> Result<(), Fail> {`
- `zzapi/src/main.rs:1086` — `fn report_api_error(e: &ApiError) {`
- `zzapi/src/main.rs:1098` — `fn run(cli: Cli) -> Result<(), Fail> {`
- `zzapi/src/main.rs:1176` — `fn main() {`
- `dept/test_pr_comment_watcher.py:11` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_pr_comment_watcher.py:16` — `class WatermarkTest(unittest.TestCase):`
- `dept/round_state.py:8` — `def write_round(path, data):`
- `dept/pr_comment_watcher.py:33` — `MARKER = "🤖"`
- `dept/pr_comment_watcher.py:35` — `CONFIG = load_config()`
- `dept/pr_comment_watcher.py:36` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/pr_comment_watcher.py:37` — `WATCHER = CONFIG.get("pr_comment_watcher", {})`
- `dept/pr_comment_watcher.py:38` — `PRS = WATCHER.get("prs", [])`
- `dept/pr_comment_watcher.py:39` — `REPO = WATCHER.get("repo", "")`
- `dept/pr_comment_watcher.py:40` — `PROJECT_DIR = WATCHER.get("project_dir", "")`
- `dept/pr_comment_watcher.py:41` — `STATE_DIR = os.path.join(state_dir(CONFIG), "pr-watch-active")`
- `dept/pr_comment_watcher.py:42` — `WATERMARK = os.path.join(STATE_DIR, "watermark.json")`
- `dept/pr_comment_watcher.py:43` — `PROMPT_DIR = os.path.join(state_dir(CONFIG), "prompts")`
- `dept/pr_comment_watcher.py:44` — `DEPT = os.path.join(ROOT, "dept.py")`
- `dept/pr_comment_watcher.py:45` — `LEDGER = os.path.join(state_dir(CONFIG), "ledger.jsonl")`
- `dept/pr_comment_watcher.py:46` — `SESSIONS_FILE = os.path.join(state_dir(CONFIG), "pr_sessions.json")`
- `dept/pr_comment_watcher.py:47` — `BURST_FILE = os.path.join(STATE_DIR, "burst.json")`
- `dept/pr_comment_watcher.py:48` — `LOCK_FILE = os.path.join(STATE_DIR, "watcher.lock")`
- `dept/pr_comment_watcher.py:49` — `BURST_MINUTES = 30  # burst window; sliding-extended while his comments keep arriving`
- `dept/pr_comment_watcher.py:52` — `def burst_active():`
- `dept/pr_comment_watcher.py:60` — `def burst_extend(minutes=BURST_MINUTES, reason=""):`
- `dept/pr_comment_watcher.py:74` — `def burst_clear():`
- `dept/pr_comment_watcher.py:81` — `def load_sessions():`
- `dept/pr_comment_watcher.py:92` — `def log(msg):`
- `dept/pr_comment_watcher.py:96` — `def dept_status_text(tid):`
- `dept/pr_comment_watcher.py:105` — `def task_finished(status):`
- `dept/pr_comment_watcher.py:111` — `def pr_task_running(pr):`
- `dept/pr_comment_watcher.py:149` — `SSH_BASE = ssh_base(CONNECTION)`
- `dept/pr_comment_watcher.py:152` — `def mac(cmd):`
- `dept/pr_comment_watcher.py:160` — `def add_reaction(pr, c, content):`
- `dept/pr_comment_watcher.py:187` — `def load_watermark():`
- `dept/pr_comment_watcher.py:207` — `def save_watermark(wm):`
- `dept/pr_comment_watcher.py:214` — `def pr_head_branch(n):`
- `dept/pr_comment_watcher.py:220` — `def review_comments(n):`
- `dept/pr_comment_watcher.py:244` — `def issue_comments(n):`
- `dept/pr_comment_watcher.py:259` — `def review_bodies(n):`
- `dept/pr_comment_watcher.py:281` — `PROMPT_TMPL = """# PR follow-up (watcher-dispatched): address Shukant's review comments on leveled#{pr}`
- `dept/pr_comment_watcher.py:333` — `def comment_key(c):`
- `dept/pr_comment_watcher.py:341` — `DECL_RE = re.compile(r"^[\w\s]*\b(class|struct|enum|actor|protocol)\s+([A-Za-z_]\w*)")`
- `dept/pr_comment_watcher.py:344` — `def hunk_new_lines(diff_hunk):`
- `dept/pr_comment_watcher.py:361` — `def resolve_symbol(diff_hunk, original_line):`
- `dept/pr_comment_watcher.py:378` — `def hunk_window(diff_hunk, original_line, radius=18, max_lines=40):`
- `dept/pr_comment_watcher.py:387` — `def dispatch(pr, branch, new_comments, parent_bodies):`
- `dept/pr_comment_watcher.py:428` — `def _dispatch_locked(pr, session_id, prompt, new_comments):`
- `dept/pr_comment_watcher.py:472` — `def main():`
- `dept/review_round_watcher.py:38` — `CONFIG = load_config()`
- `dept/review_round_watcher.py:39` — `CONNECTION = CONFIG.get("connection", {})`
- `dept/review_round_watcher.py:40` — `STATE_DIR = state_dir(CONFIG)`
- `dept/review_round_watcher.py:41` — `ROUNDS_DIR = os.path.join(STATE_DIR, "review_rounds")`
- `dept/review_round_watcher.py:42` — `DEPT = os.path.join(ROOT, "dept.py")`
- `dept/review_round_watcher.py:43` — `LEDGER = os.path.join(STATE_DIR, "ledger.jsonl")`
- `dept/review_round_watcher.py:44` — `REMOTE_DEPT = CONNECTION.get("remote_dept", "~/.codex/dept")`
- `dept/review_round_watcher.py:45` — `LOCK_FILE = os.path.join(ROUNDS_DIR, "watcher.lock")`
- `dept/review_round_watcher.py:48` — `WATCHER_LOCK = os.path.join(state_dir(CONFIG), "worker-dispatch.lock")`
- `dept/review_round_watcher.py:49` — `SESSIONS_FILE = os.path.join(state_dir(CONFIG), "pr_sessions.json")`
- `dept/review_round_watcher.py:51` — `DRY_RUN = "--dry-run" in sys.argv`
- `dept/review_round_watcher.py:53` — `MISS_LIMIT = 12`
- `dept/review_round_watcher.py:55` — `SSH_BASE = ssh_base(CONNECTION)`
- `dept/review_round_watcher.py:58` — `def mac(cmd):`
- `dept/review_round_watcher.py:66` — `def load_round(path):`
- `dept/review_round_watcher.py:71` — `def save_round(path, rnd):`
- `dept/review_round_watcher.py:75` — `def session_key(repo, pr):`
- `dept/review_round_watcher.py:80` — `def session_state(sessions, repo, pr):`
- `dept/review_round_watcher.py:85` — `def set_active_task(repo, pr, task_id):`
- `dept/review_round_watcher.py:102` — `def worker_dispatch_lock():`
- `dept/review_round_watcher.py:114` — `STATUS_RE = re.compile(r":\s*(RUNNING|DONE)(?:\s+\(exit\s+([^\)]+)\))?\s*$")`
- `dept/review_round_watcher.py:117` — `def task_status(text):`
- `dept/review_round_watcher.py:136` — `def project_dir_busy(project_dir):`
- `dept/review_round_watcher.py:164` — `def reviewer_states(task_ids):`
- `dept/review_round_watcher.py:203` — `def fetch_findings(task_ids):`
- `dept/review_round_watcher.py:214` — `VERDICT_RE = re.compile(r"^VERDICT:\s*(APPROVE|CHANGES REQUESTED)\s*$", re.MULTILINE)`
- `dept/review_round_watcher.py:215` — `HEAD_RE = re.compile(r"^HEAD:\s*([0-9a-f]{40})\s*$", re.IGNORECASE | re.MULTILINE)`
- `dept/review_round_watcher.py:218` — `def validated_verdict(text, head):`
- `dept/review_round_watcher.py:229` — `def verdict_body(lens, head, verdict, task_id, round_number, round_lenses):`
- `dept/review_round_watcher.py:240` — `def verdict_already_posted(repo, pr, body):`
- `dept/review_round_watcher.py:247` — `def post_verdict(repo, pr, lens, head, verdict, task_id, round_number,`
- `dept/review_round_watcher.py:257` — `def process_round(path):`
- `dept/review_round_watcher.py:261` — `def _process_round_locked(path):`
- `dept/review_round_watcher.py:343` — `def main():`
- `dept/test_approval_gate.py:13` — `DEPT_DIR = pathlib.Path(__file__).parent`
- `dept/test_approval_gate.py:18` — `class ApprovalGateChecksTest(unittest.TestCase):`
- `dept/status.py:23` — `DEFAULT_STATE_FILE = Path("~/.codex/zigzag/events.json").expanduser()`
- `dept/status.py:24` — `DEFAULT_TOKEN_FILE = Path("~/.codex/zigzag/zigzag.token").expanduser()`
- `dept/status.py:28` — `RELAY_OUTPUT_TAIL_BYTES = 32 * 1024 * 1024`
- `dept/status.py:34` — `INTERNAL_TASK_IDS = frozenset({"relay-update"})`
- `dept/status.py:39` — `MAX_STREAM_EVENTS = 10_000`
- `dept/status.py:42` — `def dept_task_id(task_id: str) -> str:`
- `dept/status.py:52` — `def parse_time(value: object) -> dt.datetime | None:`
- `dept/status.py:62` — `def parse_started_at(value: object) -> dt.datetime | None:`
- `dept/status.py:74` — `def event_time(event: dict[str, Any]) -> dt.datetime | None:`
- `dept/status.py:78` — `def duration_between(left: dict[str, Any], right: dict[str, Any]) -> dt.timedelta | None:`
- `dept/status.py:88` — `def is_cross_clock(left: dict[str, Any], right: dict[str, Any]) -> bool:`
- `dept/status.py:93` — `def observed_duration(events: list[dict[str, Any]]) -> tuple[dt.timedelta | None, bool]:`
- `dept/status.py:108` — `def format_duration(duration: dt.timedelta | None) -> str:`
- `dept/status.py:121` — `def sort_events(events: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:`
- `dept/status.py:135` — `def execution_key(event: dict[str, Any]) -> str | None:`
- `dept/status.py:141` — `class Execution:`
- `dept/status.py:198` — `def build_executions(`
- `dept/status.py:257` — `def audit_directory(state_file: Path) -> Path:`
- `dept/status.py:261` — `def valid_sequence(event: dict[str, Any]) -> bool:`
- `dept/status.py:269` — `def read_audit_events(state_file: Path) -> tuple[list[dict[str, Any]], list[str]]:`
- `dept/status.py:302` — `def get_json(url: str, token: str) -> dict[str, Any]:`
- `dept/status.py:311` — `class EventStream:`
- `dept/status.py:444` — `def mac_dept_root() -> Path:`
- `dept/status.py:453` — `def transcript_path(task_id: str, root: Path | None = None) -> Path:`
- `dept/status.py:458` — `def execution_transcript_path(execution: Execution, root: Path | None = None) -> Path:`
- `dept/status.py:465` — `def task_workdir(task_id: str, root: Path | None = None) -> str:`
- `dept/status.py:474` — `def transcript_command(task_id: str, root: Path | None = None) -> str:`
- `dept/status.py:479` — `def relay_output(`
- `dept/status.py:509` — `def merged_events(audit: Iterable[dict[str, Any]], recent: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:`
- `dept/status.py:521` — `def flags(execution: Execution) -> str:`
- `dept/status.py:531` — `def detail_lines(execution: Execution, limit: int = 12) -> list[str]:`
- `dept/status.py:547` — `def transcript_lines(`
- `dept/status.py:567` — `class StatusScreen:`
- `dept/status.py:750` — `def print_once(executions: list[Execution], warnings: list[str], root = None) -> None:`
- `dept/status.py:762` — `def main(argv: list[str] | None = None) -> int:`
