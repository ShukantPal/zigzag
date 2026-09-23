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
- `DELETE /v1/agents/<agent-id>` — gracefully stop the agent (SIGTERM, then
  SIGKILL) and deregister it; the worktree is left in place

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

## Network security: Tailscale ACLs

The relay never binds to a public interface. On startup it listens on
`127.0.0.1` and on the Mac's Tailscale IPv4 address (`100.64.0.0/10`,
resolved via `tailscale ip -4`; `--tailscale-ip` overrides it for testing and
is rejected unless it is a Tailscale IPv4 address). The default port is 8765.

Binding to the tailnet is necessary but not sufficient: **Tailscale ACLs are
the relay's network-level access control.** The relay speaks plain HTTP on the
tailnet and every route requires the bearer token — but any tailnet device
that can reach the port can attempt authentication indefinitely, probe for
weaknesses, and burn relay resources. The ACL is what keeps that set to
exactly the orchestrator. Tailscale's default ACL allows all tailnet traffic
(`*` to `*:*`), so if you have never edited your ACLs, every device on your
tailnet can already reach the relay's port.

### Recommended policy

In the Tailscale admin console, restrict the relay's port to the devices that
need it — the owner and the tagged orchestrator nodes — and nothing else:

```json
{
  "tagOwners": {
    "tag:zigzag-client": ["autogroup:member"]
  },
  "acls": [
    {
      "action": "accept",
      "src": ["autogroup:member", "tag:zigzag-client"],
      "dst": ["100.x.y.z:8765"]
    }
  ]
}
```

Replace `100.x.y.z` with the Mac's Tailscale IPv4 address (`tailscale ip -4`
on the Mac). Notes:

- Prefer a tag (`tag:zigzag-client`) for the orchestrator VM over naming
  individual devices, so a rebuilt VM keeps access without an ACL change —
  but keep the tag's membership minimal.
- Do not open the port to `autogroup:shared` or `*`: shared nodes and future
  tailnet members would gain network access to the relay.
- The relay host itself needs no inbound rule beyond this one; with a
  default-deny policy, everything not explicitly accepted is dropped.

### If the ACL is misconfigured

- **Too open** (the default allow-all, or the relay port left reachable
  during a default-deny migration): every tailnet device — including
  compromised or shared nodes — can reach the relay. The bearer token still
  guards every route, and the exec allowlist can only be changed from the
  Mac's GUI login session, but a network-reachable attacker can attempt
  authentication without limit, exploit any future unauthenticated endpoint,
  and run resource-exhaustion attacks against the HTTP server.
- **Too closed** (orchestrator not in `src`, wrong IP in `dst`): the poller
  and orchestrator tooling lose connectivity — events stop flowing and
  `/v1/spawn` calls fail. The relay itself keeps running; this fails safe,
  not open.
- **Stale IP in `dst`**: the Mac's Tailscale address can change (reinstall,
  `tailscale logout`/`login`). If the relay becomes unreachable after such a
  change, compare `tailscale ip -4` on the Mac against the ACL.

When in doubt, verify from both sides: from an allowed host, `curl` against
the Tailscale IP without the bearer token should return `401` (reachable and
authenticated), and with the token `200`. From anything not in `src`, the
connection should time out — if it instead returns `401`, the ACL is too open.

## Mac-owned review loop

At startup, the daemon reads `~/.zigzag/config.yaml`, validates it against the
embedded draft 2020-12 JSON Schema (including the `regex` format), and rejects
YAML duplicate keys, custom tags, unknown fields, and duplicate repository
entries. Configuration errors fail closed
for reviews only and are logged with paths; the relay keeps serving.

When enabled, Zigzag owns a durable state machine for each
`(repository, pull request, head)` in `events.reviews.json`. It dispatches
independent configured lenses through a dedicated tool-free Codex launcher,
admits only bounded version 1 JSON results for the exact current head, and
publishes validated verdicts under the trusted GitHub identity. It evaluates
every matching required CI check and emits `review_ready` or bounded
`review_findings` events. Findings resume the owning Codex session only as
escaped, explicitly untrusted JSON claims that the owner must independently
verify. A new head or base commit supersedes the old comparison and approvals,
and a merge kills outstanding reviewer and owner-resume process groups,
including recovered orphan groups. Comparisons at GitHub's 300-file response
cap are rejected as potentially truncated and put the round in `Attention`.

Set `ZIGZAG_REVIEW_LOOP_SHADOW=1` during the migration comparison window.
Shadow mode runs discovery, verdict admission, and gate decisions alongside
the VM tooling, preserves the legacy `--watch-repo` `github_pr_opened` feed,
and suppresses agent dispatch, owner resume, and process kills.
Remove it only after decisions match and the VM review jobs have drained and
stopped. See [launchd/INSTALL.md](launchd/INSTALL.md) for configuration and
cutover.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Verified relay updates

On every successful `main` build, CI produces a signed ARM64 relay binary, a
SHA-256 manifest, and GitHub SLSA provenance attestations for both files. The newest
non-prerelease GitHub Release is the floating discovery location; it is never
trusted merely because it is named `latest`.

The relay checks hourly by default. Before accepting an update it requires the
release target/version to be newer, the manifest digest to match, the expected
Apple-anchored code-signing identifier and team, an attested manifest, and an
attested binary verified with the bundled GitHub and Sigstore trust roots.
Verification constrains the repository, `ci.yml` workflow, and `main` source
ref/commit. A root rotation is a reviewed source change embedded in each relay;
downloaded release metadata and stale on-disk material cannot replace it.

Install the initial relay under the managed directory and configure the
LaunchAgent to execute its stable `current` symlink, for example
`~/.codex/zigzag/relay/current`. This ensures LaunchAgent recovery starts the
last known-good release after an acknowledged update. The running relay keeps
the previous image, stops accepting `/v1/spawn` requests, waits for the durable
agent registry to have no `running` records, atomically switches `current`, and
`exec`s the candidate. A small child watchdog stays in the same GUI session;
if the replacement cannot bind, open state, and answer authenticated
`/v1/health` within one minute, it `exec`s the saved binary. This deliberately
does not invoke `launchctl bootout` or `bootstrap`, preserving Keychain access.

The durable local controls are:

```sh
zigzag updates --dir ~/.codex/zigzag/relay status
zigzag updates --dir ~/.codex/zigzag/relay pause
zigzag updates --dir ~/.codex/zigzag/relay pin v0.1.42
zigzag updates --dir ~/.codex/zigzag/relay unpin
```

`--update-interval SECONDS` changes the cadence (`0` disables scheduled
checks); `--update-policy enabled|paused|pin:VERSION` supplies the initial
policy. The `update-status.json` control file records the accepted version,
policy, last check, and candidate result. Update attempts and the applied or
failed result are schema-v1 `mac-relay` audit events under `task_id=relay-update`.

### Update threat model

Auto-update is deliberate remote code execution. A network attacker, a forged
manifest, an altered release asset, or an unrelated GitHub workflow cannot
pass the digest and identity-constrained attestation verification. Shipping a
malicious relay requires valid provenance records from the pinned Zigzag CI
workflow (or compromise of the local trusted binary/state). A
repository maintainer, protected workflow, GitHub Actions credential, or GitHub
organization compromise can still produce a trusted malicious release; those
are the remaining trust assumptions, not claims this mechanism eliminates.

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

### OpenCode pilot runner

`scripts/opencode-launch` is the only OpenCode launcher owned by Zigzag. A
department client stages a directory, asks the existing `/v1/spawn` flow to
invoke the runner, and records the returned process handle. It must not
construct a separate `opencode run` command itself.

The runner accepts exactly one operation and an absolute staged-task directory:

```sh
scripts/opencode-launch run --task-dir /absolute/staged-task
scripts/opencode-launch resume --task-dir /absolute/staged-task
```

Each staged task has two UTF-8, regular (non-symlink) input files:

```text
prompt.txt
runtime.json
```

For `run`, `runtime.json` requires an absolute, non-symlink `project_dir` and
may contain `model` and `title`. The model defaults to
`opencode/muse-spark-1.3-contributor-free`; selecting another free model is a
staged `model` setting, not a CLI override. For `resume`, it instead requires
only `project_dir` and `session_id`. Before resuming, the runner exports the
session and requires its project and effective model to match the staged
project and a free Zen model.

The pilot deliberately does not accept a staged `agent`: an agent definition
can itself select a model or subagent, which cannot yet be attested as part of
this one-runner contract. The runner starts OpenCode in pure mode with a
minimal environment, a fresh configuration directory, and an empty inline
configuration for catalog inspection; it discards caller-supplied
`OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG_DIR`, and
model-catalog overrides. (The account's local model cache remains available so
the approved default catalog is not replaced by an empty-home fallback.) It
pins both primary and small-model config to the approved model, then exports
the completed session and verifies the effective project, model, standard
`build` agent, and every assistant turn before reporting success.

Project roots are source-controlled OSS/Talon allowlists. Models are checked
against OpenCode's current local `models opencode --verbose` metadata: the
provider must be `opencode`, the endpoint must be Zen, and every reported cost
must be zero. This admits newly available free Zen models (including
`opencode/big-pickle`) while rejecting OpenAI, Anthropic, and paid models.

After every invocation the task directory contains the raw structured stream
in `opencode-events.jsonl`, stderr in `opencode-stderr.log`, and rendered
artifacts: `opencode-result.txt`, `opencode-session-id.txt` (when OpenCode
emits one), `opencode-usage.json`, and `opencode-run.json`. The result is the
last text event; usage is labeled `runtime: "opencode"` and aggregates input,
output, reasoning, cache-read, cache-write, and cost from every `step_finish`
event. `opencode-usage.json` also records `completed`, `stream_error`, and
`timed_out` so a child exit code of zero cannot hide an OpenCode error event,
malformed stream, non-finite usage, or a stream missing a terminal `stop`
completion.
Preflight rejection clears prior artifacts and writes the same structured
failure status whenever the staged task directory is usable.

The runner always invokes OpenCode with closed stdin and places `--` before
the staged prompt. Leaving stdin open makes non-interactive `opencode run`
wait forever for an interactive session; the option terminator keeps prompt
text from being interpreted as an OpenCode flag.

Install its relay policy from Shukant's GUI login session after reviewing the
absolute path for the deployed checkout. The following is a **policy fragment**
to merge under `bins`; `set-allowlist` replaces the entire policy, so first run
`zigzag config get-allowlist`, merge this entry with the existing bins, then
write the complete policy back with `zigzag config set-allowlist --file …`.

```json
{
  "bins": {
    "opencode-launch": {
      "path": "/Users/shukant/Workspace/ShukantPal/zigzag/scripts/opencode-launch",
      "commands": [["run"], ["resume"]]
    }
  }
}
```

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
