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

`runtime.json` is an object with a required absolute `project_dir` and optional
`model`, `title`, `agent`, and `session_id` strings. `resume` requires
`session_id`. The runner allows only its source-controlled OSS/Talon project
roots and only the listed free OpenCode Zen models. Its default model is
`opencode/muse-spark-1.3-contributor-free`; `--model` may select another
approved free model. It rejects all OpenAI, Anthropic, and paid/non-Zen model
IDs. Adding a project root or a model is therefore a reviewed code change.

After every invocation the task directory contains the raw structured stream
in `opencode-events.jsonl`, stderr in `opencode-stderr.log`, and rendered
artifacts: `opencode-result.txt`, `opencode-session-id.txt` (when OpenCode
emits one), `opencode-usage.json`, and `opencode-run.json`. Usage is labeled
`runtime: "opencode"` and includes input, output, reasoning, cache-read,
cache-write, and cost fields from the final `step_finish` event.

The runner always invokes OpenCode with closed stdin. Leaving stdin open makes
non-interactive `opencode run` wait forever for an interactive session.

Install its relay policy from Shukant's GUI login session after reviewing the
absolute path for the deployed checkout:

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
