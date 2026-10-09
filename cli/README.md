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
| `--hostname` / `ZIGZAG_HOSTNAME` | `100.101.237.83` | relay host (port is always 8765) |
| `--token-file` / `ZIGZAG_TOKEN_FILE` | `~/.codex/zigzag.token` | file holding the bearer token |
| `ZIGZAG_TOKEN` | — | bearer token directly (overrides the file) |
| `ZIGZAG_PROXY` | — | HTTP proxy URL (needed when reaching the relay from the VM) |
| `--json` | — | print raw JSON instead of human-readable tables |

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

# review gate for a PR
zzapi review-gate --repo leveled-inc/leveled --pr 1031
```

Agent IDs accept a unique prefix — `zzapi agents logs 360fc6b36a9d` resolves
the full handle for you.

## Exit codes

- `0` — success
- `1` — API error (auth failure, 404, allowlist denial, non-zero exec exit)
- `2` — usage / config error
- `130` — interrupted (Ctrl-C during `--follow`; via the shell's SIGINT handling)
