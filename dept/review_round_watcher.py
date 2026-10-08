#!/usr/bin/env python3
"""Watch review rounds and publish validated verdicts from isolated reviewers.

Runs from a platform cron (every 10 min) — stateless polling via SSH, no
long-lived process to die. Reviewers run without GUI-keychain access and their
raw output is never sent to a write-capable owner task.

Seeding a round: write <state-dir>/review_rounds/<repo>-pr<pr>-round<n>.json:
{
  "pr": 922,
  "repo": "leveled-inc/leveled",
  "head": "0123456789abcdef0123456789abcdef01234567",
  "project_dir": "/Users/shukant/.codex/worktrees/stale-engine-race",
  "reviewers": {"t-c6fb36": "concurrency", "t-3868f8": "simplicity", "t-4c19a0": "tests"},
  "created_at": "2026-09-20T22:30:00-07:00",
  "status": "collecting"
}
status: collecting -> published (terminal) | attention (needs a human).

Rules honored:
- Only an explicit zero exit permits reviewer output to be published.
- Model output is constrained to one verdict and one exact reviewed head.
- A reviewer task dead with no output after ~2h of polls -> status "attention".
"""
import json
import fcntl
from contextlib import contextmanager
import os
import re
import shlex
import subprocess
import sys
import time
from dept_config import ROOT, load_config, ssh_base, ssh_env, state_dir

CONFIG = load_config()
CONNECTION = CONFIG.get("connection", {})
STATE_DIR = state_dir(CONFIG)
ROUNDS_DIR = os.path.join(STATE_DIR, "review_rounds")
DEPT = os.path.join(ROOT, "dept.py")
LEDGER = os.path.join(STATE_DIR, "ledger.jsonl")
REMOTE_DEPT = CONNECTION.get("remote_dept", "~/.codex/dept")
LOCK_FILE = os.path.join(ROUNDS_DIR, "watcher.lock")
# Shared with dispatchers: serializes the gap between checking a project and
# launching the next owning-session worker.
WATCHER_LOCK = os.path.join(state_dir(CONFIG), "worker-dispatch.lock")
SESSIONS_FILE = os.path.join(state_dir(CONFIG), "pr_sessions.json")

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


def session_key(repo, pr):
    """Avoid cross-repository collisions for same-number PRs."""
    return f"{repo}#{pr}"


def session_state(sessions, repo, pr):
    """Read scoped state while preserving legacy numeric-key sessions."""
    return {**sessions.get(str(pr), {}), **sessions.get(session_key(repo, pr), {})}


def set_active_task(repo, pr, task_id):
    try:
        with open(SESSIONS_FILE) as f:
            sessions = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        sessions = {}
    key = session_key(repo, pr)
    scoped = sessions.setdefault(key, {})
    for name, value in sessions.pop(str(pr), {}).items():
        scoped.setdefault(name, value)
    scoped["active_task"] = task_id
    os.makedirs(os.path.dirname(SESSIONS_FILE), exist_ok=True)
    with open(SESSIONS_FILE, "w") as f:
        json.dump(sessions, f, indent=2)


@contextmanager
def worker_dispatch_lock():
    """Serialize project-idle checks and owning-worker launches across watchers."""
    os.makedirs(os.path.dirname(WATCHER_LOCK), exist_ok=True)
    with open(WATCHER_LOCK, "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
        else:
            yield True


STATUS_RE = re.compile(r":\s*(RUNNING|DONE)(?:\s+\(exit\s+([^\)]+)\))?\s*$")


def task_status(text):
    """Parse status without treating an ambiguous completion as safe."""
    if text.strip().endswith(": DONE (pruned)"):
        return "pruned"
    match = STATUS_RE.search(text.strip())
    if not match:
        return "unknown"
    if match.group(1) == "RUNNING":
        return "running"
    exit_code = match.group(2)
    if exit_code == "0":
        return "succeeded"
    if exit_code and exit_code.isdigit():
        return "failed"
    return "unknown"


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
            text = p.stdout + p.stderr
            status = task_status(text) if p.returncode == 0 else "unknown"
            if status in ("running", "unknown"):
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
        status = task_status(text)
        if status == "running":
            states[tid] = "running"
        elif status == "succeeded":
            completed.append(tid)
        elif status in ("failed", "pruned"):
            states[tid] = "failed"
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


VERDICT_RE = re.compile(r"^VERDICT:\s*(APPROVE|CHANGES REQUESTED)\s*$", re.MULTILINE)
HEAD_RE = re.compile(r"^HEAD:\s*([0-9a-f]{40})\s*$", re.IGNORECASE | re.MULTILINE)


def validated_verdict(text, head):
    """Accept one unambiguous pair of constrained fields from advisory output."""
    verdicts = VERDICT_RE.findall(text)
    reviewed_heads = HEAD_RE.findall(text)
    if len(verdicts) != 1 or len(reviewed_heads) != 1:
        return None
    if reviewed_heads[0].lower() != head.lower():
        return None
    return verdicts[0].upper()


def verdict_body(lens, head, verdict, task_id):
    return (f"> 🤖 Codex (AI assistant) — [{lens}] review verdict\n\n"
            f"VERDICT: {verdict}\nHEAD: {head}\n\n"
            "ATTESTATION: MODEL_ADVISORY\n\n"
            f"Validated read-only reviewer task: {task_id}. An APPROVE verdict "
            "requires a formal review from an allowlisted human before it can "
            "satisfy the gate.")


def verdict_already_posted(repo, pr, body):
    output = mac(f"gh pr view {int(pr)} --repo {shlex.quote(repo)} --json comments")
    comments = json.loads(output).get("comments", [])
    return any((c.get("author") or {}).get("login") == "ShukantPal" and
               c.get("body") == body for c in comments)


def post_verdict(repo, pr, lens, head, verdict, task_id):
    body = verdict_body(lens, head, verdict, task_id)
    if verdict_already_posted(repo, pr, body):
        return False
    mac(f"gh pr comment {int(pr)} --repo {shlex.quote(repo)} --body {shlex.quote(body)}")
    return True


@contextmanager
def round_publish_lock(path):
    """Serialize duplicate polls of one round without blocking worker launches."""
    with open(f"{path}.lock", "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
        else:
            yield True


def process_round(path):
    with round_publish_lock(path) as acquired:
        if not acquired:
            return f"{os.path.basename(path)}: another poll is publishing this round"
        return _process_round_locked(path)


def _process_round_locked(path):
    # Reload only after acquiring the shared lock: another watcher may have
    # completed this round while this process was waiting to run.
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
    failed = [t for t in tids if states.get(t) == "failed"]
    if failed:
        rnd["status"] = "attention"
        rnd["attention_reason"] = f"reviewer tasks exited unsuccessfully: {', '.join(failed)}"
        save_round(path, rnd)
        return f"#{rnd['pr']}: ATTENTION — {rnd['attention_reason']}"
    if any(states.get(t) != "done" for t in tids):
        save_round(path, rnd)
        pend = [f"{t}={states.get(t, '?')}" for t in tids]
        return f"#{rnd['pr']}: waiting ({', '.join(pend)})"

    # All reviewers done — publish only validated verdict fields. Never pass
    # raw model output to the privileged owner session: reviewer output may be
    # adversarial even when the review sandbox itself is read-only.
    findings = fetch_findings(tids)
    verdicts = {}
    for tid, lens in rnd["reviewers"].items():
        verdict = validated_verdict(findings.get(tid, ""), rnd["head"])
        if not verdict:
            rnd["status"] = "attention"
            rnd["attention_reason"] = f"reviewer {tid} did not return a valid verdict for {rnd['head']}"
            save_round(path, rnd)
            return f"#{rnd['pr']}: ATTENTION — {rnd['attention_reason']}"
        verdicts[lens] = (verdict, tid)
    if DRY_RUN:
        return f"#{rnd['pr']}: DRY RUN — would publish {len(verdicts)} validated verdicts"
    posted = set(rnd.get("posted_lenses", []))
    for lens, (verdict, tid) in verdicts.items():
        if lens not in posted:
            post_verdict(rnd["repo"], rnd["pr"], lens, rnd["head"], verdict, tid)
            posted.add(lens)
            rnd["posted_lenses"] = sorted(posted)
            save_round(path, rnd)
    rnd["status"] = "published"
    rnd["published_at"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    save_round(path, rnd)
    return f"#{rnd['pr']}: published validated review verdicts; owner must inspect PR findings manually"


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
