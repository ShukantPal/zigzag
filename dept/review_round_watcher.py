#!/usr/bin/env python3
"""Watch review rounds: when every reviewer task for a PR has finished, resume
the owning worker session with the queued review comments + reviewer findings.

Runs from a platform cron (every 10 min) — stateless polling via SSH, no
long-lived process to die. This automates what was previously a manual step:
dispatch read-only review teams, wait for them, collect their last-message.txt
files, and resume the owning session once with everything batched.

Seeding a round: write <state-dir>/review_rounds/<pr>.json:
{
  "pr": 922,
  "repo": "leveled-inc/leveled",
  "owning_session": "01a0c0ec-5e74-7ef0-8108-c64384c7e5ab",
  "project_dir": "/Users/shukant/.codex/worktrees/stale-engine-race",
  "branch": "codex/audiorecorder-stale-engine-race",
  "reviewers": {"t-c6fb36": "concurrency", "t-3868f8": "simplicity", "t-4c19a0": "tests"},
  "queued_comments": ["AudioRecorder.swift:506 -- \"Explain when this would happen\"", "..."],
  "force_push_authorized": true,
  "created_at": "2026-09-20T22:30:00-07:00",
  "status": "collecting"
}
status: collecting -> dispatched (terminal) | attention (needs a human).

Rules honored:
- Never two workers on the same project_dir at once (ledger scan, fail closed).
- force-push on Codex-owned PR branches is standing pre-authorized
  (2026-09-20, Shukant's rule): the worker decides, using --force-with-lease.
  Set "force_push_authorized": false in the round file only for a PR Shukant
  opened himself or that belongs to someone else — then the worker commits
  locally and reports that a force-push is needed.
- A reviewer task dead with no output after ~2h of polls -> status "attention".
"""
import json
import fcntl
import os
import re
import subprocess
import sys
import time
from dept_config import ROOT, load_config, ssh_base, ssh_env, state_dir

CONFIG = load_config()
CONNECTION = CONFIG.get("connection", {})
STATE_DIR = state_dir(CONFIG)
ROUNDS_DIR = os.path.join(STATE_DIR, "review_rounds")
PROMPT_DIR = os.path.join(STATE_DIR, "task-prompts")
DEPT = os.path.join(ROOT, "dept.py")
LEDGER = os.path.join(STATE_DIR, "ledger.jsonl")
REMOTE_DEPT = CONNECTION.get("remote_dept", "~/.codex/dept")
LOCK_FILE = os.path.join(ROUNDS_DIR, "watcher.lock")

DRY_RUN = "--dry-run" in sys.argv
# ~2h of missed polls at 10-min cadence before flagging a dead reviewer task.
MISS_LIMIT = 12

SSH_BASE = ssh_base(CONNECTION)


def mac(cmd):
    p = subprocess.run(SSH_BASE + [cmd], capture_output=True, text=True,
                       env=ssh_env(), timeout=180)
    if p.returncode != 0:
        raise RuntimeError(f"mac cmd failed: {cmd[:80]} :: {p.stderr.strip()[:200]}")
    return p.stdout


def load_round(path):
    with open(path) as f:
        return json.load(f)


def save_round(path, rnd):
    with open(path, "w") as f:
        json.dump(rnd, f, indent=2)


def project_dir_busy(project_dir):
    """True if a dept task for this project_dir looks still running (fail closed)."""
    try:
        with open(LEDGER) as f:
            lines = f.readlines()
    except FileNotFoundError:
        return False
    cands = []
    for line in lines:
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("project", e.get("dir")) == project_dir and e.get("id"):
            cands.append(e["id"])
    for tid in reversed(list(dict.fromkeys(cands))):
        try:
            p = subprocess.run([sys.executable, DEPT, "status", tid],
                               capture_output=True, text=True, timeout=90)
            if "RUNNING" in (p.stdout + p.stderr):
                return True
        except Exception:
            return True  # fail closed: can't verify, don't dispatch
    return False


def reviewer_states(task_ids):
    """Use the dept ledger/relay status, then inspect completed task output."""
    completed = []
    states = {}
    for tid in task_ids:
        p = subprocess.run([sys.executable, DEPT, "status", tid],
                           capture_output=True, text=True, timeout=90)
        text = p.stdout + p.stderr
        if p.returncode != 0:
            raise RuntimeError(f"could not determine reviewer {tid} state: {text[:200]}")
        if "RUNNING" in text:
            states[tid] = "running"
        elif "DONE" in text:
            completed.append(tid)
        else:
            raise RuntimeError(f"unrecognized reviewer {tid} state: {text[:200]}")
    if not completed:
        return states
    script = "; ".join(
        f'd={tid}; f={REMOTE_DEPT}/{tid}/last-message.txt; '
        f'if [ -s "$f" ]; then echo "{tid} done"; else echo "{tid} dead-empty"; fi'
        for tid in completed
    )
    out = mac(script)
    for line in out.splitlines():
        parts = line.strip().split()
        if len(parts) == 2 and parts[0] in completed:
            states[parts[0]] = parts[1]
    return states


def fetch_findings(task_ids):
    """Cat every reviewer's last-message.txt in one SSH call, delimited.

    Delimiter is matched with a regex anchored on newlines: a findings file
    may not end with a trailing newline, which would glue the next delimiter
    onto its last line and break line-based parsing.
    """
    script = "; ".join(
        f'printf "\\n@@@{tid}@@@\\n"; cat {REMOTE_DEPT}/{tid}/last-message.txt'
        for tid in task_ids
    )
    out = mac(script)
    parts = re.split(r"\n@@@([A-Za-z0-9_-]+)@@@\n", out)
    # parts: [pre, tid1, body1, tid2, body2, ...]
    findings = {}
    for i in range(1, len(parts) - 1, 2):
        findings[parts[i]] = parts[i + 1].strip()
    return findings


PROMPT_TEMPLATE = """# PR #{pr} revision round — address queued review comments + review-team findings

Context: PR #{pr} ({repo}), branch `{branch}`, worktree `{project_dir}`.
You own this PR's revisions.

## Queued review comments (posted by Shukant on the PR — ADDRESS each, don't just report)
{comments}

For each: make the code/doc/test change that resolves it AND post a threaded reply
to that comment on the PR. Every threaded reply MUST start with the marker line
`> \U0001F916 Codex (AI assistant)` on its own line (replies post as ShukantPal via
shared gh auth; without the marker they read as his own words).

Staleness guard: before working, check the PR — if any queued comment above is
already addressed (replied to, or resolved in the code), skip it and say so.

## Review-team findings (address all of these too — verify each against the actual code before changing anything)
{findings}

## Push authorization
{push_rule}

## Done criteria
All of the above addressed, tests updated and passing, required CI green on the
latest head, PR updated, threaded replies posted to each queued comment with the
marker line. Then report the PR URL and a per-comment summary of what changed.
"""


def build_prompt(rnd, findings):
    comments = "\n".join(f"{i+1}. {c}" for i, c in enumerate(rnd.get("queued_comments", [])))
    ftext = "\n\n".join(
        f"### {label} ({tid})\n{findings.get(tid, '(no output captured)')}"
        for tid, label in rnd["reviewers"].items()
    )
    if rnd.get("force_push_authorized", True):
        push_rule = ("Force-push with `--force-with-lease` on this PR branch is standing "
                     "pre-authorized (Shukant's rule, 2026-09-20): decide yourself whether "
                     "the revision needs it. Re-verify the PR shows only this task's work "
                     "afterward.")
    else:
        push_rule = ("Do NOT force-push. If the revision requires rewriting pushed history, "
                     "commit locally and report that a force-push is needed so Shukant can "
                     "authorize it explicitly.")
    return PROMPT_TEMPLATE.format(pr=rnd["pr"], repo=rnd.get("repo", ""),
                                  branch=rnd.get("branch", ""),
                                  project_dir=rnd["project_dir"],
                                  comments=comments or "(none queued)", findings=ftext,
                                  push_rule=push_rule)


def process_round(path):
    rnd = load_round(path)
    if rnd.get("status") != "collecting":
        return f"#{rnd['pr']}: status={rnd.get('status')}, skipping"
    tids = list(rnd["reviewers"].keys())
    states = reviewer_states(tids)
    misses = rnd.setdefault("misses", {})

    dead = [t for t in tids if states.get(t) == "dead-empty"]
    for t in tids:
        if states.get(t) == "dead-empty":
            misses[t] = misses.get(t, 0) + 1
        else:
            misses[t] = 0
    worst = [t for t in dead if misses.get(t, 0) >= MISS_LIMIT]
    if worst:
        rnd["status"] = "attention"
        rnd["attention_reason"] = f"reviewer tasks dead with no output: {', '.join(worst)}"
        save_round(path, rnd)
        return f"#{rnd['pr']}: ATTENTION — {rnd['attention_reason']}"
    if any(states.get(t) != "done" for t in tids):
        save_round(path, rnd)
        pend = [f"{t}={states.get(t, '?')}" for t in tids]
        return f"#{rnd['pr']}: waiting ({', '.join(pend)})"

    # All reviewers done — collect findings and dispatch.
    if project_dir_busy(rnd["project_dir"]):
        return f"#{rnd['pr']}: all reviewers done but {rnd['project_dir']} busy, will retry"
    findings = fetch_findings(tids)
    prompt = build_prompt(rnd, findings)
    ts = time.strftime("%Y%m%d-%H%M%S")
    ppath = os.path.join(PROMPT_DIR, f"{rnd['pr']}-review-round-{ts}.md")
    os.makedirs(PROMPT_DIR, exist_ok=True)
    with open(ppath, "w") as f:
        f.write(prompt)

    if DRY_RUN:
        return (f"#{rnd['pr']}: DRY RUN — would resume session {rnd['owning_session'][:8]} "
                f"with prompt {ppath} ({len(prompt)} chars, {len(findings)} findings)")

    p = subprocess.run([sys.executable, DEPT, "resume", rnd["project_dir"],
                        rnd["owning_session"], ppath],
                       capture_output=True, text=True, timeout=300)
    out = (p.stdout + p.stderr).strip()
    task_id = None
    for line in out.splitlines():
        if line.startswith("resumed "):
            task_id = line.split()[1]
    if p.returncode != 0 or not task_id:
        # Fall back to a fresh task so the findings don't rot.
        p2 = subprocess.run([sys.executable, DEPT, "start", rnd["project_dir"], ppath],
                            capture_output=True, text=True, timeout=300)
        out2 = (p2.stdout + p2.stderr).strip()
        for line in out2.splitlines():
            if line.startswith("started "):
                task_id = line.split()[1]
        if p2.returncode != 0 or not task_id:
            rnd["status"] = "attention"
            rnd["attention_reason"] = f"resume+start both failed: {(out + out2)[:300]}"
            save_round(path, rnd)
            return f"#{rnd['pr']}: ATTENTION — dispatch failed"
        log_extra = " (fresh task; resume failed)"
    else:
        log_extra = ""
    rnd["status"] = "dispatched"
    rnd["dispatched_at"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    rnd["dispatched_task"] = task_id
    rnd["prompt_file"] = ppath
    save_round(path, rnd)
    return f"#{rnd['pr']}: resumed owning session as {task_id}{log_extra}"


def main():
    os.makedirs(ROUNDS_DIR, exist_ok=True)
    lock = open(LOCK_FILE, "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print("another review-round poll holds the lock; exiting")
        return
    files = sorted(f for f in os.listdir(ROUNDS_DIR) if f.endswith(".json"))
    if not files:
        print("no review rounds seeded")
        return
    for fn in files:
        path = os.path.join(ROUNDS_DIR, fn)
        try:
            print(process_round(path))
        except Exception as e:
            print(f"{fn}: ERROR {e}")


if __name__ == "__main__":
    main()
