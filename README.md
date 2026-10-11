# Zigzag

Zigzag is a Mac-hosted relay for dispatching and supervising Codex agents through an authenticated API. A Rust daemon runs in the Mac user's login session, starts agents in isolated Git worktrees, records their state and output, and exposes controlled execution and event-delivery endpoints to local tools or remote automation hosts. Its companion Rust CLI, `zzapi`, makes the API convenient to use; a read-only status TUI shows running and completed work. The relay can keep supervised tasks alive across daemon restarts, stream events and agent output over an authenticated bidirectional socket, and verify its own updates.

## Architecture

- **Relay daemon (`zigzag`, Rust):** Mac-side control plane. It supervises agent process groups, persists agent state and logs, manages permitted worktrees, serves the HTTP API, streams events, and checks signed updates. It runs as a per-user LaunchAgent so macOS Keychain access is available.
- **CLI (`zzapi`, Rust):** Authenticated client for agent, worktree, execution, and event operations. It prints readable output by default and supports `--json` for scripts.
- **Status TUI (`zzapi status`):** Read-only terminal dashboard for agent/task state, output, and event history. It uses the relay API and local task metadata.

The Rust workspace is organized by crate: [`zz/`](zz/) contains shared state,
registry, and parsing code; [`zzd/`](zzd/) contains the `zigzag` daemon, HTTP
routes, process supervision, event socket, and relay services; [`zzapi/`](zzapi/)
contains the `zzapi` client and status TUI. Event consumers can use the retained
HTTP event feed or `zzapi events stream` for live delivery over the authenticated
socket.

The daemon listens on loopback and the Mac's Tailscale address (HTTP on port `8765`; the bidirectional TCP event socket defaults to `8766`). Every API request requires the relay bearer token. Keep network access limited to trusted tailnet clients with a Tailscale ACL. See [architecture](docs/architecture.md) and [operations](docs/operations.md) for deployment details.

The relay can poll GitHub PR conversation comments, inline review comments, and review bodies, then resume the Codex session recorded for that PR. Routing runs live by default, keys durable comment events by GitHub's GraphQL `node_id`, skips replies whose first line begins `> 🤖`, and dispatches Codex resumes through the relay supervisor and shared session-operation gate. The `--comment-router-live` flag remains accepted for compatibility and has no effect. See the [native PR comment routing installation guide](launchd/INSTALL.md#native-pr-comment-routing) for the Mac-local reloadable session file and LaunchAgent setup.

## Quickstart

Build the two Rust binaries and create a private relay token:

```sh
cargo build --release -p zigzag -p zzapi
mkdir -p ~/.codex/zigzag
umask 077
openssl rand -hex 32 > ~/.codex/zigzag/zigzag.token
chmod 600 ~/.codex/zigzag/zigzag.token
```

Start the relay in a terminal for a first run (for persistent use, install the [LaunchAgent](launchd/INSTALL.md)):

```sh
target/release/zigzag \
  --secret-file ~/.codex/zigzag/zigzag.token \
  --state-file ~/.codex/zigzag/events.json
```

In another terminal, point `zzapi` at the local relay and dispatch an agent. Codex CLI must be installed and signed in for the Mac user. The agent gets its own branch/worktree under `~/.zigzag/worktrees` by default. Set `ZIGZAG_WORKTREE_BASE` to choose a different default base; `ZIGZAG_WORKTREE_ROOTS` configures allowed roots and, when set alone, its first root is also the default base.

```sh
export ZIGZAG_HOSTNAME=127.0.0.1:8765
export ZIGZAG_TOKEN_FILE=~/.codex/zigzag/zigzag.token
alias zzapi="$PWD/target/release/zzapi"

zzapi health
zzapi agents create --prompt "Inspect this project and summarize its architecture" \
  --project-dir "$PWD" --branch codex/architecture-summary
zzapi agents list
zzapi agents get AGENT_ID
```

Run `zzapi status` for the live status TUI (`zzapi status --once` prints one snapshot). Use `zzapi agents logs AGENT_ID --follow` to follow an agent's output.

## API overview

All routes below require `Authorization: Bearer <token>`.

| Operation | Endpoint / CLI |
| --- | --- |
| Create a supervised Codex agent | `POST /v1/agents` · `zzapi agents create --prompt TEXT --project-dir DIR --branch BRANCH` |
| List agents (filter by state or task) | `GET /v1/agents` · `zzapi agents list [--state running] [--task-id ID]` |
| Get agent details | `GET /v1/agents/{id}` · `zzapi agents get ID` |
| Stop an agent | `DELETE /v1/agents/{id}` · `zzapi agents stop ID` |
| Pause / resume an agent | `POST /v1/agents/{id}/pause` and `/resume` · `zzapi agents pause|resume ID` |
| Read captured output | `GET /v1/agents/{id}/logs` · `zzapi agents logs ID [--follow]` |
| Run a synchronous allowlisted command | `POST /v1/exec` · `zzapi exec --bin NAME --args ...` |
| Post events or read the retained event stream | `POST /v1/events`, `GET /v1/events?after=...&epoch=...&timeout=...` · `zzapi events [--follow]` |
| Stream events and agent output interactively | Authenticated bidirectional TCP socket · `zzapi events stream` |

Agent creation accepts an inline prompt or prompt-file path, project directory, branch, and optional worktree, model, approval mode, and timeout. The relay records lifecycle state and bounded stdout/stderr logs; stopping an agent leaves its worktree in place. Events are retained in a bounded durable queue, so consumers should use cursors and deduplicate by event ID. See [agents and events](docs/agents.md), [zzapi](docs/zzapi.md), and [relay API details](docs/relay.md).

## Configuration and security

The relay token lives at `~/.codex/zigzag/zigzag.token`; keep it owner-readable only. `zzapi` reads it from `ZIGZAG_TOKEN_FILE` (or `--token-file`) and uses `ZIGZAG_HOSTNAME` to select the relay. For a remote client, use the Mac's Tailscale address and restrict port `8765` (and socket port `8766`, if used) with a Tailscale ACL.

`/v1/exec` can run only binaries and argument prefixes in the relay's allowlist. The macOS login Keychain is preferred. For headless or Keychain-unavailable deployments, set `ZIGZAG_EXEC_ALLOWLIST_FILE` in the daemon environment to a private JSON policy file; if neither source is available, the relay uses an empty deny-all policy and logs a warning. See [configuration and security](docs/configuration.md) for file ownership, permissions, and LaunchAgent setup. The Keychain policy can be read or changed from the Mac's GUI login session:

```sh
zigzag config get-allowlist
zigzag config set-allowlist --file /path/to/policy.json
```

The JSON policy maps binary names to absolute paths and permitted argv prefixes. `set-allowlist` replaces the complete policy. The first Keychain access may prompt for permission; review the binary before granting it. See [configuration and security](docs/configuration.md) and the [Mac installation guide](launchd/INSTALL.md).

## Development

The workspace includes the shared `zz` library, `zzd` daemon crate (built as
the `zigzag` binary), and `zzapi` CLI crate (built as the `zzapi` binary). From
the repository root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Install the repository's pre-commit and pre-push hooks with `./scripts/install-hooks.sh`. For a local full-stack relay/CLI check, use `bash scripts/e2e-full-stack.sh target/debug/zigzag target/debug/zzapi`. Browse [docs/](docs/README.md) for component guides and deployment runbooks.
