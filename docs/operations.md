# Operations

## First five minutes

Run read-only checks before restart, kill, or redispatch:

```sh
launchctl print "gui/$(id -u)/com.shukantpal.zigzag"
tail -n 100 ~/.codex/zigzag/zigzag.error.log
zzapi health
zzapi agents list --state running
zzapi status
zzapi events --follow
```

LaunchAgent status and authenticated health are separate checks. Never print
or copy the relay token.

## Stuck-task runbook

1. Identify the agent with `zzapi agents list --state running`, then inspect its state and logs before choosing a stop action.
2. Check relay health: `launchctl print`, `zzapi health`, and the error-log tail. A registered agent with failed health is daemon/startup/token/network trouble, not task completion.
3. Inspect `zzapi agents get ID` and `zzapi agents logs ID --stream stderr --tail 1500`. `orphaned`, `lost_after_restart`, `audit_degraded`, `log_degraded`, and `dropped_before` show restart or data-loss conditions.
4. Review event history with `zzapi events --follow`; use the daemon's `zigzag timeline TASK_ID --state-file PATH` subcommand when you need a task chronology. Cross-clock timestamps are not durations.
5. Only after reviewing state and output, stop with `zzapi agents stop ID`. Stopping leaves a worktree.

## Common failures

| Symptom | Safe response |
| --- | --- |
| LaunchAgent runs but health fails | Read error log; verify bind/token permissions; restart only through documented GUI-session deploy flow. |
| VM cannot reach relay but Mac health works | Check ACL, current Tailscale IP, proxy, and token transport. |
| `/v1/exec` hangs or policy load fails | Restart from a GUI session; Keychain is unavailable/nonfunctional under SSH. |
| `denied` from exec/spawn | Adjust policy only through GUI-session `zigzag config`; do not weaken the allowlist blindly. |
| `orphaned` agent | Inspect process/output deliberately; relay lost pipe attachment, not necessarily the process. |
| Log gap or events `lost`/`reset` | Rebuild from audit/registry; missing bounded-spool or live-event data is not recoverable there. |
| Review round `attention` | Inspect reviewer results, correct dispatch/config, and seed a new exact-head round. |
| Gate fails despite model approvals | Use the gate report; satisfy CI, lens, current-head, and human approval requirements. |

## Validate and deploy

```sh
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
bash scripts/e2e-full-stack.sh target/debug/zigzag target/debug/zzapi
```

`scripts/sign-release.sh` is the interactive Mac deploy helper: build, sign,
verify, and restart the LaunchAgent. It deliberately refuses SSH. Supporting
scripts sign CI output, verify release material, install the materialization
hook, launch hardened reviewers, and run full-stack E2E.
