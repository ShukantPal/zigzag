# Architecture

## System overview

```text
Authorized clients                             Mac GUI login session
------------------                             ---------------------
zzapi / HTTP clients -- bearer token --------> zigzag relay (LaunchAgent)
                                                   | event/audit store
                                                   | agent registry + log spools
                                                   +--> agent processes/worktrees

zzapi events stream <--- authenticated socket -----+
zzapi status       <--- API state/events -----------+
```

The relay is a per-user Mac LaunchAgent named `com.shukantpal.zigzag`; it is
not a system daemon. It binds loopback and the Mac Tailscale address. A
Tailscale ACL and bearer token form its network/authentication boundary.

## Component map

| Location | Role |
| --- | --- |
| `zzd/src/main.rs` | `zigzag` daemon, config/control subcommands, and HTTP server. |
| `zzd/src/{server,http,auth,exec,proc,events}.rs` | Dispatch, authentication, restricted execution, supervision, durable events/audit. |
| `zzd/src/routes/` | Route handlers for agents, events, exec/spawn, procs, providers, and worktrees. |
| `zzd/src/{github,review_loop,provider,session,update}.rs` | GitHub watch, Rust review loop, providers, GUI/Tailscale checks, and signed update. |
| `zz/` | Shared durable JSON store, registry, parser, and secret-file support. |
| `zzapi/src/main.rs` | `zzapi`, the typed Rust relay client. |
| `zzapi/src/status.rs` | Read-only terminal status UI. |
| `launchd/` | LaunchAgent template and installation instructions. |
| `scripts/` | Signing, verification, hooks, reviewer launch, and full-stack E2E. |

## Work creation paths

`POST /v1/agents` (usually `zzapi agents create`) creates or uses a permitted
worktree, starts a supported provider command, persists an agent record,
captures logs, and creates a transcript for API-created Codex agents. The
compatibility `/v1/spawn` route can launch an allowlisted background command.
An `execution_id` identifies one attempt; retried tasks do not necessarily
share an execution ID.

## Status and completion

The relay stores bounded live events plus a separate per-execution audit
archive. `zzapi status` presents relay state and event history. An event is not
proof of completion: inspect the agent or process terminal state and final
output. Use `zzapi events` to read the retained feed or `zzapi events stream`
for live delivery; event commands do not launch tasks.
