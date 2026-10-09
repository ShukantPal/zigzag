# Zigzag: architecture and operations guide

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

poller <--------- GET /v1/events long poll ---+
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
| `relay/src/main.rs` | `zigzag` daemon: config/control subcommands or HTTP relay server. |
| `relay/src/{server,http,auth,exec,proc,events}.rs` | HTTP dispatch/authentication, restricted execution, process supervision, durable event/audit lifecycle. |
| `relay/src/routes/` | Implemented API route handlers for agents, events, exec/spawn, procs, providers, and worktrees. |
| `relay/src/{github,review_loop,provider,session,update}.rs` | Legacy GitHub watch, Mac review loop, Codex/OpenCode providers, GUI/Tailscale checks, signed self-update. |
| `relay-core/` | Shared durable JSON store, agent registry, JSON parser, and secret-file support. |
| `cli/src/main.rs` | `zzapi`, a typed relay REST client. |
| `poller/src/main.rs` | VM-side event long-poll client that emits JSON Lines and persists its cursor. |
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

The relay agent path is intentionally narrower than `/v1/exec`: callers select a provider-level request, not arbitrary binary/arguments. `POST /v1/agents` accepts a prompt (inline or file path), `project_dir`, `branch`, and optional `worktree`, `model`, `approval_mode`, and `timeout_secs`. It creates a supervised Codex process in the worktree (default under `/private/tmp/<branch-slug>/`), stores durable lifecycle state, and returns an agent ID and resolved worktree. Capacity is bounded (default 16 agents).

The registry reaper records terminal exit. A relay restart cannot reattach old pipes: a still-alive group becomes `orphaned`; a dead former group becomes `lost_after_restart`. Terminal registry entries and spool metadata are pruned after seven days. The generic spawn compatibility path does not create the same full transcript file; API-created agents do.

`POST /v1/worktrees` and `DELETE /v1/worktrees` are explicit worktree helpers. They canonicalize paths, restrict them to approved roots (`/private/tmp` or `/Users/shukant/.codex/worktrees`), reject traversal/unusable paths, and refuse to remove a worktree used by a live agent. Stopping an agent leaves its worktree; cleanup is a separate, deliberate action.

### Event and completion model

The live store in `events.json` is durable but bounded. Posting the same `id` is idempotent only while that event is retained; consumers must deduplicate. `GET /v1/events` returns `epoch`, `reset`, `lost`, `events`, and `next`. Consumers pass `next` as `after` and echo `epoch`; `reset` means the store was replaced and `lost` means the requested sequence was evicted.

Schema-v1 lifecycle facts use `id`, `task_id`, `execution_id`, `kind`, `source`, `occurred_at`, `clock`, and object `payload`. Accepted sources are `vm-department`, `mac-relay`, and `vm-poller`. Facts with an execution ID are also appended to per-execution JSONL audit logs in `events.audit/`; that archive is independent from live retention, capped at 20 MiB total, and removes oldest execution logs first. It excludes prompts, command arguments, and raw output.

Relay events such as spawn/exit plus department transition events let `status.py` project execution phase. A task is not completed merely because its event appeared: use the relay agent/proc exit state or `dept.py status` for liveness, then inspect final output. The optional completion announcement in `dept/relay-announce.md` posts `<task-id>-done` best-effort after the agent has actually finished; it is a notification, not an authoritative exit record.

## HTTP endpoint reference

All entries below are authenticated with the relay bearer token unless noted. Use `openapi.yaml` for wire schemas, but trust `relay/src/routes/` for routes introduced after the specification: the current implementation includes `/v1/providers` and `/v1/agents/{id}/transcript`, which the OpenAPI file does not yet enumerate.

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
zzapi agents create --prompt TEXT --project-dir DIR --branch BRANCH
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

### `poller`

`poller` is a VM/orchestrator consumer, not a task launcher. It continuously long-polls `/v1/events`, prints each delivered event as JSON Lines with `zigzag_epoch`, and atomically saves `{epoch,next}` in its cursor state file. It retries after errors; `--once` is useful for a smoke check.

```text
poller --zigzag-url http://HOST:8765 --secret-file PATH --state-file PATH
       [--proxy URL] [--timeout 1..55] [--once]
```

`ZIGZAG_SECRET_FILE` and `ZIGZAG_PROXY` provide defaults. Treat its `lost` or `reset` warning as a signal to rebuild a downstream projection from durable sources rather than pretending the missing event range was observed.

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
| Agent shows `orphaned` after restart | Relay lost pipe attachment, not necessarily the process. Inspect process group/output, then stop or recover deliberately. |
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

