# Zigzag

_Managed by Pal's Muse._

`zigzag` runs on a Mac and retains the latest 1,000 authenticated completion
events. `poller` runs on the orchestrator VM, holds a long-poll request open,
and writes delivered events as JSON Lines. This replaces a five-minute status
poll with normal delivery latency close to one network round trip.

Delivery is at least once: downstream consumers must deduplicate using the
event `id` (or Zigzag epoch and sequence). POST idempotency applies while an
event remains in the durable bounded queue; a replay after eviction is a new
delivery. The queue is durable but bounded; when it overflows, the poller
reports a warning on stderr.

## Execution audit trail

The relay also keeps append-only JSON Lines audit logs beside its state file
(`events.audit/` for an `events.json` state file). These are per execution,
separate from the 1,000-event live-delivery queue, and capped at 20 MiB total;
the oldest execution logs are removed first when the cap is exceeded. Audit
records contain the posted event envelope plus relay `sequence` and
`received_at` fields. They never contain command arguments, prompts, or raw
agent output.

New audit events use schema version 1 and include `id`, `task_id`,
`execution_id`, `kind`, `source`, `occurred_at` (RFC 3339 UTC milliseconds),
`clock`, and an object payload. The relay accepts `vm-department`,
`mac-relay`, and `vm-poller` sources. Existing id-only event posts remain
wire-compatible for live delivery, but cannot be archived by execution because
they have no execution key.

`POST /v1/spawn` also accepts an optional safe `execution_id` field. A VM that
has already assigned an execution should supply it so its dispatch/poll facts
and the relay's launch/process facts land in the same per-execution log;
callers that omit it retain the existing request shape and receive a
relay-generated execution identifier internally.

The Mac reader is `zigzag timeline`, rather than `dept timeline`: this
repository owns the `zigzag` relay binary while `dept.py` is VM-owned and is
not present here.

```sh
zigzag timeline TASK_ID --state-file ~/.codex/zigzag/events.json
```

It renders the persisted events and named duration summaries. Durations are
shown only when both facts came from the same `clock`; a Mac/VM handoff prints
both timestamps as cross-clock rather than fabricating a transit time.

## Supervised agent diagnostics

`POST /v1/spawn` keeps its existing `{"id", "proc"}` response and
`/v1/proc/<proc>` compatibility status view. During migration, `proc` is also
the relay-generated agent ID. The relay persists a private agent registry next
to its event state and continuously drains each agent's stdout and stderr.
Authenticated read-only diagnostics are available at:

- `GET /v1/agents?state=…&task_id=…`
- `GET /v1/agents/<agent-id>`
- `GET /v1/agents/<agent-id>/logs?stream=stdout|stderr|both&after=…&tail=…&follow=0|1`

Logs are sensitive. They are owner-only local spools, limited to 32 MiB per
agent; evicting old complete records advances `dropped_before` rather than
silently truncating. Readers use `next_cursor` and must handle that explicit
loss marker. Known bearer/API-key/private-key forms are redacted before the
spool is written, but redaction is not a guarantee—treat the relay bearer
token as granting log access and rotate it after suspected exposure.

After a relay restart, live process groups become `orphaned` (their former
pipes cannot be reattached); dead groups become `lost_after_restart`. Terminal
registry records and their bounded spool metadata are pruned after seven days.
The relay remains HTTP only on loopback/Tailscale; the approved forwarding
proxy terminates TLS for orchestrator-facing HTTPS. The bearer token and
GUI-session Keychain allowlist are unchanged. The legacy kill route is only
enabled when a distinct `--control-secret-file` (or
`ZIGZAG_CONTROL_SECRET_FILE`) is configured.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Release signing and deploy (Mac)

Every `cargo build` re-generates the binary's ad-hoc signature (new
identifier, new cdhash), so the keychain treats each rebuild as a different
app and re-prompts for allowlist access. Sign every release build with a
stable certificate identity instead:

```sh
./scripts/sign-release.sh
```

This builds, signs with `Apple Development: Shukant Pal` under the fixed
identifier `com.shukantpal.zigzag`, verifies, and restarts the LaunchAgent.
Run it in an interactive Mac terminal, never over SSH: code signing needs
the login keychain, and restarting the LaunchAgent from SSH puts the daemon
in the wrong macOS security session (its keychain reads hang and `/v1/exec`
stops responding). The script refuses to run over SSH.

The first run after switching to stable signing triggers one keychain
prompt when the daemon first reads the allowlist; choose **Always Allow**
so all future rebuilds keep working with no further prompts.

### Allowlist management

```sh
# Read the current policy (GUI session only)
zigzag config get-allowlist
# Replace the entire policy with the JSON in FILE (GUI session only).
# This REPLACES, not merges: export first, edit, then set.
zigzag config set-allowlist --file /path/to/policy.json
```

Policy JSON shape: `{"bins": {"<name>": {"path": "/abs/path", "commands": [["sub", "..."]], ...}}}`.
`commands` entries are argv prefixes. The `gh` bin also accepts
`"gh_read_repos": ["owner/repo"]` to scope `gh api` / `pr` commands.

## Linux VM build

The poller uses only the Rust standard library. Cross-compile for the VM after
installing the target and a compatible linker:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo build --release --target x86_64-unknown-linux-gnu -p poller
```

For a static binary, if the musl target/toolchain is available:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p poller
```

See [launchd/INSTALL.md](launchd/INSTALL.md) for Mac installation and both
binary command-line interfaces.
