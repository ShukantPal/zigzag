# Zigzag Control-Plane Migration: Mac-Native Review Loops and Doc Routing

## Context

Zigzag is the Mac-hosted relay and daemon that lets Shukant's personal AI agent do work on his MacBook — running commands in his login session (where the macOS Keychain is reachable) and hosting the "engineering department": background Codex agents that implement features, open pull requests, and iterate on review feedback.

Today the department's control loops run on a Linux VM and reach the Mac over SSH (Tailscale) or the relay's HTTP API: watchers poll GitHub for PR comments, dispatch review teams, check review verdicts, enforce the merge gate, and poll Google Docs for design feedback. Every poll is a network round trip. The VM holds the state (watermarks, review-round files, session maps) while the work it governs happens on the Mac. This split is the source of most operational incidents: relay flakiness, SSH keychain gaps, and duplicate dispatches when the two sides disagree about what is running.

On 2026-09-26 we sketched moving the review loop onto the Mac daemon. This document extends that sketch to all GitHub loops plus design-doc comment routing, and fixes the role split: the Mac operates the control plane; Muse configures it.

## Direction

The Mac daemon becomes the control plane's execution site. It runs the GitHub review loops end to end — watching PR comments, dispatching configured review teams, polling verdicts, running the merge gate, resuming owners, killing reviewers on merge — and it routes Google Doc comments to the assigned Codex session. The loops are implemented in Rust inside the relay daemon: a port of the current Python watchers, not a reuse. The only code running on the Mac is the Rust binary. Muse remains the control-plane configurer. Chat surfacing (review-ready / decision-needed) and cross-repo work stay on the VM.

The later review-loop-specific decision for this implementation supersedes this document for review policy: that policy is hand-written personal YAML at `~/.zigzag/config.yaml`, with no Python authoring or materialization layer, and is loaded on daemon startup. Repository-authored configuration and materialization below continue to apply only to the other control-plane loops as they migrate.

## What moves to the Mac

1. **PR comment watching** — poll PR threads and review bodies, dispatch the owning session on new human feedback. Already Mac-adjacent (it manages Mac-side tasks); the SSH hop is pure overhead.
2. **Review rounds** — dispatch 3-lens reviewers, poll verdicts, resume the owner with findings. Retires the round-dispatch script and the review-round watcher from the VM.
3. **Merge gate** — the approval gate becomes a daemon-side check: required CI green on the latest head plus every lens APPROVE on that head.
4. **Merge killer** — on PR merge, terminate still-running workers on the dead branch.
5. **Dependabot watcher** — single security-lens review and auto-merge on a green gate.
6. **Doc-comment routing (new)** — the relay becomes a configurable router: it watches design docs and routes new comments straight to the assigned Codex session, replacing VM polling entirely.

## Auth: reuse the existing service account

No new credential is created. All design docs live in a single Drive folder shared once with the existing zigzag service account as Commenter; everything inside inherits the share. The daemon mints tokens from the key already in the Mac keychain — the same key verified end-to-end on 2026-09-28 (token mint, folder listing, comment poll). Commenter covers everything the loop needs: reading comments and posting the "picked up" acknowledgment reply. Thread resolution stays manual, as it is today.

## Config as code

Router config (doc → assigned session) and non-review-loop watcher config live versioned in the repo under `dept/`. Muse edits them; their consumers read them on startup and on SIGHUP where supported. Review policy is the deliberate exception: it lives in `~/.zigzag/config.yaml` and takes effect on daemon restart. Runtime state — watermarks, review-round files, session maps — moves to the Mac with the loops, single-homed, so there is exactly one writer.

## Config materialization

For non-review-loop control-plane configuration, the Python config is the source and JSON is a build artifact. Both are committed to the repo. The daemon never executes Python. The review loop does not consume this artifact; its personal YAML is validated directly against the daemon's embedded schema.

- **Commit time**: a versioned pre-commit hook (`scripts/githooks/pre-commit`, installed via `scripts/install-hooks.sh`) re-materializes the JSON on every commit touching the config source. The hook is convenience, not enforcement — it can be bypassed with `--no-verify`.
- **CI enforcement**: CI re-runs materialization and diffs against the committed JSON. Any mismatch — hand-edited JSON, or Python changed without regenerating — fails the build. A stale config can never merge.
- **Determinism contract**: materialization must be byte-identical for identical source — sorted keys, no timestamps, no environment-dependent values, no unordered iteration in the emit path. Nondeterministic output makes CI flaky and diffs unreviewable.
- **Reviewability**: the JSON diff in a PR is plain data — it is exactly what the daemon will run, and it is what gets reviewed. The Python diff is the logic behind it.

## What stays on the VM

Chat surfacing: review-ready and decision-needed notifications still come from the VM, fed by daemon events. Cross-repo loops (leveled) stay where they are; the same pattern can follow later. Anything needing his personal Google account beyond the SA-shared docs folder stays VM-side.

## Tradeoffs

**Single binary, no Python on the Mac (decided 2026-09-28).** The loops are ported to Rust inside the relay daemon — no Python watchers via launchd, no Python runtime in the daemon's path. The port cost is real (the watchers are battle-tested), but it buys one deployable and no split-brain between two runtimes on the same machine. Review-loop policy uses the personal YAML boundary defined by the later review-loop design; other migrated loops may retain repository-materialized configuration.

**Quiet hours.** Mac-side loops pause when the laptop sleeps (10pm–7am). Overnight review latency is the price; nothing is lost, because state is on disk and loops resume on wake. This is accepted behavior, not a failure mode.

**Migration split-brain.** During rollout, only one side owns side effects. The review loop first runs in shadow mode beside the VM tooling so decisions can be compared without duplicate dispatch or publication; observational shadow rounds accept the VM's trusted, structured, exact-head verdict comments without daemon generation markers, while authoritative rounds require their active generation marker. The VM approval gate queries the Mac binary's validated personal YAML policy so the comparison cannot drift on lenses, trusted identity, or CI requirements. Authority transfers only after decisions match. Other loops use an explicit owner flag and retire their VM cron as the daemon takes over.

**Rollout.** Phase 1: review loops (comment watcher, rounds, gate, merge killer). Phase 2: doc router and Dependabot watcher. Phase 3: retire the VM crons and delete the paused-watchers tracking.
