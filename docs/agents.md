# Agents, events, and state

## Agent lifecycle

Relay-native agent creation is deliberately narrower than `/v1/exec`: callers
select a supported CLI harness (`codex`, `gemini`, `opencode`, or `antigravity`) instead of
supplying arbitrary commands. The relay starts the chosen CLI in a permitted
worktree or, with explicit `--no-branch`, directly in the project directory;
it persists lifecycle state, captures stdout/stderr, and records an
API-created transcript. The harness defaults to `codex` for compatibility.

The shared approval modes map to each harness: Gemini uses `default`,
`auto_edit`, or `yolo`; OpenCode stores the permission rules on each session;
Codex uses its app-server defaults. Antigravity uses `agy -p` with streamed
JSON output; its `full-auto` mode uses `--dangerously-skip-permissions`, while
other approval modes retain AGY's default permission behavior. `agy` is
accepted as an alias for the `antigravity` harness. AGY handles sign-in through
its own Google account flow and system keyring on the relay host. Model
selection is passed to the harness session when supported. With no model
override, Gemini, OpenCode, and Antigravity use their configured defaults, and
Codex uses its app-server default.

Codex agents use one long-lived app-server for the daemon's local isolation
domain and one durable thread per agent. OpenCode agents use one loopback-only
`opencode serve` process for the domain and one durable session per agent.
Each session is created with that agent's worktree (or project directory for
`--no-branch`). On daemon startup, active Codex threads and OpenCode sessions
are reattached from the durable registry. Session IDs are routing keys, not
tenant security boundaries; agents that share a server also share its process
failure domain.

`POST /v1/agents/{id}/messages` accepts `{ "text": "...", "delivery":
"steer" | "queue" }`. `zzapi agents message <id> --steer "..."` sends a
mid-turn instruction; `--queue "..."` appends a follow-up turn for either
native harness. Stop remains session-scoped. Pause/resume are unavailable for
app-server sessions because signaling their shared process would affect every
agent in the domain.

`POST /v1/agents/{id}/continue` accepts `{ "prompt": "..." }` and returns
only after the Codex follow-up turn completes. On a running agent, the default
queues the prompt behind its active turn. `{ "interrupt": true }` instead
interrupts the active turn first, then sends the prompt as the next turn; this
requires a running agent. The CLI equivalent is
`zzapi agents continue <id> --prompt TEXT --interrupt`.

On a finished, interrupted, or otherwise non-running Codex agent, the same
endpoint resumes the existing thread from its on-disk rollout, then sends the
prompt as a new turn and updates the existing agent record. It never creates a
replacement thread. If the rollout is missing, corrupt, or cannot be resumed,
the endpoint returns HTTP 409 with `agent_transcript_unavailable` and leaves the
record unchanged. Resume restores conversation history from the rollout, not
exact mid-tool-call execution state. If a turn stopped during a tool call,
resume starts at the last completed item and the agent re-derives its place from
the branch and transcript. `--interrupt` cannot be combined with resume; it
returns HTTP 409 for a non-running agent. These operations use the same
bearer-token authentication as agent creation.

The shared live-session budget defaults to four Codex threads and OpenCode
sessions across a daemon domain. Set `ZIGZAG_MAX_LIVE_AGENT_THREADS` before
starting the daemon to tune it for the machine; it must be a positive integer.
The limit is checked when a native agent is created and returns HTTP 429 when
the budget is full. Increase it after observing memory pressure and CPU
contention while representative agents run; decrease it if those resources
become constrained. Existing sessions keep running if the configured budget
is lowered.
Auto-update waits until registered agents stop before it shuts down the shared
harness servers and replaces the daemon. If a daemon starts with running
native agents after an interrupted restart, it reconnects their durable IDs.

The initial default is based on local measurements, not a vendor limit. On a
128 GiB Mac, the shared Codex app-server measured about 0.25–0.29 GiB RSS and
the OpenCode server about 0.63 GiB RSS after the steering runs; both were
below 1% CPU while idle. Active model/tool work varies, so the four-session
budget bounds concurrent agent work while leaving room to tune against the
machine's own active workload.

Gemini, Antigravity, and other one-shot harnesses retain their process
lifecycle. The registry reaper records terminal exit. After relay restart, the
daemon verifies each live agent's recorded PID birth identity and keeps a
verified process in `running`. It tails the durable transcript and stderr
files, and records `unexpected_exit` if the process disappears while the relay
is away. A dead process detected during startup becomes `lost_after_restart`.
Terminal entries and spool metadata are pruned after seven days. Generic
`/v1/spawn` compatibility processes do not necessarily have the API-created
transcript.

Agent controls are listing/get, graceful stop (SIGTERM then SIGKILL), pause
(SIGSTOP), and resume (SIGCONT). Paused state is reapplied where possible
after restart. Log reads return a cursor; if `dropped_before` is beyond the
requested cursor, the bounded spool has irretrievably lost earlier output.

## Event delivery

`events.json` keeps a bounded in-memory cache backed by append-only JSONL
segments in `events.stream/`. Segments are synced before an event is reported
accepted, and whole segments are garbage-collected 30 days after creation. A
reader whose cursor predates the cache reads the retained segments, so cache
eviction does not set `lost`. A duplicate event ID remains idempotent while its
segment is retained. Long-poll reads return `epoch`, `reset`, `lost`, `events`,
and `next`: pass `next` as `after`, preserve epoch, rebuild a projection on
`reset`, and treat `lost` as an expired or otherwise unavailable file window.

Schema-v1 facts contain `id`, `task_id`, `execution_id`, `kind`, `source`,
`occurred_at`, `clock`, and object `payload`. Facts with an execution ID are
appended to per-execution audit JSONL; that archive is separate from live
retention, capped at 20 MiB, drops oldest executions first, and excludes
prompts, arguments, and raw output. A completion event is never the authority
for whether a process exited.

## State and artifact locations

| Path | Contents |
| --- | --- |
| `~/.codex/zigzag/zigzag.token` | Bearer token, mode 0600; never log it. |
| `~/.codex/zigzag/events.json` | Bounded durable live-event store. |
| `~/.codex/zigzag/events.stream/` | Append-only event segments; whole files expire after 30 days. |
| `~/.codex/zigzag/events.audit/` | Per-execution append-only audit JSONL. |
| `~/.codex/zigzag/events.agents.json` | Durable relay agent registry. |
| `~/.codex/zigzag/events.agents.agent-logs/` | Owner-only bounded stdout/stderr spools. |
| `~/.codex/sessions/**/rollout-*.jsonl` | Codex session metadata used by resume CWD recovery. |
| `/private/tmp/<branch-slug>/` | Default relay-native worktree; inspect before cleanup. |

For another state-file basename `<state>`, derive adjacent artifact names as
`<state>.agents.json`, `<state>.reviews.json`, and `<state>.audit/`.
