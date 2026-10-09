# Design: Asset observability for the Codex department (Phase 3)

## Decision

Add an asset registry derived from durable lifecycle events and Mac-local inspection. Show it as an **Assets** pane in `dept status`; allow only an explicit, confirmed prune of assets that meet conservative safety rules. This makes worktrees and related local resources visible without changing task scheduling, agent execution, or the relay’s control surface.

The registry answers four questions that task status cannot: what was created, which task owns it, whether it is still in use, and whether it is safe to reclaim. Event history supplies attribution; local inspection supplies the current filesystem and Git facts. Neither is sufficient by itself.

## Scope and ownership

The initial registry tracks:

- **Worktrees** under `/private/tmp/<branch-slug>/`, with canonical path, Git branch/HEAD, dirty state, PR reference, and owner task/execution.
- **Managed temporary directories**, including detached-HEAD or throwaway directories, only when a worker registers them. Do not infer ownership for arbitrary `/private/tmp` paths.
- **Bazel output bases** a worker creates or selects, with resolved path and owner.

Containers are deferred until there is a department-managed ownership convention. Git branches are properties of worktrees, not independently pruned assets: a branch can outlive its checkout and needs repository-specific review.

Each asset has a stable ID, type, canonical locator, task and execution owner, creation/last-use times, lifecycle state, and optional PR reference. State is a projection: **active** while its owner is live or recently used; **stale** after policy expiry without a conflicting safety signal; and **orphaned** when a path has no valid owner. “Not observed” and “safe to prune” remain distinct.

## Lifecycle event contract

Workers publish `asset_created`, `asset_released`, and `asset_pruned` in the existing schema-v1 envelope: stable event ID, task/execution IDs, kind, source, RFC 3339 millisecond time, clock, and object payload. On-Mac workers use the accepted `mac-relay` source; the department VM uses `vm-department` for facts it observes. Retries reuse the complete ID and body.

`asset_created` records identity, type, locator, ownership, and creation facts. `asset_released` records no further need and updates last use; `asset_pruned` records successful removal and reason. Release is not deletion. The relay appends these facts to the per-execution audit log, so the registry survives restart and live-queue eviction. A later scan can report a missing path but never invent a prune event.

## User experience

`dept status` gains an Assets pane, toggled from the execution view. It lists active and attention-worthy assets first with type, task, branch/PR, age, last use, and state. Selection shows ownership, resolved path, Git cleanliness, available PR state, and classification evidence. The pane reads durable audit logs plus local Git/filesystem state; the live relay snapshot remains advisory.

The only mutation is `p` for **Prune**, available only for clearly prunable entries. The UI presents exact paths, reason, and irreversible effect, then requires confirmation. It rechecks before removal; changed, unreadable, or ambiguous state cancels the action. Every success emits `asset_pruned`; no other status key changes assets or task state.

## Retention and safety policy

PR-linked worktrees become *candidates* when their PR is merged or closed, never automatic deletions. They require a fresh clean-worktree, non-current-checkout, and no-running-owner check. Human confirmation is always required because closed PRs can still aid diagnosis.

Detached-HEAD managed temporary directories become candidates after 7 days without registered use or active owner. A separately enabled local GC may auto-prune clean, expired directories and record the action; the interactive default is confirmation. Bazel output bases are never auto-pruned in Phase 3: they may be shared or costly to rebuild. Any worktree with uncommitted changes is never pruned. Orphaned paths, unknown locations, and assets outside the approved convention always require confirmation.

## Non-goals and rollout

This phase does not alter scheduling, execution policy, branch deletion, or the worktree rule. It does not make status a file manager. Roll out observe-only first: emit and display records, compare them with the Mac, then enable confirmed pruning. Success is trustworthy ownership, not aggressive reclamation.

## Repository alignment

Zigzag already validates schema-v1 audit envelopes, accepts `vm-department` and `mac-relay`, archives execution-keyed events beside its state file, and caps that archive independently of the bounded live queue. `dept status` already merges durable archive data with read-only relay event and running-agent views. Phase 3 extends those boundaries without a second durable store or relay control endpoint.
