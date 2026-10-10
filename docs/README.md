# Zigzag documentation

Zigzag is a Mac-local Rust relay, a set of VM/automation-host Python tools, and
clients that coordinate supervised Codex work. The relay is authoritative for
relay state; much of `dept/` reaches it over Tailscale/SSH.

Start here, then use the focused references below.

| Guide | What it covers |
| --- | --- |
| [Architecture](architecture.md) | Component boundaries, launch paths, and data flow. |
| [Relay](relay.md) | Rust daemon, its HTTP API, modules, and worktrees. |
| [Department tooling](dept.md) | `dept/` scripts, manager commands, status UI, and watchers. |
| [zzapi CLI](zzapi.md) | Relay client configuration and command reference. |
| [Agents and events](agents.md) | Agent lifecycle, state, logs, transcripts, and event delivery. |
| [Review system](review-system.md) | Review rounds, lenses, advisory results, and the approval gate. |
| [Operations](operations.md) | Running, diagnosis, recovery, validation, and common failures. |
| [Configuration and security](configuration.md) | Relay options, allowlist, Keychain, tokens, network boundary, and paths. |

The implementation is the source of truth. In particular, route behavior lives
in `zzd/src/routes/`; `openapi.yaml` is a useful but incomplete public
contract. See `launchd/INSTALL.md` for deployment/cutover details.
