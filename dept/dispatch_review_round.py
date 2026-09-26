#!/usr/bin/env python3
"""Seed a read-only review team and its follow-up revision round."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from dept_config import ROOT, load_config, state_dir

CONFIG = load_config()
STATE_DIR = state_dir(CONFIG)
ROUNDS_DIR = Path(STATE_DIR) / "review_rounds"
PROMPT_DIR = Path(STATE_DIR) / "task-prompts"
DEPT = os.path.join(ROOT, "dept.py")
LENSES = ("correctness", "simplicity", "tests")

FULL_TEMPLATE = """# Independent PR review — {lens}

Review PR #{pr} in `{repo}` at the exact head `{head}`. This is a read-only
review: do not modify files, commit, push, or merge.

## Design rationale — read this FIRST
{body}

## Instructions
1. In `{project_dir}`, inspect the current PR and diff with:
   `gh pr view {pr} --repo {repo}`
   `gh pr diff {pr} --repo {repo}`
2. Review the flat diff only. Do not request re-adding code deliberately
   removed or changed when the PR description explains why.
3. Use the {lens} lens. Report only concrete, actionable findings; do not
   invent style nits or findings outside the changed code.
4. Post one top-level PR comment exactly in this format:
   > 🤖 Codex (AI assistant) — [{lens}] review verdict
   VERDICT: APPROVE | CHANGES REQUESTED
   HEAD: {head}
   <2–5 line summary; findings when CHANGES REQUESTED>

Do not use `gh pr review --approve`; this must be a normal PR comment.
"""


def run(*args):
    return subprocess.run(args, capture_output=True, text=True, check=True)


def pr_info(pr, repo):
    """Read PR metadata with explicit repo scope, never checkout-relative gh."""
    result = run("gh", "pr", "view", str(pr), "--repo", repo,
                 "--json", "headRefOid,headRefName,body")
    return json.loads(result.stdout)


def round_files(pr):
    """Return non-superseded rounds; superseded rounds do not consume the cap."""
    rounds = []
    for path in ROUNDS_DIR.glob(f"{pr}-*.json"):
        try:
            data = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if str(data.get("status", "")).startswith("superseded"):
            continue
        rounds.append((path, data))
    return rounds


def next_round_number(pr):
    return max((int(data.get("round", 0)) for _, data in round_files(pr)), default=0) + 1


def task_id(output):
    for line in output.splitlines():
        if line.startswith("started "):
            return line.split()[1]
    raise RuntimeError(f"dept.py did not report a task id: {output[-500:]}")


def dispatch_reviewer(project_dir, prompt_file):
    result = run(sys.executable, DEPT, "start", project_dir, str(prompt_file), "--no-sop")
    return task_id(result.stdout + result.stderr)


def seed(pr, repo, project_dir, owning_session, branch, max_rounds=3):
    existing = round_files(pr)
    if len(existing) >= max_rounds:
        raise RuntimeError(f"PR #{pr} already has {len(existing)} active review rounds (cap {max_rounds})")
    info = pr_info(pr, repo)
    head = info["headRefOid"]
    number = next_round_number(pr)
    ROUNDS_DIR.mkdir(parents=True, exist_ok=True)
    PROMPT_DIR.mkdir(parents=True, exist_ok=True)
    reviewers = {}
    round_path = ROUNDS_DIR / f"{pr}-round{number}.json"
    round_data = {
        "pr": pr, "round": number, "repo": repo, "head": head, "branch": branch,
        "project_dir": project_dir, "owning_session": owning_session,
        "reviewers": reviewers, "queued_comments": [], "status": "dispatching",
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    # Persist before the first launch: a later failure must not orphan earlier
    # reviewer tasks or let a retry dispatch duplicates invisibly.
    round_path.write_text(json.dumps(round_data, indent=2))
    try:
      for lens in LENSES:
        prompt = FULL_TEMPLATE.format(pr=pr, repo=repo, project_dir=project_dir,
                                      lens=lens, head=head, body=info.get("body") or "(none)")
        prompt_file = PROMPT_DIR / f"pr{pr}-round{number}-{lens}.md"
        prompt_file.write_text(prompt)
        reviewers[dispatch_reviewer(project_dir, prompt_file)] = lens
        round_path.write_text(json.dumps(round_data, indent=2))
    except Exception as e:
        round_data.update({"status": "attention", "attention_reason": f"reviewer dispatch failed: {e}"})
        round_path.write_text(json.dumps(round_data, indent=2))
        raise
    round_data["status"] = "collecting"
    round_path.write_text(json.dumps(round_data, indent=2))
    return round_path, reviewers


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("pr", type=int)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--project-dir", required=True)
    parser.add_argument("--owning-session", required=True)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--max-rounds", type=int, default=3)
    args = parser.parse_args(argv)
    path, reviewers = seed(args.pr, args.repo, args.project_dir, args.owning_session,
                           args.branch, args.max_rounds)
    print(f"seeded {path}: " + ", ".join(f"{lens}={tid}" for tid, lens in reviewers.items()))


if __name__ == "__main__":
    main()
