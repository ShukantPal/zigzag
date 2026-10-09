# Configuration and security

## Relay launch and configuration

The deployed LaunchAgent normally uses the canonical checkout's release binary
with `--secret-file` and `--state-file`, restarts on failure, and logs to
`~/.codex/zigzag/zigzag.log` and `zigzag.error.log`. The checked-in template
is `launchd/com.shukantpal.zigzag.plist`; use `launchd/INSTALL.md` rather than
creating another job.

The relay's executable allowlist is held in macOS Keychain and may be loaded
only from a GUI login session. `zigzag config get-allowlist` and
`zigzag config set-allowlist --file PATH` are privileged GUI-session commands.
Do not bootstrap/restart through SSH: Keychain access can hang and `/v1/exec`
will cease to work even if the daemon remains alive.

## Auth and network boundary

All normal HTTP routes require `Authorization: Bearer <relay token>`.
`~/.codex/zigzag/zigzag.token` is sensitive, must remain private mode 0600,
and grants log/API access. The generic legacy proc-kill route has a separate
control secret and is absent when none is configured.

The daemon binds `127.0.0.1` and a Tailscale IPv4 address, never a public
interface. HTTP is intentionally plain inside that tailnet boundary; an
approved proxy provides orchestrator-facing TLS. Restrict port 8765 with a
Tailscale ACL to the owner and narrowly tagged orchestrator nodes. Binding is
not an ACL: a broad tailnet policy lets every device attempt authentication and
consume relay resources.

From an allowed host, an unauthenticated request should yield `401`; from a
disallowed host it should time out. If the VM cannot connect while Mac health
works, check ACL source/destination, current `tailscale ip -4`, proxy settings,
and token transport—never open the relay broadly to debug.

## Department and review configuration

`dept/config.py` produces the checked-in materialized config; deployment
`config.json`/`CODEX_DEPT_CONFIG` controls SSH/proxy/state location and stays
ignored. The Rust review policy lives in `~/.zigzag/config.yaml`; invalid YAML,
duplicate keys, tags, unknown fields, or invalid schema disable reviews
fail-closed while the HTTP relay continues serving.

Review configuration includes repository/lens/CI/human-actor policy. It is the
authority behind `/v1/review-gate`; Python `approval_gate.py` fetches, rather
than reimplements, that decision.

## Signed updates

The relay updater keeps release images, a `current` symlink, and status under
`~/.codex/zigzag/relay/`. It verifies version, digest, Apple signing identity,
and constrained GitHub/Sigstore provenance before switching. The watchdog
rolls back if the replacement cannot bind, open state, and answer authenticated
health within one minute. This is deliberate remote code execution: protected
CI/workflow, maintainer, GitHub organization, and local trusted state remain
important trust assumptions.
