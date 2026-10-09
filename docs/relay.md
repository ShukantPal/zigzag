# Rust relay

The `zigzag` binary in `relay/src/main.rs` is the authenticated Mac-local
control plane. The normal server requires `--secret-file PATH` (or
`ZIGZAG_SECRET_FILE`) and `--state-file PATH` (or `ZIGZAG_STATE_FILE`).

## Server options and subcommands

| Option | Meaning |
| --- | --- |
| `--control-secret-file PATH` | Enables legacy generic proc kill with a distinct secret. |
| `--port N` | Default: 8765. |
| `--tailscale-ip IP` | Test override; must be a `100.64.0.0/10` IPv4 address. Otherwise uses `tailscale ip -4`. |
| `--max-events N` | Positive live-event retention bound; default 1000. |
| `--watch-repo OWNER/REPO`, `--watch-interval S` | Legacy GitHub watch (30–3600 seconds), used only while Rust review loop is not authoritative. |
| `--update-dir PATH`, `--update-interval S`, `--update-policy …` | Signed-update controls; interval 0 disables scheduled checks. |
| `--update-ready-file PATH` | Internal replacement/watchdog handoff. |

`zigzag config get-allowlist`, `zigzag config set-allowlist --file PATH`,
`zigzag timeline TASK_ID --state-file PATH`, and
`zigzag updates --dir PATH status|pause|pin VERSION|unpin` are operational
subcommands. `update-watchdog` is internal.

## Endpoint reference

All routes require the relay bearer token unless specifically noted.

| Route | Purpose |
| --- | --- |
| `GET /v1/health` | Authenticated liveness. |
| `POST /v1/events`, `GET /v1/events?after=&epoch=&timeout=` | Persist or long-poll lifecycle events. |
| `POST /v1/exec` | Synchronously run `{id,bin,args}` only when allowlisted. Denials are opaque. |
| `POST /v1/spawn`, `GET /v1/proc/{id}` | Compatibility background process launch/status. Spawn accepts optional safe `execution_id`. |
| `POST /v1/proc/{id}/kill` | Legacy process-group kill; needs control secret and may be disabled. |
| `GET, POST /v1/agents` | List durable agents or create a relay-native provider agent. |
| `GET, DELETE /v1/agents/{id}` | Read lifecycle or gracefully stop/deregister; the worktree remains. |
| `POST /v1/agents/{id}/pause`, `/resume` | Stop/continue an agent process group. |
| `GET /v1/agents/{id}/logs` | Bounded redacted stdout/stderr spool; supports cursor/tail/follow. |
| `GET /v1/agents/{id}/transcript` | Transcript for API-created Codex agents, where available. |
| `GET /v1/providers` | Available providers and supported models. |
| `POST, DELETE /v1/worktrees` | Create/remove permitted worktrees. |
| `GET /v1/review-gate?repository=&pull_request=` | Review, CI, and human-approval decision. |

`openapi.yaml` documents wire formats but does not yet enumerate every
implemented route; check `relay/src/routes/` when they differ.

## Worktrees and provider agents

`POST /v1/agents` takes a prompt (inline or file), `project_dir`, `branch`,
and optional `worktree`, `model`, `approval_mode`, and `timeout_secs`. Its
default worktree is `/private/tmp/<branch-slug>/`; capacity defaults to 16.
Worktree routes canonicalize paths, permit only `/private/tmp` or
`/Users/shukant/.codex/worktrees`, reject traversal, and will not remove a
live agent's worktree. Stopping an agent deliberately does not clean it up.
