# Review system

Model review is advisory. A passing approval gate also needs configured green
CI and a formal review from an allowlisted human actor.

## Review-round workflow

1. Push the PR head and check CI, then run `python3 dept/dispatch_review_round.py PR --repo OWNER/REPO --project-dir DIR`. Add `--security` for auth, credentials, network, cryptography, or PII.
2. The dispatcher locks the PR round, resolves its precise GitHub head, marks it `dispatching`, archives that commit and its binary base diff, creates lens prompts, and launches fresh read-only `dept.py start --no-sop --read-only --ssh --task-id` agents.
3. Required lenses are `correctness`, `simplicity`, and `tests`; `security` is additional. Each reviewer must output exactly one `VERDICT: APPROVE` or `VERDICT: CHANGES REQUESTED` and a full `HEAD: <40-hex>` for the snapshot. Reviewers have neither GitHub credentials nor a mutable worktree.
4. `review_round_watcher.py` accepts only zero-exit, unambiguous, exact-head results and posts them as `ATTESTATION: MODEL_ADVISORY`. Any failed/missing/ambiguous result is `attention`; inspect that task result rather than trusting partial output.
5. Fix findings, push a new head, and seed a new round. Old-head verdicts cannot pass.
6. Run `python3 dept/approval_gate.py OWNER/REPO PR`. It passes only for every current-head lens in the latest complete round, matching configured green CI, and formal approval by `approval_gate.human_review_actors`.

Snapshots are under `~/.codex/dept/review-snapshots/`; durable round state is
under the department runtime `review_rounds/` directory. Raw findings are not
injected into a write-capable owner task.

## Rust review loop

The Rust loop uses `~/.zigzag/config.yaml` and can supersede the VM tooling.
With `ZIGZAG_REVIEW_LOOP_SHADOW=1`, it observes/discovers/adjudicates alongside
the VM jobs but suppresses reviewer dispatch, owner resume, and kill side
effects. Do not run both paths authoritatively: that produces duplicate
reviewers and comments. The loop persists state in
`~/.codex/zigzag/events.reviews.json` and rejects potentially truncated GitHub
comparisons at the 300-file response cap.
