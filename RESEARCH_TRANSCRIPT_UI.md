# Research: native UI for Zigzag transcripts

**Scope:** read-only remote viewing of Codex agent transcripts on desktop and, if practical, mobile. This report describes the repository as inspected on 2026-10-09. It does not change the relay.

## Findings

### Where transcript data lives

Relay-native agents created through `POST /v1/agents` write Codex's JSONL stdout to:

```text
~/.zigzag/agents/codex/<32-hex-agent-id>.jsonl
~/.zigzag/agents/codex/<32-hex-agent-id>.stderr
```

The directory is created with mode `0700`; output files use `0600`. The JSONL is the raw Codex `--json` event stream (each line is a JSON event), and stderr is a separate text file. This is distinct from the durable registry at the configured state-file path, its adjacent `<state>.agents.json` registry, and its bounded `<state>.agent-logs/` diagnostic spool. The spool is capped at 32 MiB per relay-core constant and reports eviction/degradation metadata. See [`relay/src/proc.rs`](relay/src/proc.rs), [`relay-core/src/lib.rs`](relay-core/src/lib.rs), and [`docs/agents.md`](docs/agents.md).

Other task artifacts may live under `~/.codex/dept/t-*/` (prompt, markers, final message and task events). Codex session rollout files also exist under `~/.codex/sessions/**/rollout-*.jsonl`, but they are described as session metadata for resume CWD recovery, not as the relay agent transcript API's source. `/v1/spawn` compatibility processes do not necessarily have the API-created Codex JSONL transcript.

### API available today

All HTTP routes require `Authorization: Bearer <relay-token>`.

| Endpoint | Current use | Viewer relevance / limits |
| --- | --- | --- |
| `GET /v1/agents` | Agent list, optional `state` and `task_id` filters | Provides list/navigation metadata; no pagination. |
| `GET /v1/agents/{id}` | Status record | Metadata, not transcript content. |
| `GET /v1/agents/{id}/logs?stream=stdout|stderr|both&after=N&tail=N` | Read output records | Byte-offset cursor, optional tail. Response includes records, `next_cursor`, dropped/degraded information. |
| `GET /v1/agents/{id}/logs?...&follow=1` | Long-poll logs | Waits for records up to 50 seconds; clients can reconnect with the returned byte cursor. |
| `GET /v1/agents/{id}/transcript?after=N&tail=N` | Aggregate metadata, optional prompt/last message, stdout and stderr strings | Not listed in `openapi.yaml`; query has no `follow` parameter and returns text fields rather than structured Codex JSONL events. |
| Authenticated TCP socket (`8766`) | `agents`, `events`, and `logs.<id>` subscriptions | Pushes agent snapshots/events and log records; not a transcript topic, and no replay guarantee beyond log cursors/resynchronizing over HTTP. |
| `GET /v1/events?after=&epoch=&timeout=` | Long-poll retained lifecycle events | Useful for status/timeline updates, not transcript contents. Bounded retention with reset/lost signals. |

The `zzapi` CLI has agents list/get/logs and transcript verbs, but its docs say providers and transcripts may require authenticated HTTP until CLI verbs are fully exposed; actual CLI support has moved ahead of that note. The OpenAPI spec documents agent logs but not `/transcript` or the TCP socket protocol. See [`relay/src/routes/agents.rs`](relay/src/routes/agents.rs), [`relay/src/socket.rs`](relay/src/socket.rs), [`cli/src/main.rs`](cli/src/main.rs), and [`openapi.yaml`](openapi.yaml).

**Retention detail to resolve before promising complete remote history:** the raw stdout JSONL file is intentionally kept separately, but `agent_logs_json()` reads the bounded registry spool when `stdout_next > 0`. The transcript route is built on that function. Therefore, once output has entered the spool, the HTTP transcript/log views may only expose the retained 32 MiB window even while the raw JSONL file remains on disk. The UI must display `dropped_before`/`log_degraded` and make truncation visible. A follow-up relay change should serve the durable JSONL by byte range/cursor (or make that durable file the source of truth for this API), while preserving redaction expectations.

### Remote connectivity and auth

The daemon binds HTTP port `8765` and its event socket port `8766` on loopback and the Mac's Tailscale IPv4 address. The HTTP listener logs `http://` and does not itself terminate TLS. The documented deployment relies on the encrypted Tailscale network plus a Tailscale ACL/grant restricting these ports, with the bearer token as application authentication. A desktop or mobile device with Tailscale connected can address the relay's tailnet IP or MagicDNS name directly. That is the simplest path; no public port forwarding is needed.

For HTTPS browser/web clients or a future webview app, Tailscale Serve can proxy a local relay port to a tailnet-only HTTPS name and apply tailnet access policy. Tailscale's documentation says Serve is tailnet-only, requires HTTPS certificates to be enabled, and respects tailnet access rules ([Serve docs](https://tailscale.com/docs/features/tailscale-serve)). Avoid Funnel/public exposure for this private viewer. The relay's current static bearer token authorizes the broader API, including control routes; do not bundle it into a distributed mobile binary. A viewer should get a dedicated read-only credential or an authenticated gateway that maps Tailscale identity to read-only permissions. Store device credentials in Keychain/Keystore, support revocation/rotation, and do not log them.

## Desktop framework recommendation

**Recommendation: Tauri 2 for a product intended to span desktop and mobile; use a Rust HTTP/client crate for protocol and event handling, and a web UI for transcript rendering.** It is a Rust-backed shell with native webview rendering and current official docs list macOS, Windows, Linux, iOS, and Android as targets ([Tauri](https://tauri.app/)). This fits long, structured, searchable transcripts well: virtualized event rows, syntax/code blocks, copy/search, responsive layouts, and accessibility are easier to build with web UI components than custom immediate-mode widgets. The network/parser/state logic can remain Rust and be shared with mobile.

If the requirement is **pure Rust widgets**, choose **egui/eframe** for the first desktop viewer: low setup friction, portable, and easy to build a log/event inspector quickly ([egui docs](https://docs.rs/egui/latest/egui/)). Its immediate-mode approach suits a technical dashboard, though polished text-heavy document navigation, accessibility, and mobile interaction take more bespoke work. **iced** is a reasonable alternative for a more structured native Rust UI, but its current crate docs still describe it as experimental and its type-heavy Elm-style model adds learning/iteration cost ([iced docs](https://docs.rs/iced/latest/iced/)).

The choice hinges on what “native Rust UI” means. If it means Rust owns app behavior and native desktop packaging, Tauri is the pragmatic cross-device choice. If it means the UI itself must be Rust widgets, use egui for desktop and treat mobile as a separate project. Tauri uses a web frontend; it is not a pure Rust-widget framework.

## Mobile feasibility

Mobile is feasible, but it adds a meaningful second UX/build/release surface:

- **Tauri 2:** reuses Rust commands/core and much of the frontend across iOS and Android. Native build toolchains, signing, platform permissions, deep links, secure credential storage, and touch-first layout still need platform-specific setup and QA. This is the best single-codebase option if mobile is an actual goal.
- **Native Swift/Kotlin shell + Rust core:** expose the Rust HTTP client/parser through UniFFI or a small C ABI. This gives the most conventional native mobile UI and OS integration, while duplicating the view layer. Better if mobile polish matters more than frontend reuse.
- **egui/iced on mobile:** technically possible only through additional platform integration; not recommended as the quickest route to a polished phone reader. Small screens and accessibility are the hard parts, not parsing JSONL.
- **Mobile web/PWA:** fastest read-only pilot if served privately over Tailscale Serve, but it is a browser experience, has browser background/network constraints, and still needs secure login/token handling.

On iOS/Android, the simplest remote route is the device's Tailscale app connected to the same tailnet, then the viewer calls the tailnet relay endpoint. Requiring Tailscale installation is an explicit product constraint. Supporting users without tailnet membership would require a public-facing gateway and a separate user authentication design; do not expose the current broad relay token directly to the internet.

## API work recommended for a viewer

The existing surface is enough to prototype a desktop list + output pane, but a durable and robust multi-device viewer needs the following:

1. **Document and normalize transcript API.** Add `/v1/agents/{id}/transcript` to OpenAPI. Define whether it returns raw JSONL lines, parsed Codex events, or both; include the source encoding, byte cursor semantics, `next_cursor`, `has_more`, dropped/truncated status, and process completeness. Define prompt/last-message availability separately since the relay intentionally does not retain prompts itself.
2. **Serve full history with pagination.** Make cursors usable against the durable transcript file, support bounded page sizes and backward/tail reads for initial display, and expose a stable snapshot/ETag or transcript generation ID. Byte cursor offsets can split UTF-8 unless the server aligns them to line boundaries; line/event sequence cursors are friendlier for UI pagination. Keep log truncation explicit.
3. **Provide transcript streaming.** Add authenticated SSE (simple cross-platform HTTP) or WebSocket for append events, with replay from a cursor and reconnect semantics. Today `/logs?follow=1` is 50-second long polling and the socket's `logs.<id>` pushes records, but native clients must implement custom length-prefixed TCP framing; neither is a documented standard event stream. A pragmatic first version can long-poll existing logs while a standard stream is designed.
4. **Read-only auth boundary.** Issue scoped viewer credentials limited to list/status/transcript/events. Current bearer tokens authorize mutating and execution endpoints too, which is excessive for a viewer and unsafe to embed. Keep access network-restricted by Tailscale grants/ACLs; add TLS via Serve if an HTTP webview/browser path needs it.
5. **Round out viewer metadata.** Add server-side list pagination/sorting and a stable agent/task timeline query if histories grow. Current `GET /v1/agents` returns the matching agent set without pagination; events already use a cursor and expose retention loss.

Suggested MVP: Tauri desktop client on Tailscale, read-only list/status and transcript display, initial tail plus cursor-based follow via `/logs?follow=1`, visible truncation/degraded markers, then relay changes for durable line-oriented pages and scoped read-only auth. Add SSE after the read API is stable. If shipping phone access soon matters, choose Tauri now so it can host the same UI; otherwise validate the desktop viewer first and defer mobile packaging.

## Repository and external references

- [`README.md`](README.md), [`docs/architecture.md`](docs/architecture.md), [`docs/agents.md`](docs/agents.md), [`docs/relay.md`](docs/relay.md)
- [`relay/src/proc.rs`](relay/src/proc.rs), [`relay/src/routes/agents.rs`](relay/src/routes/agents.rs), [`relay/src/socket.rs`](relay/src/socket.rs), [`relay-core/src/lib.rs`](relay-core/src/lib.rs)
- [Tauri](https://tauri.app/), [egui crate docs](https://docs.rs/egui/latest/egui/), [iced crate docs](https://docs.rs/iced/latest/iced/), [Tailscale Serve](https://tailscale.com/docs/features/tailscale-serve)
