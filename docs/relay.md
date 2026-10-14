# Rust relay

The `zigzag` binary in `zzd/src/main.rs` is the authenticated Mac-local
control plane. The normal server requires `--secret-file PATH` (or
`ZIGZAG_SECRET_FILE`) and `--state-file PATH` (or `ZIGZAG_STATE_FILE`).

## Server options and subcommands

| Option | Meaning |
| --- | --- |
| `--control-secret-file PATH` | Enables legacy generic proc kill with a distinct secret. |
| `--port N` | Default: 8765. |
| `--tailscale-ip IP` | Test override; must be a `100.64.0.0/10` IPv4 address. Otherwise uses `tailscale ip -4`. |
| `--max-events N` | Positive live-event retention bound; default 1000. |
| `--watch-repo OWNER/REPO`, `--watch-interval S` | Legacy GitHub watch (30–3600 seconds), used only while Rust review loop is not authoritative. |
| `--update-dir PATH`, `--update-interval S`, `--update-policy …` | Signed-update controls; interval 0 disables scheduled checks. |
| `--update-ready-file PATH` | Internal replacement/watchdog handoff. |

`zigzag config get-allowlist`, `zigzag config set-allowlist --file PATH`,
`zigzag timeline TASK_ID --state-file PATH`, and
`zigzag updates --dir PATH status|pause|pin VERSION|unpin` are operational
subcommands. `update-watchdog` is internal.

## Endpoint reference

All routes require the relay bearer token unless specifically noted.

| Route | Purpose |
| --- | --- |
| `GET /v1/health` | Authenticated liveness. |
| `POST /v1/events`, `GET /v1/events?after=&epoch=&timeout=` | Persist or long-poll lifecycle events. |
| `POST /v1/exec` | Synchronously run `{id,bin,args}` only when allowlisted. Denials are opaque. |
| `POST /v1/spawn`, `GET /v1/proc/{id}` | Compatibility background process launch/status. Spawn accepts optional safe `execution_id`. |
| `POST /v1/proc/{id}/kill` | Legacy process-group kill; needs control secret and may be disabled. |
| `GET, POST /v1/agents` | List durable agents or create a relay-native provider agent. |
| `GET, DELETE /v1/agents/{id}` | Read lifecycle, stop a live agent, or delete a succeeded agent through safe completion cleanup. |
| `POST /v1/agents/{id}/pause`, `/resume` | Stop/continue an agent process group. |
| `GET /v1/agents/{id}/logs` | Bounded redacted stdout/stderr spool; supports cursor/tail/follow. |
| `GET /v1/agents/{id}/transcript` | Transcript for API-created Codex agents, where available. |
| `GET /v1/providers` | Available providers and supported models. |
| `POST, DELETE /v1/worktrees` | Create/remove permitted worktrees. |
| `GET /v1/review-gate?repository=&pull_request=` | Review, CI, and human-approval decision. |

Successful agent completion removes the succeeded registry record automatically,
including records left by earlier relay versions during startup. A linked
worktree is removed only when it is clean, its branch has no commits ahead of
its configured upstream, and no other live agent record refers to it. Otherwise
the relay retains the worktree and logs why. The branch is retained. A manual
DELETE of a running Codex or OpenCode agent interrupts its session and returns
without worktree cleanup; the generic process path performs legacy worktree
cleanup after stopping, except when a pull request is being watched.

`openapi.yaml` documents wire formats but does not yet enumerate every
implemented route; check `zzd/src/routes/` when they differ.

## Worktrees and provider agents

`POST /v1/agents` takes a prompt (inline or file), `project_dir`, and exactly
one of `branch`, `no_branch: true`, or `pr`. Branch mode creates or reuses a
worktree under `/private/tmp/<branch-slug>/` by default. PR mode resolves the
head branch with `gh pr view` and uses a worktree. No-branch mode runs directly
in `project_dir` without creating a worktree or checking out a branch. Capacity
defaults to 16.
Worktree routes canonicalize paths, permit only `/private/tmp` or
`/Users/shukant/.codex/worktrees`, reject traversal, and will not remove a
live agent's worktree. Stopping an agent deliberately does not clean it up.

### Worktree deletion hook contract

A repository may provide lifecycle scripts at
`<repository-root>/.zigzag/repo-start.sh` and
`<repository-root>/.zigzag/repo-stop.sh`. The relay finds the owning repository
from Git worktree metadata; it does not read hooks from the worktree being
deleted. Missing scripts are ignored.

**Trust is an explicit local operator decision.** The relay executes no
repository hook unless that repository's canonical root appears in the
`ZIGZAG_TRUSTED_HOOK_REPOS` environment variable supplied to the relay. This is
a platform path list (colon-separated on macOS/Linux); configure it in the
relay's local service environment, for example:
`ZIGZAG_TRUSTED_HOOK_REPOS=/Users/me/Workspace/project-a:/Users/me/Workspace/project-b`.
An unset, empty, invalid, or nonmatching list skips the hook and writes a
warning. Trust applies to the repository root and therefore to both start and
stop hooks it contains, including hooks reached by the manual-deletion watcher.
The relay trusts the operator's local configuration, not a declaration made
inside the repository. Review the repository and its hook scripts before
adding its canonical root.

For relay-initiated deletion, the relay runs the hook before removing the
worktree. A two-second poll of configured worktree roots also detects manual
`git worktree remove` operations for worktrees the relay observed before
deletion, then runs the same hook. A manual removal before the relay's first
scan or while it is stopped cannot be detected.

The relay invokes the script through `/bin/sh`, passing the absolute worktree
path as its first argument. It clears the daemon environment and sets only
`PATH=/usr/bin:/bin`, `LANG=C`, `LC_ALL=C`, and the documented hook variables;
shells may also set `PWD`, `SHLVL`, and `_`. This keeps unrelated daemon tokens and secrets out of
the child environment. The hook still runs as the relay's logged-in user, with
that user's filesystem permissions and access to resources available through
the OS (including the user keychain). Environment scrubbing is not a sandbox:
trusted hooks must be treated as local code running with the relay user's
privileges. `ZIGZAG_WORKTREE_PATH` is the same absolute path;
`ZIGZAG_WORKTREE_REPO` is the owning repository root.
`ZIGZAG_WORKTREE_OBSERVED` is `0` for relay-initiated deletion and `1`
when the relay watcher detected a manual deletion. The working directory is
the worktree while it still exists, and the owning repository root after a
manual removal.

Hooks are best-effort: a nonzero exit does not cancel worktree deletion. The
relay logs each invoked hook's name, target, exit status, stdout, and stderr;
nonzero exits and invocation errors are logged as warnings. Repositories own
the behavior and safety of their hook scripts. Zigzag does not ship
repository-specific deletion cleanup.
