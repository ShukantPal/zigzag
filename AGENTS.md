# Repository guidance

## Small PR auto-merge

For a pull request that is ready to merge without the normal four-lens review
gate, Codex may add the `ready` label. This is an explicit decision to bypass
that review gate. Use the label only for small PRs: fewer than 5 changed files
and fewer than 500 total added and deleted lines.

The `auto-merge-ready` workflow removes `ready` from PRs over either size
limit or with a failing or canceled required check. It merges eligible PRs with
a merge commit once all required checks pass. When checks are still pending,
the label remains and the workflow retries when a check suite completes.
