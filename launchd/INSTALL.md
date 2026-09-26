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

## GitHub PR watchdog events

The relay can discover open pull requests in explicitly watched repositories
and enqueue one durable `github_pr_opened` event per PR. This is the trigger
for the VM department worker; it does not run Codex or mutate GitHub from the
Mac. Add one `--watch-repo` argument for each repository to the reviewed
LaunchAgent arguments (beginning with `leveled-inc/leveled`):

```sh
zigzag --secret-file ~/.codex/zigzag/zigzag.token \
  --state-file ~/.codex/zigzag/events.json \
  --watch-repo leveled-inc/leveled \
  --watch-interval 30
```

The supplied LaunchAgent template includes this initial repository; add
additional `--watch-repo` argument pairs only after reviewing their scope.

The template starts the scan, but it fails closed (and logs a policy error)
until the owner installs the `gh` policy fragment below. Each scan then runs
exactly this read-only command through the policy boundary:

```sh
gh api --paginate --slurp repos/leveled-inc/leveled/pulls?state=open\&per_page=100
```

Newly discovered PRs produce a stable event id such as
`github-pr-opened:leveled-inc/leveled:42` and this payload:

```json
{"id":"github-pr-opened:leveled-inc/leveled:42","kind":"github_pr_opened","repository":"leveled-inc/leveled","pull_request":42,"url":"https://github.com/leveled-inc/leveled/pull/42"}
```

On a relay restart, and after bounded event-queue eviction, already-open PRs
may be rediscovered. `dept.py` must use the repository/PR pair as a durable
watchdog lease key (with the event id as its idempotency key), so recovery can
re-offer delivery without creating a second active watchdog.

### Department handoff

Run the existing VM poller continuously and send its JSON Lines to the
department event adapter. For a `github_pr_opened` event, that adapter creates
or resumes a Codex worker using the repository/PR lease key and event id from
the payload. The worker must remain active until CI is green and then keep
polling for later feedback. Its required loop is:

1. Read PR state and checks through Zigzag's read-only `gh` endpoint.
2. On every poll, read both issue comments and pull-request review comments.
3. For each unacknowledged comment by `ShukantPal`, add an eyes reaction in
   that same poll cycle, address the requested change on the PR branch, push
   the branch, and return to the CI watch.
4. Persist the GitHub comment id in the department task state before the next
   poll, so a restart cannot acknowledge or apply the same feedback twice.

The reaction and branch push are intentionally performed by the VM-side Codex
worker using its GitHub credentials, not by the Mac relay. The relay policy
below is read-only and cannot create reactions, merge PRs, or push code.

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
+      "gh_read_repos": ["leveled-inc/leveled"]
+    },
     "existing-binary": { "path": "/existing/path", "commands": [["existing-command"]] }
   }
 }
```

`/opt/homebrew/bin/gh` must be replaced with the owner's actual absolute `gh`
path. Zigzag additionally restricts the `gh` entry to the repositories named
in `gh_read_repos`, `pr list`, `pr view`, `pr checks`, and GET-only `api` calls
for PR discovery, issue comments, review comments, and reviews. It rejects
`--method`, `-X`, body flags, all other `gh` subcommands, and `--web` (including
`--web=true`), so this policy cannot be used to write GitHub state or read a
different repository.
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
