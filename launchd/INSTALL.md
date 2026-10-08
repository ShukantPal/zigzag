# Install on the Mac (owner-run)

Do not put the bearer token in a plist, shell history, command line, or agent
prompt. Create the token file with restrictive permissions:

```sh
mkdir -p ~/.codex/zigzag
umask 077
openssl rand -hex 32 > ~/.codex/zigzag/zigzag.token
chmod 600 ~/.codex/zigzag/zigzag.token
```

Build Zigzag, then edit every `/REPLACE/...` path in
`com.shukantpal.zigzag.plist`. The service resolves the Mac's Tailscale
IPv4 address at startup and binds only that address plus `127.0.0.1` on port
8765. Do not change it to `0.0.0.0`, enable Funnel, or add public forwarding.

Note: the daemon discovers the Tailscale IPv4 address by running
`tailscale ip -4`, so the `tailscale` CLI must be on the daemon's PATH.
If it lives outside the default PATH (e.g. a Nix install), add an
`EnvironmentVariables` -> `PATH` entry to the installed plist.

Copy and bootstrap the reviewed plist:

```sh
cp launchd/com.shukantpal.zigzag.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.shukantpal.zigzag.plist
```

This repository deliberately does not run either command for you.

For verified self-updates, make the `ProgramArguments` binary path the stable
`~/.codex/zigzag/relay/current` symlink and pass `--update-dir
~/.codex/zigzag/relay`. Seed `current` from the reviewed initial release before
bootstrapping. Do not restart the LaunchAgent to apply an update: the relay
drains and re-execs itself so it remains in this GUI login session.

## Zigzag

```sh
zigzag --secret-file ~/.codex/zigzag/zigzag.token \
  --state-file ~/.codex/zigzag/events.json [--port 8765]
```

Read a durable task timeline locally with the same state file (no relay token
or running HTTP listener is needed):

```sh
zigzag timeline TASK_ID --state-file ~/.codex/zigzag/events.json
```

`--secret-file` can instead be supplied by `ZIGZAG_SECRET_FILE`. The file must
not be group/world readable and its content must be at least 32 bytes. For
testing only, `--tailscale-ip` can set a specific Tailscale IPv4 address;
ordinary operation discovers it using `tailscale ip -4`.

## Mac-owned review loop

Zigzag reads `~/.zigzag/config.yaml` once at daemon startup. The file is
hand-written personal policy, not a generated repository artifact. Missing,
unreadable, malformed, or schema-invalid configuration disables only the
review loop; the relay and its other duties continue. Every validation error
is logged with its JSON path.

Create the directory, install the version 1 policy from the design, and then
restart the LaunchAgent:

```sh
mkdir -p ~/.zigzag
chmod 700 ~/.zigzag
$EDITOR ~/.zigzag/config.yaml
chmod 600 ~/.zigzag/config.yaml
```

Current Zigzag policy:

```yaml
schema_version: 1
review_loop:
  enabled: true
  intervals:
    discovery_seconds: 300
    review_seconds: 600
    merge_seconds: 300
  repositories:
    - repository: ShukantPal/zigzag
      full_rounds_max: 2
      verification_rounds_max: 2
      lenses: [correctness, simplicity, tests, security]
      require_security_lens: true
      required_ci_checks:
        - label: semgrep
          name_pattern: semgrep
        - label: BuildBuddy
          name_pattern: buildbuddy
      trusted_verdict_identity: ShukantPal
      result_limits:
        max_findings_per_lens: 20
        max_bytes_per_lens: 16384
```

The daemon persists one round per repository/PR/head as
`events.reviews.json`, dispatches independent reviewers, accepts only
versioned exact-head verdicts from the configured identity, checks required
CI, and resumes the owning Codex session with bounded findings. New heads
supersede old approvals, and merge polling terminates reviewer process groups.

For the migration comparison period, set `ZIGZAG_REVIEW_LOOP_SHADOW=1` in the
LaunchAgent environment. Shadow mode evaluates and emits decisions but never
dispatches, resumes, or kills agents. At cutover, drain and stop the VM review
jobs, remove the environment variable, and restart Zigzag.
`review_loop.enabled: false` is different: it starts no review loop at all.

Install the dedicated reviewer launcher beside the department launcher:

```sh
cp scripts/codex-review-launch.sh ~/.codex/dept/
chmod 700 ~/.codex/dept/codex-review-launch.sh
```

Reviewer agents use this launcher with Codex's shell tool and web search
disabled, a read-only sandbox, approval policy `never`, no user config/rules,
an empty process environment, and a JSON output schema. They receive only a
bounded PR patch as untrusted prompt data and cannot access credentials, run
commands, or post to GitHub. The daemon validates their result, then performs
the narrow trusted comment publication step. Owner resume continues to use
`codex-launch resume`.

GitHub discovery and gate reads remain read-only at the public execution
boundary. Every repository in the YAML must also be present in the
Keychain-held `gh_read_repos` policy.

### Owner approval required: proposed `gh` policy addition

Do **not** apply this from a remote session. This is an additive diff for the
owner's Keychain-held allowlist, to be reviewed and installed from the local
macOS GUI session with `zigzag config set-allowlist --file PATH`.

```diff
 {
   "bins": {
+    "gh": {
+      "path": "/opt/homebrew/bin/gh",
+      "commands": [
+        ["pr", "list"],
+        ["pr", "view"],
+        ["pr", "checks"],
+        ["api"]
+      ],
+      "gh_read_repos": ["ShukantPal/zigzag"]
+    },
+    "codex-review-launch": {
+      "path": "/Users/REPLACE/.codex/dept/codex-review-launch.sh",
+      "commands": [["run"]]
+    },
     "existing-binary": { "path": "/existing/path", "commands": [["existing-command"]] }
   }
 }
```

`/opt/homebrew/bin/gh` must be replaced with the owner's actual absolute `gh`
path. Zigzag additionally restricts the public `gh` entry to repositories in
`gh_read_repos`, `pr list`, `pr view`, `pr checks`, and GET-only `api` calls
(including exact-SHA compare reads). It rejects
`--method`, `-X`, body flags, all other `gh` subcommands, and `--web` (including
`--web=true`), so this policy cannot be used to write GitHub state or read a
different repository. The daemon's internal verdict publisher reuses only the
configured `gh` path and repository scope after validating a reviewer result;
`/v1/exec` still cannot invoke `gh pr comment`.
The policy is deliberately not stored in this repository and this change does
not touch the live Keychain item.

## Exec endpoint

Zigzag runs as a LaunchAgent inside the Mac's GUI login session, where the
macOS keychain is available. Its `POST /v1/exec` endpoint executes only a
binary and argv prefix named in the Keychain-held policy. SSH sessions cannot
read or modify that policy.

Shukant installs or updates it from his GUI login session by putting the JSON
policy in a file and running:

```sh
zigzag config set-allowlist --file /secure/path/exec-allowlist.json
```

Zigzag verifies that it is in a local graphical macOS session before either
reading or writing the policy, and refuses those operations from SSH. This
keeps Keychain policy access within the owner GUI-login session.
The command validates the policy, stores canonical JSON in the `zigzag` /
`exec-allowlist` Keychain item, then prints the stored normalized policy for
confirmation. macOS may prompt once to grant this specific Zigzag binary
"Always Allow" access to the Keychain item; accept that prompt only after
verifying the binary is the reviewed Zigzag build.

For an allowed operation, a caller uses the configured binary short name
rather than a caller-supplied path:

```sh
curl -s http://100.101.237.83:8765/v1/exec \
  -H "Authorization: Bearer $(cat ~/.codex/zigzag/zigzag.token)" \
  -H 'Content-Type: application/json' \
  -d '{"id": "list-repos-1", "bin": "jules", "args": ["remote", "list", "--repo"]}'
```

There is no shell or PATH lookup: arguments are passed directly to the absolute
path from policy. Execution is capped at 300 seconds and 1 MiB of captured
output per stream. A successful response looks like:

```json
{"id": "list-repos-1", "exit_code": 0, "stdout": "...", "stderr": "",
 "truncated": false, "timed_out": false}
```

Unknown binary names, disallowed prefixes, and malformed request bodies all
return the same opaque denial response (with the submitted id when usable):

```json
{"id": "list-repos-1", "error": "denied"}
```

### Detached processes

`POST /v1/spawn` accepts the same request body and policy as `/v1/exec`, but
returns immediately with a 128-bit hexadecimal process handle. Use
`GET /v1/proc/<handle>` to retrieve the current status and captured output, or
`POST /v1/proc/<handle>/kill` to terminate a still-running process group.
Output follows the same 1 MiB-per-stream limit as `/v1/exec`; completed
process records are retained for up to one hour (with at most 128 retained).
The relay terminates tracked process groups during normal shutdown and does not
restore process records after a restart.

## VM poller

Provision an identical mode-600 token file using the existing secret-delivery
mechanism. With the verified HTTP forward proxy:

```sh
ZIGZAG_PROXY="${HTTPS_PROXY%:*}:3130" poller \
  --zigzag-url http://100.101.237.83:8765 \
  --secret-file /secure/path/zigzag.token \
  --state-file /var/lib/muse/zigzag-cursor.json
```

The poller has `--once` for supervised smoke tests and accepts `--timeout`
(1–55 seconds, default 50). It retries network failures, reports reset/loss
warnings on stderr, and emits exactly one JSON object per stdout line.
