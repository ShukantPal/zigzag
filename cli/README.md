# zzapi

Command-line client for the [zigzag relay](https://github.com/ShukantPal/zigzag) REST API.
Replaces hand-rolled curl commands. Single native binary, no runtime dependencies.

## Install

```bash
cargo build --release -p zzapi
cp target/release/zzapi ~/bin/   # or anywhere on your PATH
```

## Config

| Flag / env | Default | Purpose |
|---|---|---|
| `--hostname` / `ZIGZAG_HOSTNAME` | `100.101.237.83` | relay host (default port: 8765; an explicit `host:port` is accepted) |
| `--token-file` / `ZIGZAG_TOKEN_FILE` | `~/.codex/zigzag.token` | file holding the bearer token |
| `ZIGZAG_TOKEN` | — | bearer token directly (overrides the file) |
| `ZIGZAG_PROXY` | — | HTTP proxy URL (needed when reaching the relay from the VM) |
| `--json` | — | print raw JSON instead of human-readable tables |

Token files must be regular files owned by the current user with mode `0600`;
the client rejects group- or world-readable token files. `ZIGZAG_TOKEN` is
still useful for managed secret injection where no token file is present.

For `--follow` commands, `--json` emits [JSON Lines](https://jsonlines.org/):
one compact JSON response per line. This keeps an indefinite stream parseable.
When an event or agent-log cursor predates retained data, the response is still
printed with a warning, then the command exits `1` so automation cannot mistake
a partial stream for a complete one.

## Usage

```bash
# health
zzapi health

# agents
zzapi agents list
zzapi agents list --state running
zzapi agents get <id-or-prefix>
zzapi agents create --prompt "Fix the flaky test" \
    --project-dir /Users/shukant/Workspace/leveled-inc/leveled \
    --branch codex/fix-flaky

# Read-only investigation in the project checkout (explicit opt-in)
zzapi agents create --prompt "Trace the request flow" \
    --project-dir /Users/shukant/Workspace/leveled-inc/leveled --no-branch

# Work on an existing pull request's head branch
zzapi agents create --prompt "Address review feedback" \
    --project-dir /Users/shukant/Workspace/leveled-inc/leveled --pr 1031
zzapi agents logs <id-or-prefix> --follow
zzapi agents logs <id> --stream stderr --tail 5000
zzapi agents pause <id>      # relay PR in flight
zzapi agents resume <id>     # relay PR in flight
zzapi agents stop <id>       # relay PR in flight

# worktrees
zzapi worktrees create --path /private/tmp/my-branch/ --branch my-branch \
    --repo /Users/shukant/Workspace/ShukantPal/zigzag
zzapi worktrees delete --path /private/tmp/my-branch/

# synchronous exec (allowlisted bins only; denials exit 1)
zzapi exec --bin gh --args api repos/leveled-inc/leveled

# spawn a supervised background process
zzapi spawn --bin codex-launch --args run --prompt-file /tmp/p.txt
zzapi proc get <proc-handle>

# event stream (like the poller, but interactive)
zzapi events --follow
zzapi events --after 2800 --timeout 30
# low-latency event push over the relay's authenticated bidi socket
zzapi events stream
# use a non-default relay socket port when needed
zzapi events stream --socket-port 9876

# review gate for a PR
zzapi review-gate --repo leveled-inc/leveled --pr 1031
```

Agent IDs accept a unique prefix — `zzapi agents logs 360fc6b36a9d` resolves
the full handle for you.

## Exit codes

- `0` — success
- `1` — API error (auth failure, 404, allowlist denial, retention loss, failed or indeterminate exec)
- `2` — usage / config error
- `130` — interrupted (Ctrl-C during `--follow`; via the shell's SIGINT handling)
