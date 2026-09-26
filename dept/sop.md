# Codex Department — Standard Operating Procedure

This preamble is automatically prepended to every dispatched task prompt
(unless --no-sop is passed). It defines how every code task ends.

## SOP: work → PR → review → CI green

**Worktree rule (read first):** if your prompt names a worktree directory, do
ALL repo work there — never in the shared checkout at the project directory.
If no worktree is named and the project directory may be shared with other
agents, create your own first: fetch origin, then
`git worktree add -b codex/<short-desc> <path> origin/main` — the base MUST
be a freshly-fetched `origin/main`, never local `main` (often stale) and
never `git rev-parse HEAD` from a shared checkout (may sit on an unmerged
stack). Verify with `git merge-base --is-ancestor origin/main HEAD` before
you push. `<path>` MUST be outside the main repo checkout — use
`~/.codex/worktrees/<short-desc>/` or `/private/tmp/<short-desc>` — NEVER
create worktrees nested inside the shared checkout (no `.codex-worktrees/`
or `.codex/worktrees/` under it); nested worktrees pollute `git status` and
risk deletion by repo-level operations. When you commit, stage ONLY your own
files (`git add <paths>`, never `git add -A`), so your PR contains only your
task's changes.

1. **Do the requested work** in the project directory. Keep the diff focused;
   don't reformat unrelated files.
2. **Branch**: create a new branch named `codex/<short-description>`.
   Never commit directly to main/master.
3. **Commit** with a clear message describing the change.
4. **Push** the branch and **open a pull request** with `gh pr create`
   (use `--title` and `--body`; summarize what changed and why). Write the
   body with real newlines — never literal `\n` escape sequences.
   The title and summary must stand alone: describe what the PR does in the
   repo's present state, never as "Replace X" / "Supersedes X" when X never
   shipped. A reader who never saw prior designs must understand the PR fully;
   the design process gets at most one context line, never the title.
5. **Verify the PR renders correctly**: after opening, fetch the PR body
   (`gh pr view <number> --json body --jq .body`) and confirm there are no
   literal `\n` sequences and the markdown renders as intended. Fix with
   `gh pr edit` if broken. The human reads these on their phone — formatting
   must be clean.
5b. **Verify the PR contains ONLY your work** (do this before reporting):
   `gh pr view <number> --json commits,files`. Every commit must be your
   task's own; every file must be in your task's scope. If anything foreign
   is there (stale commits from a bad base, another task's files), stop:
   cherry-pick just your commits onto a fresh branch from `origin/main`,
   force-push to the same PR branch, and re-verify. Never report a PR
   you have not checked this way — checking the local branch is NOT enough.
6. **Review team — mandatory oversight** (Shukant's standing rule, 2026-09-20:
   no single Codex ships alone — one worker's blind spots are the team's catch).
   After your final code push with required CI green, dispatch a review team of
   3 INDEPENDENT Codex tasks via fresh `dept.py start` calls — never your own
   subagents, never yourself re-checking. Each reviewer gets: the PR number +
   repo, the branch, the exact head SHA to review, and ONE lens:
   - correctness: logic bugs, concurrency/races, error handling, API misuse
   - simplicity: over-engineering, dead code, consistency with repo patterns
   - tests: coverage gaps, edge cases, flaky/async patterns
   (+ a 4th, security, if the PR touches auth, credentials, network, crypto,
   or PII — then the team is 4.)
   Reviewers work read-only: they check out the PR head in their own worktree
   (or review the diff) and MUST NOT push. Each reviewer finishes by posting a
   top-level PR comment in exactly this format:
     > 🤖 Codex (AI assistant) — [<lens>] review verdict
     VERDICT: APPROVE | CHANGES REQUESTED
     HEAD: <full 40-char head sha reviewed>
     <2–5 line summary; findings when CHANGES REQUESTED>
   (Comments land as ShukantPal via shared gh auth — the marker line marks them
   as the review team's, and the comment watcher skips marker comments, so
   verdicts never re-dispatch. Do NOT use `gh pr review --approve`: formal
   approvals would count toward branch protection as ShukantPal.)
   Address every CHANGES REQUESTED finding, push, then re-dispatch the reviewers
   on the new head for a fresh verdict.
   **Approval gate**: the PR is review-ready ONLY when required CI is green on
   the latest head AND every required lens shows a latest verdict of APPROVE
   whose HEAD equals the PR's current head. Stale approvals (older head) do not
   count. The orchestrator verifies with
   `python3 dept/approval_gate.py <owner/repo> <pr>`
   before the PR is reported — do not claim review-ready until the gate passes.
7. **CI green**: watch the PR's checks (`gh pr checks --watch`) and fix
   failures, pushing until CI passes.
7b. **Address the human's PR feedback**: on every poll cycle, also check
   the PR for review comments (`gh pr view <n> --json reviews,comments`).
   When the human (GitHub user ShukantPal) requests changes — in a review,
   a comment, or via the orchestrator — address them: make the changes,
   push to the same PR branch, and re-watch CI. This includes PR
   description/body feedback (rewrite with `gh pr edit`). Report what you
   changed. Review comments from anyone else: report verbatim, do not
   auto-apply. When you pick up a feedback comment from ShukantPal,
   acknowledge it in the same cycle with an eyes reaction so he can see
   it was seen: `gh api -X POST
   repos/<owner>/<repo>/issues/comments/<comment-id>/reactions -f
   content=eyes` (get `<comment-id>` from `gh pr view <n> --json
   comments`). For line-level review comments the endpoint is
   `repos/<owner>/<repo>/pulls/comments/<comment-id>/reactions`
   (list them with `gh api repos/<owner>/<repo>/pulls/<n>/comments`).

   Hook bypass: Shukant granted Codex workers permission to bypass git hooks
   with --no-verify (2026-09-17, "for now" — temporary), e.g. when the
   pre-push gate fails on pre-existing issues unrelated to the task. Always
   note the bypass in the report and PR body so he can see the gate was
   skipped. This does NOT grant force-push on branches you do not own — never
   force-push anyone else's branch; if a push is rejected as non-fast-forward
   on a branch you don't own, report it. Force-pushing YOUR OWN PR branch
   is standing pre-authorized (2026-09-20, Shukant's standing rule): use
   --force-with-lease, re-verify the PR shows only your work, and report it.
8. **Report back**: PR URL, summary of changes, review findings addressed,
   and final CI status. The human reviews the PR — do not merge it yourself.

If any step is impossible (e.g. `gh` auth broken), complete everything up to
the blocker, then report exactly what is blocked and what is needed.
