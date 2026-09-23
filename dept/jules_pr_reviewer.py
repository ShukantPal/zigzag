#!/usr/bin/env python3
"""Poll leveled for newly opened Jules PRs (branches jules/*) and dispatch a
Codex review task for each one that hasn't been reviewed yet.

State: ~/workspace/goals/codex-engineering-department/hidden_files/jules-pr-reviews.json
Review tasks post their findings as a PR comment (with the bot marker) and never merge.
Run from cron every ~10 min; disable the cron once all expected PRs are reviewed.
"""
import json
import os
import subprocess
import sys

HOME = os.path.expanduser("~")
STATE_FILE = os.path.join(
    HOME, "workspace/goals/codex-engineering-department/hidden_files/jules-pr-reviews.json")
PROMPT_DIR = os.path.join(HOME, "workspace/codex-dept/prompts")
DEPT = os.path.join(HOME, "workspace/codex-dept/dept.py")
PROJECT = "/Users/shukant/Workspace/leveled-inc/leveled"
REPO = "leveled-inc/leveled"
SSH = [
    "ssh", "-i", os.path.join(HOME, ".ssh/id_ed25519"),
    "-o", "BatchMode=yes", "-o", "PasswordAuthentication=no",
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "UserKnownHostsFile=/home/hatch/.ssh/known_hosts",
    "-o", "ProxyCommand=python3 ~/workspace/tailscale/proxy_connect.py %h %p",
    "shukant@100.101.237.83",
]
ENV = dict(os.environ, TUNNEL_PROXY=os.environ["HTTPS_PROXY"].rsplit(":", 1)[0] + ":3130")

# Jules session ids for the J8-J13 batch (from the Delegation sheet). Jules names
# each PR branch with the session id as suffix, e.g.
# fix-audiorecorder-interruption-6632355049745658931
JULES_SESSIONS = {
    "6632355049745658931": "APPLE-IOS-22W",
    "1844131579710009734": "APPLE-IOS-1BX",
    "13298046696359534919": "APPLE-IOS-21Z",
    "8060398488853582017": "APPLE-IOS-1CN",
    "4798968791252688636": "APPLE-IOS-22R",
    "1126462001172428634": "SCRIBES-SERVER-9N",
}

REVIEW_PROMPT = """# Review Jules PR #{pr} and post findings as a PR comment

You are reviewing a pull request authored by Jules (an AI coding agent), not by a human.
Repo checkout (READ-ONLY): {project}
PR: https://github.com/{repo}/pull/{pr}

Steps:
1. Read the PR: `gh pr view {pr} --json title,body,headRefName,baseRefName,comments` and the full diff with `gh pr diff {pr}`. Also check CI: `gh pr checks {pr}`.
2. Review for:
   - Correctness: does the change do what the PR body / linked Sentry issue asks? Logic bugs, missed edge cases, possible regressions?
   - Tests: are new/changed tests meaningful (not tautological, actually exercise the fix)? If the touched area has a fast test command, run it.
   - Style/consistency: matches surrounding code conventions; no dead code, debug leftovers, or unrelated changes.
3. Build & warnings validation (do this, don't skip it):
   - Run Bazel builds of the affected iOS targets on this Mac (e.g. `bazel build //scribe/clients/xplat/apple:apple` and `bazel test` on the touched test targets). The Xcode pin is fixed and local Bazel Apple builds work — this is the source of truth, not CI logs.
   - The checkout at {project} is READ-ONLY for source edits — if Bazel needs to write into the tree, build in a scratch copy/worktree under /tmp or ~/.codex/dept scratch space with the PR branch checked out (`gh pr checkout {pr}` into the scratch space), or apply the diff. Never modify {project} or push anything.
   - Report build success/failure with the exact command run. If the build fails, include the first errors.
   - Check for NEW compiler warnings attributable to the PR: build the base commit too if practical and diff the warnings in the touched files; otherwise list warnings in the touched files and flag any the PR plausibly introduced (new code, not pre-existing). The repo has a first-party Apple Swift warning gate (#756) — a new warning is a real finding.
   - Keep it light: affected targets only, no full-monorepo builds, modest parallelism (`--jobs=4`) — the machine's fan must stay off.
4. Post your findings as a PR comment: `gh pr comment {pr} --body "..."`.
   - The comment MUST start with this exact line: `> 🤖 Codex (AI assistant)`
   - Then a verdict line (LGTM / Needs changes), then specific findings with file:line references.
   - Include a short "Build & warnings" section: the build command, result, and any new warnings (or "no new warnings").
   - If the PR is clean, still post a brief comment summarizing what you verified (diff scope, tests run, CI state, build result).
   - The comment MUST start with this exact line: `> 🤖 Codex (AI assistant)`
   - Then a verdict line (LGTM / Needs changes), then specific findings with file:line references.
   - If the PR is clean, still post a brief comment summarizing what you verified (diff scope, tests run, CI state).
4. HARD RULES: never merge, never push to the branch, never approve via `gh pr review --approve`. Comment only.

Report back the comment URL when done.
"""


REREVIEW_PROMPT = """# Re-review Jules PR #{pr} (new commits pushed) and post findings as a PR comment

You are reviewing a pull request authored by Jules (an AI coding agent), not by a human.
Repo checkout (READ-ONLY): {project}
PR: https://github.com/{repo}/pull/{pr}

Jules has pushed new commits since the last Codex review — this is a RE-REVIEW, not a fresh one.

Steps:
1. Read the PR: `gh pr view {pr} --json title,body,headRefName,baseRefName` and the full diff with `gh pr diff {pr}`. Also check CI: `gh pr checks {pr}`.
2. Read the previous review thread: `gh api repos/{repo}/issues/{pr}/comments` and `gh api repos/{repo}/pulls/{pr}/comments`. Find the earlier Codex review comments (they start with `> 🤖 Codex (AI assistant)`), Jules' replies, and the @jules follow-ups. For each finding in the earlier reviews, determine from the new diff whether it is actually resolved — do not take Jules' claims at face value.
3. Review the NEW changes for:
   - Correctness: does the change do what the PR body / linked Sentry issue asks? Logic bugs, missed edge cases, possible regressions?
   - Tests: are new/changed tests meaningful (not tautological, actually exercise the fix)? If the touched area has a fast test command, run it.
   - Style/consistency: matches surrounding code conventions; no dead code, debug leftovers, or unrelated changes.
4. Build & warnings validation (do this, don't skip it): same as a normal review — Bazel builds of the affected iOS targets on this Mac, report the exact command and result, check for NEW compiler warnings attributable to the PR (the repo has a first-party Apple Swift warning gate #756 — a new warning is a real finding). Build in a scratch copy/worktree under /tmp or ~/.codex/dept scratch space; the checkout at {project} is READ-ONLY — never modify it or push anything. Keep it light: affected targets only, `--jobs=4` — the machine's fan must stay off.
5. Post your findings as a PR comment: `gh pr comment {pr} --body "..."`.
   - The comment MUST start with this exact line: `> 🤖 Codex (AI assistant)`
   - Then a verdict line (LGTM / Needs changes), then specific findings with file:line references. Lead with the disposition of each earlier finding (resolved / partially addressed / not addressed).
   - Include a short "Build & warnings" section: the build command, result, and any new warnings (or "no new warnings").
   - If the PR is clean, still post a brief comment summarizing what you verified (diff scope, tests run, CI state, build result). LGTM comments need no @jules mention.
   - If it still needs changes, add a follow-up comment with the `> 🤖 Codex (AI assistant)` marker line that @jules-mentions Jules with a concise summary of the remaining findings and a pointer to the review comment.
6. HARD RULES: never merge, never push to the branch, never approve via `gh pr review --approve`. Comment only.

Report back the comment URL when done.
"""


def load_state():
    try:
        with open(STATE_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save_state(state):
    os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
    with open(STATE_FILE, "w") as f:
        json.dump(state, f, indent=2)


def open_jules_prs():
    out = subprocess.run(
        SSH + [f"cd {PROJECT} && gh pr list --state open --limit 50 "
               "--json number,headRefName,headRefOid,title"],
        capture_output=True, text=True, env=ENV, timeout=120)
    if out.returncode != 0:
        print(f"gh pr list failed: {out.stderr[-300:]}", file=sys.stderr)
        return []
    try:
        prs = json.loads(out.stdout or "[]")
    except json.JSONDecodeError:
        return []
    found = []
    for pr in prs:
        branch = pr.get("headRefName", "")
        for sid, issue in JULES_SESSIONS.items():
            if sid in branch:
                found.append({"n": pr["number"], "b": branch,
                              "o": pr.get("headRefOid", ""),
                              "t": pr.get("title", ""), "issue": issue})
                break
    return found


def task_status(task_id):
    out = subprocess.run([sys.executable, DEPT, "status", task_id],
                         capture_output=True, text=True, timeout=60)
    txt = (out.stdout + out.stderr).strip()
    if "DONE" in txt:
        return "done"
    if "RUNNING" in txt or "running" in txt:
        return "running"
    return "unknown"


def dispatch(prompt_template, pr, extra):
    prompt_path = os.path.join(PROMPT_DIR, f"jules-review-{pr['n']}.md")
    with open(prompt_path, "w") as f:
        f.write(prompt_template.format(pr=pr["n"], project=PROJECT, repo=REPO))
    out = subprocess.run(
        [sys.executable, DEPT, "start", PROJECT, prompt_path, "--no-sop"],
        capture_output=True, text=True, timeout=180)
    line = (out.stdout + out.stderr).strip().splitlines()
    task_id = line[-1].split()[1] if line and line[-1].startswith("started ") else "?"
    print(f"dispatched review for PR #{pr['n']} ({pr['b']}){extra}: {task_id}")
    return task_id


def main():
    state = load_state()
    changed = False
    for pr in open_jules_prs():
        key = str(pr["n"])
        entry = state.get(key)
        if entry and entry.get("status") in ("done", "running"):
            if entry["status"] == "running" and task_status(entry["task"]) == "done":
                entry["status"] = "done"
                changed = True
            # Revision watch: a reviewed PR whose branch moved since the last
            # review gets a Codex re-review. One task per PR at a time.
            if (entry["status"] == "done" and pr["o"]
                    and entry.get("head") and entry["head"] != pr["o"]):
                task_id = dispatch(REREVIEW_PROMPT, pr,
                                   f" (re-review of head {pr['o'][:8]})")
                entry.update({"task": task_id, "branch": pr["b"],
                              "title": pr["t"], "head": pr["o"],
                              "status": "running",
                              "reviews": entry.get("reviews", 1) + 1})
                changed = True
            continue
        task_id = dispatch(REVIEW_PROMPT, pr, "")
        state[key] = {"task": task_id, "branch": pr["b"], "title": pr["t"],
                      "head": pr["o"], "status": "running", "reviews": 1}
        changed = True
    if changed:
        save_state(state)


if __name__ == "__main__":
    main()
