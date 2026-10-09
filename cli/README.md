# zigzag-cli

Command-line client for the [zigzag relay](https://github.com/ShukantPal/zigzag) REST API.
Replaces hand-rolled curl commands. Zero dependencies — only the Python standard library.

## Install

```bash
chmod +x cli/zigzag-cli
ln -s "$PWD/cli/zigzag-cli" ~/bin/zigzag-cli   # or anywhere on your PATH
```

Requires Python 3.8+.

## Config

| Flag / env | Default | Purpose |
|---|---|---|
| `--url` / `ZIGZAG_URL` | `http://100.101.237.83:8765` | relay base URL |
| `--token-file` / `ZIGZAG_TOKEN_FILE` | `~/.codex/zigzag.token` | file holding the bearer token |
| `ZIGZAG_TOKEN` | — | bearer token directly (overrides the file) |
| `ZIGZAG_PROXY` | — | HTTP proxy URL (needed when reaching the relay from the VM) |
| `--json` | — | print raw JSON instead of human-readable tables |

## Usage

```bash
# health
zigzag-cli health

# agents
zigzag-cli agents list
zigzag-cli agents list --state running
zigzag-cli agents get <id-or-prefix>
zigzag-cli agents create --prompt "Fix the flaky test" \
    --project-dir /Users/shukant/Workspace/leveled-inc/leveled \
    --branch codex/fix-flaky
zigzag-cli agents logs <id-or-prefix> --follow
zigzag-cli agents logs <id> --stream stderr --tail 5000
zigzag-cli agents pause <id>      # relay PR in flight
zigzag-cli agents resume <id>     # relay PR in flight
zigzag-cli agents stop <id>       # relay PR in flight

# worktrees
zigzag-cli worktrees create --path /private/tmp/my-branch/ --branch my-branch \
    --repo /Users/shukant/Workspace/ShukantPal/zigzag
zigzag-cli worktrees delete --path /private/tmp/my-branch/

# synchronous exec (allowlisted bins only; denials exit 1)
zigzag-cli exec --bin gh --args api repos/leveled-inc/leveled

# spawn a supervised background process
zigzag-cli spawn --bin codex-launch --args run --prompt-file /tmp/p.txt
zigzag-cli proc get <proc-handle>

# event stream (like the poller, but interactive)
zigzag-cli events --follow
zigzag-cli events --after 2800 --timeout 30

# review gate for a PR
zigzag-cli review-gate --repo leveled-inc/leveled --pr 1031
```

Agent IDs accept a unique prefix — `zigzag-cli agents logs 360fc6b36a9d` resolves
the full handle for you.

## API contract

Requests and responses follow [`openapi.yaml`](../openapi.yaml), the relay's
OpenAPI 3.0 spec. The `agents create/pause/resume/stop` commands implement the
spec'd contract; they activate once the corresponding relay endpoints land.

## Exit codes

- `0` — success
- `1` — API error (auth failure, 404, allowlist denial, non-zero exec exit)
- `2` — usage / config error
- `130` — interrupted (Ctrl-C during `--follow`)
