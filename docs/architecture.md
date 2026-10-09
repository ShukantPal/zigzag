# Architecture

## System overview

```text
VM / automation host                         Mac GUI login session
---------------------                        ---------------------
dept.py / watchers -- SSH -----------------> ~/.codex/dept/t-*/codex-launch.sh
       |                                         |
       | HTTP (Tailscale, bearer token)           v
       +---------------------------------> zigzag relay (LaunchAgent)
                                             | event/audit store
                                             | agent registry + log spools
                                             +--> Codex process groups/worktrees

poller <--------- GET /v1/events long poll ---+
status.py <----- events + agents + audit ------+
zzapi ---------- supported relay routes -------+
```

The relay is a per-user Mac LaunchAgent named `com.shukantpal.zigzag`; it is
not a system daemon. It binds loopback and the Mac Tailscale address. A
Tailscale ACL and bearer token form its network/authentication boundary.

## Component map

| Location | Role |
| --- | --- |
| `relay/src/main.rs` | `zigzag` daemon, config/control subcommands, and HTTP server. |
| `relay/src/{server,http,auth,exec,proc,events}.rs` | Dispatch, authentication, restricted execution, supervision, durable events/audit. |
| `relay/src/routes/` | Route handlers for agents, events, exec/spawn, procs, providers, and worktrees. |
| `relay/src/{github,review_loop,provider,session,update}.rs` | GitHub watch, Rust review loop, providers, GUI/Tailscale checks, and signed update. |
| `relay-core/` | Shared durable JSON store, registry, parser, and secret-file support. |
| `cli/src/main.rs` | `zzapi`, the typed Rust relay client. |
| `poller/src/main.rs` | VM event long-poll consumer with a durable cursor. |
| `dept/` | Python dispatcher, status UI, config, lifecycle helpers, and scheduled watchers. |
| `launchd/` | LaunchAgent template and installation instructions. |
| `scripts/` | Signing, verification, hooks, reviewer launch, and full-stack E2E. |

## Work creation paths

There are two distinct paths.

1. Relay-native: `POST /v1/agents` (usually `zzapi agents create`) creates or
   uses a permitted worktree, starts a fixed provider command, persists an
   agent record, captures logs, and creates a transcript for API-created
   agents.
2. Department manager: `dept.py start` or `resume` prepares
   `~/.codex/dept/<task-id>/` on the Mac and normally calls `/v1/spawn` to run
   allowlisted `codex-launch`. With `--ssh`, it instead starts `codex exec`
   over SSH with `nohup` and tracks that attempt itself.

IDs differ: `t-xxxxxx` is a logical department task, a relay `proc` is also
the compatibility-era agent ID, and an `execution_id` identifies one attempt.
Retried tasks do not necessarily have the same execution.

## Status and completion

The relay stores bounded live events plus a separate per-execution audit
archive. `status.py` combines those with agent snapshots to project task
phase. An event is not proof of completion: inspect an agent/proc terminal
state or `dept.py status`, then inspect the final output. `poller` only
consumes events; it never launches tasks.
