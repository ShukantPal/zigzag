# Zigzag documentation

Zigzag is a Mac-local Rust relay and its authenticated client. The relay
supervises agent work, persists state and events, and exposes the API used by
`zzapi` and other authorized clients.

Start here, then use the focused references below.

| Guide | What it covers |
| --- | --- |
| [Architecture](architecture.md) | Component boundaries, launch paths, and data flow. |
| [Relay](relay.md) | Rust daemon, its HTTP API, modules, and worktrees. |
| [zzapi CLI](zzapi.md) | Relay client configuration and command reference. |
| [Agents and events](agents.md) | Agent lifecycle, state, logs, transcripts, and event delivery. |
| [Review system](review-system.md) | Review rounds, lenses, advisory results, and the approval gate. |
| [Operations](operations.md) | Running, diagnosis, recovery, validation, and common failures. |
| [Configuration and security](configuration.md) | Relay options, allowlist, Keychain, tokens, network boundary, and paths. |

The workspace contains `zz/` (shared library), `zzd/` (the `zigzag` daemon),
and `zzapi/` (the CLI and status TUI). The implementation is the source of
truth. Route behavior lives in `zzd/src/routes/`; `openapi.yaml` is a useful
but incomplete public contract. See `launchd/INSTALL.md` for deployment
details.
