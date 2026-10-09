# Agents, events, and state

## Agent lifecycle

Relay-native agent creation is deliberately narrower than `/v1/exec`: callers
submit a provider request instead of arbitrary commands. The relay starts a
supervised Codex group in a permitted worktree, persists lifecycle state,
captures stdout/stderr, and records an API-created transcript.

The registry reaper records terminal exit. After relay restart it cannot
reattach pipes: a live old group becomes `orphaned`; a dead one becomes
`lost_after_restart`. Terminal entries and spool metadata are pruned after
seven days. Generic `/v1/spawn` compatibility processes do not necessarily
have the API-created transcript.

Agent controls are listing/get, graceful stop (SIGTERM then SIGKILL), pause
(SIGSTOP), and resume (SIGCONT). Paused state is reapplied where possible
after restart. Log reads return a cursor; if `dropped_before` is beyond the
requested cursor, the bounded spool has irretrievably lost earlier output.

## Event delivery

`events.json` is durable but bounded. A duplicate event ID is idempotent only
while retained, so consumers must deduplicate. Long-poll reads return `epoch`,
`reset`, `lost`, `events`, and `next`: pass `next` as `after`, preserve epoch,
rebuild a projection on `reset`, and treat `lost` as eviction.

Schema-v1 facts contain `id`, `task_id`, `execution_id`, `kind`, `source`,
`occurred_at`, `clock`, and object `payload`. Sources are `vm-department`,
`mac-relay`, and `vm-poller`. Facts with an execution ID are appended to
per-execution audit JSONL; that archive is separate from live retention, capped
at 20 MiB, drops oldest executions first, and excludes prompts, arguments, and
raw output.

The optional `dept/relay-announce.md` completion event is best-effort only. A
completion event is never the authority for whether a process exited.

## State and artifact locations

| Path | Contents |
| --- | --- |
| `~/.codex/zigzag/zigzag.token` | Bearer token, mode 0600; never log it. |
| `~/.codex/zigzag/events.json` | Bounded durable live-event store. |
| `~/.codex/zigzag/events.audit/` | Per-execution append-only audit JSONL. |
| `~/.codex/zigzag/events.agents.json` | Durable relay agent registry. |
| `~/.codex/zigzag/events.agents.agent-logs/` | Owner-only bounded stdout/stderr spools. |
| `~/.codex/dept/t-xxxxxx/` | Task prompt/markers, PID/exit data, events, stderr, final message. |
| `~/.codex/sessions/**/rollout-*.jsonl` | Codex session metadata used by resume CWD recovery. |
| `/private/tmp/<branch-slug>/` | Default relay-native worktree; inspect before cleanup. |
| department `state_dir` | Ledger, locks, prompts, review rounds, watcher watermarks. |

For another state-file basename `<state>`, derive adjacent artifact names as
`<state>.agents.json`, `<state>.reviews.json`, and `<state>.audit/`.
