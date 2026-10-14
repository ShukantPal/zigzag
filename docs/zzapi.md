# `zzapi` CLI

`zzapi` is the portable Rust client for the relay. It handles bearer
authentication, request construction, tables, unique agent-ID prefixes, and
relay/proxy configuration. Build it with `cargo build -p zzapi` or use the
installed release binary.

Configuration precedence is `--hostname` / `ZIGZAG_HOSTNAME` (default
`100.101.237.83:8765`), `ZIGZAG_TOKEN` or `--token-file` /
`ZIGZAG_TOKEN_FILE`, and optional `ZIGZAG_PROXY`. Without a supplied token
file it tries `~/.codex/zigzag/zigzag.token`, then `~/.codex/zigzag.token`.
Token files must be owned by the current user and mode 0600. Global `--json`
emits raw JSON.

```text
zzapi [--hostname HOST[:PORT]] update
zzapi health
zzapi agents list [--state running] [--task-id TASK]
zzapi agents get|pause|resume|stop ID
zzapi agents create (--prompt TEXT | --prompt-file PATH) --project-dir DIR
zzapi agents continue ID|BRANCH --prompt TEXT [--interrupt]
zzapi agents message ID|BRANCH (--steer TEXT | --queue TEXT)
                    (--branch BRANCH | --no-branch | --pr NUMBER)
                    [--worktree PATH] [--harness codex|gemini|opencode|antigravity|agy]
                    [--model MODEL]
                    [--approval-mode MODE] [--timeout-secs N]
zzapi agents logs ID [--stream stdout|stderr|both] [--after N] [--tail N]
                    [--follow] [--prefix]
zzapi worktrees create --path PATH --branch BRANCH --repo DIR
zzapi worktrees delete --path PATH
zzapi exec --bin BIN [--id ID] --args ...
zzapi spawn --bin BIN [--id ID] [--execution-id ID] --args ...
zzapi proc get ID
zzapi events [--after N] [--epoch E] [--timeout N] [--follow]
zzapi review-gate --repo OWNER/REPO --pr N
```

`zzapi update` downloads the latest `zzapi-linux-x86_64` GitHub release asset,
checks that it can run and report its version, then atomically replaces the
current executable. It does not require relay credentials or agent flags.

The CLI intentionally exposes the common surface only. Use an authenticated
HTTP client for provider and transcript endpoints until corresponding verbs
are added.

`agents continue` and `agents message` accept an exact branch name as the
target, for example `zzapi agents continue codex/fix-flaky --prompt "Check the
CI result"` or `zzapi agents message codex/fix-flaky --steer "Keep the change
limited to the parser"`. Existing agent IDs and unique ID prefixes keep
precedence. A branch target selects its single running agent; if multiple
running agents share the branch, the CLI lists their IDs and asks you to
disambiguate. A branch with no running agent is an error.
