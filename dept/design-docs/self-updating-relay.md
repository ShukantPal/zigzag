# Design: Self-updating Zigzag relay with verified builds

## Decision

Zigzag should update itself from a GitHub Actions release once an hour, only
after it cryptographically verifies the downloaded macOS binary's digest and
GitHub build provenance. The relay, not SSH or `launchctl`, drains work and
`exec`s the verified image from its existing LaunchAgent process. That retains
the GUI login session on which Keychain access depends.

A floating **latest** GitHub Release is only a discovery pointer, not a trust
boundary. The updater accepts only a newer, verified release and keeps a
last-known-good image for automatic rollback.

## Release contract

Keep the current test CI, but add a release job on successful pushes to
`main`. It builds the supported macOS target with `cargo build --release
--locked`; `Cargo.lock` is committed today, so a lockfile mismatch fails rather
than selecting new dependencies. Pin the Rust toolchain, runner image, and
Actions by immutable revision. Existing test, Clippy, formatting, and
configuration-materialization gates run before publication.

The job signs the binary with Zigzag's established Apple identity and fixed
bundle identifier, using protected release credentials; that preserves the
Keychain designated requirement. It publishes a commit-addressed binary,
SHA-256 file, and manifest (commit, version, target, digest) to a GitHub
Release; latest points to that version. GitHub Actions generates provenance
with `actions/attest`, with only `contents: read`,
`id-token: write`, and `attestations: write` permissions. This is GitHub's
SLSA Build Level 2 provenance, not a claim that the binary is harmless.

Do not make byte-for-byte reproducible Rust builds a release gate now.
They would be a useful independent check, but matching macOS toolchains,
linkers, signing timestamps, and runner images is substantial work.
Attestation answers the
urgent question—*which reviewed commit and workflow produced this exact
digest*—even when two valid builds differ. Pinning inputs narrows variance;
periodically experiment with reproducibility later, without weakening the
provenance check.

## Safe update transaction

An hourly updater fetches release metadata, respecting a local pause flag and
an optional pinned version. It rejects a non-matching target, a version at or
below the locally accepted version, any SHA-256 mismatch, invalid macOS code
signature, or absent/invalid provenance. Verification binds the binary's hash
to the exact `ShukantPal/zigzag` release workflow, repository, `main` commit,
and pinned GitHub/Sigstore trust root. The root and expected workflow identity
ship in the already trusted relay; root rotation is an explicit, reviewed
release change and fails closed until available. Downloaded release metadata
never changes those expectations.

The candidate is written and synced in a versioned local directory, then an
atomic `current` pointer changes only after all checks pass; the previous
pointer remains last-known-good. On update, `/v1/spawn` refuses new work while
reads and event delivery stay available, until the durable agent registry has
no `running` entry. An operator may instead leave updates paused; it never
kills or labels live work complete to make progress.

Before `exec`, the old relay starts a narrowly scoped watchdog in the same GUI
session. The candidate re-binds listeners, serves its authenticated local
`/v1/events` health probe, opens the durable state, and acknowledges readiness
within a bounded window. It then becomes last-known-good and dismisses the
watchdog. A failed probe, startup failure, or missing acknowledgement makes
the watchdog `exec` the saved binary. Thus both normal upgrade and rollback
remain descendants of the GUI LaunchAgent—no SSH-session restart and no
`launchctl bootout/bootstrap` path. A crash after acknowledgement is left to
the existing LaunchAgent `KeepAlive` policy, which starts the known-good
current image on its next launch.

## Operations and audit

Default cadence is hourly, with `updates=paused` and `updates=pin:<version>`
as durable local controls. The existing `dept status` surface should show the
configured policy, current accepted version, candidate result, and last check time.
Each attempt is a schema-v1 `mac-relay` audit execution (for example,
`task_id=relay-update` and a release-specific execution ID). It records
`relay_update_started`, `relay_update_applied`, or `relay_update_failed`, with
old/new versions and digest but no credentials. The existing durable audit
archive and `zigzag status` execution view therefore retain the applied-update
fact across bounded live-queue eviction and restart.

## Security boundary and rollout

Auto-update is remote code execution by design. To ship a malicious relay, an
attacker must produce an artifact whose digest has valid provenance from the
pinned Zigzag release workflow and identity, or compromise the local trusted
binary/state. A substituted release asset, forged manifest, ordinary network
attacker, or unrelated GitHub workflow fails verification. Compromise of a
repository maintainer, protected release workflow, GitHub Actions credentials,
or the GitHub organization can legitimately produce trusted malicious code;
protecting against that is explicitly out of scope.

Roll out in observe-only mode first: create and verify releases, have the
relay report candidates without swapping, then enable drain/re-exec on this
Mac and exercise both forced health-check rollback and pause/pin behavior.

## External basis

GitHub documents binary provenance through `actions/attest`, the required
OIDC/attestation permissions, identity-constrained verification, and offline
verification with a custom trusted root: [artifact attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations), [binary provenance](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations), and [offline verification](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/verify-attestations-offline).
