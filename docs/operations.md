# Operations

## First five minutes

Run read-only checks before restart, kill, or redispatch:

```sh
launchctl print "gui/$(id -u)/com.shukantpal.zigzag"
tail -n 100 ~/.codex/zigzag/zigzag.error.log
zzapi health
zzapi agents list --state running
python3 dept/status.py --once
python3 dept/dept.py status t-abcdef
python3 dept/dept.py result t-abcdef
```

LaunchAgent status and authenticated health are separate checks. Never print
or copy the relay token.

## Stuck-task runbook

1. Identify transport/task with `dept.py list` and `dept.py status TASK`; if there is no ledger, inspect the task directory and relay agent list before choosing a kill handle.
2. Check relay health: `launchctl print`, `zzapi health`, and the error-log tail. A registered agent with failed health is daemon/startup/token/network trouble, not task completion.
3. For relay work, inspect `zzapi agents list --state running`, `zzapi agents get ID`, `zzapi agents logs ID --stream stderr --tail 1500`, then `dept.py result TASK`. `orphaned`, `lost_after_restart`, `audit_degraded`, `log_degraded`, and `dropped_before` show restart/data-loss conditions.
4. For SSH work, use `dept.py result TASK`, then inspect `pid`, `child-pid.txt`, `exit-code.txt`, `events.jsonl`, `stderr.log`, and `last-message.txt` beneath `~/.codex/dept/TASK`.
5. Check `python3 dept/status.py --once --all`; use the real Rust binary's `timeline` for chronology, not the stale wrapper. Cross-clock timestamps are not durations.
6. Only then stop: `zzapi agents stop ID` for relay-native agents, or `dept.py kill TASK` for manager-owned tasks. Stopping leaves a worktree.

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
python3 dept/config.py --check
python3 -m unittest discover -s dept -p 'test_*.py'
bash scripts/e2e-full-stack.sh target/debug/zigzag target/debug/zzapi
```

`scripts/sign-release.sh` is the interactive Mac deploy helper: build, sign,
verify, and restart the LaunchAgent. It deliberately refuses SSH. Supporting
scripts sign CI output, verify release material, install the materialization
hook, launch hardened reviewers, and run full-stack E2E.
