#!/usr/bin/env python3
"""Watch active leveled PRs for new ShukantPal comments and dispatch a Codex task to address them.

Runs from a platform cron (reliable) — no long-lived Mac process to die. Polls via SSH+gh,
which are stateless, then dispatches through dept.py only when there is genuinely new feedback.

State lives in ~/workspace/goals/codex-engineering-department/hidden_files/pr-watch-active/watermark.json
as a set of already-seen comment IDs. First run seeds the watermark without dispatching.

Worker replies appear under ShukantPal too (shared gh auth), so every worker threaded
reply MUST start with the MARKER line below. The watcher skips marker-bearing comments
(the worker's own) and dispatches on his genuine comments — top-level AND threaded
replies. Merged/closed PRs are skipped automatically.
"""
import argparse
import fcntl
import json
import os
import re
import subprocess
import sys
import time

# Marker the worker must prefix on every threaded reply it posts. Without it the
# reply looks like Shukant's own words (shared gh auth). The watcher skips comments
# carrying this marker so worker replies never re-dispatch. Deliberately just the
# emoji: workers paraphrase the exact marker line (one already dropped the "> "),
# so detection must be tolerant. Also matches the "> 🤖 **AI review suggestion**"
# review comments — bot-posted suggestions for Shukant to triage, not his feedback.
MARKER = "🤖"

# PRs currently under Shukant's review. Edit this list as stacks merge / new ones open.
# (872-874, 902, 910 merged 2026-09-18; visit-attachments stack + SE4/ANDROID-Q/ANDROID-R added 2026-09-18.)
PRS = [856, 863, 864, 865, 866, 867, 868, 869]  # 916 merged 2026-09-22 (auto-skipped)
REPO = "leveled-inc/leveled"
PROJECT_DIR = "/Users/shukant/Workspace/leveled-inc/leveled"
HOME = os.path.expanduser("~")
STATE_DIR = os.path.join(HOME, "workspace/goals/codex-engineering-department/hidden_files/pr-watch-active")
WATERMARK = os.path.join(STATE_DIR, "watermark.json")
PROMPT_DIR = os.path.join(HOME, "workspace/codex-dept/prompts")
DEPT = os.path.join(HOME, "workspace/codex-dept/dept.py")
LEDGER = os.path.join(HOME, "workspace/codex-dept/ledger.jsonl")
SESSIONS_FILE = os.path.join(HOME, "workspace/codex-dept/pr_sessions.json")
BURST_FILE = os.path.join(STATE_DIR, "burst.json")
LOCK_FILE = os.path.join(STATE_DIR, "watcher.lock")
BURST_MINUTES = 30  # burst window; sliding-extended while his comments keep arriving


def burst_active():
    try:
        with open(BURST_FILE) as f:
            return time.time() < json.load(f).get("until", 0)
    except (FileNotFoundError, json.JSONDecodeError):
        return False


def burst_extend(minutes=BURST_MINUTES, reason=""):
    """Enter/extend burst mode. Sliding window: never shortens an active window."""
    until = time.time() + minutes * 60
    try:
        with open(BURST_FILE) as f:
            until = max(until, json.load(f).get("until", 0))
    except (FileNotFoundError, json.JSONDecodeError):
        pass
    os.makedirs(STATE_DIR, exist_ok=True)
    with open(BURST_FILE, "w") as f:
        json.dump({"until": until, "reason": reason, "set_at": time.time()}, f)
    return until


def burst_clear():
    try:
        os.remove(BURST_FILE)
    except FileNotFoundError:
        pass


def load_sessions():
    try:
        with open(SESSIONS_FILE) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


_watcher_notes = []


def log(msg):
    _watcher_notes.append(msg)


def dept_status_text(tid):
    try:
        p = subprocess.run([sys.executable, DEPT, "status", tid],
                           capture_output=True, text=True, timeout=60)
        return p.stdout + p.stderr
    except Exception:
        return ""


def save_sessions(sessions):
    os.makedirs(os.path.dirname(SESSIONS_FILE), exist_ok=True)
    with open(SESSIONS_FILE, "w") as f:
        json.dump(sessions, f, indent=2)


def pr_task_running(pr):
    """Task id if a dept task for this PR is genuinely still running (serialization:
    one worker per PR, no two agents on the same branch). Primary signal is the
    task id recorded in pr_sessions.json at dispatch time (deterministic); the
    ledger prompt_head scan is a fallback. The ledger's status field is
    write-once, so verify via `dept.py status` (relay truth)."""
    try:
        sess = load_sessions().get(str(pr), {})
        tid = sess.get("active_task")
        if tid:
            if "RUNNING" in dept_status_text(tid):
                return tid
    except Exception:
        pass
    needle = f"#{pr}"
    try:
        with open(LEDGER) as f:
            entries = []
            for line in f:
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if needle in e.get("prompt_head", ""):
                    entries.append(e)
    except FileNotFoundError:
        return None
    for e in entries:
        tid = e.get("id")
        try:
            if "RUNNING" in dept_status_text(tid):
                return tid
        except Exception:
            return tid  # fail closed: don't dispatch if we can't verify
    return None

SSH_BASE = [
    "ssh", "-i", os.path.join(HOME, ".ssh/id_ed25519"),
    "-o", "BatchMode=yes", "-o", "PasswordAuthentication=no",
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "UserKnownHostsFile=/home/hatch/.ssh/known_hosts",
    "-o", "ProxyCommand=python3 ~/workspace/tailscale/proxy_connect.py %h %p",
    "shukant@100.101.237.83",
]


def mac(cmd):
    env = dict(os.environ)
    hp = env.get("HTTPS_PROXY", "")
    env["TUNNEL_PROXY"] = hp.rsplit(":", 1)[0] + ":3130" if ":" in hp else ""
    p = subprocess.run(SSH_BASE + [cmd], capture_output=True, text=True, env=env, timeout=180)
    if p.returncode != 0:
        raise RuntimeError(f"mac cmd failed: {cmd[:80]} :: {p.stderr.strip()[:200]}")
    return p.stdout.strip()


def add_reaction(pr, c, content):
    """Add a GitHub reaction to a review or PR-conversation comment."""
    if c["path"] == "(pr conversation)":
        url = f"repos/{REPO}/issues/comments/{c['id']}/reactions"
    elif c["path"] == "(review body)":
        # Review bodies have NO REST reaction endpoint — only GraphQL addReaction.
        # Retry the node-id lookup once; the reviews index can lag a fresh submit.
        review_id = c["id"]
        node = None
        for _attempt in range(2):
            node = mac(f"gh api repos/{REPO}/pulls/{pr}/reviews/{review_id} "
                       f"--jq .node_id")
            if node:
                break
            time.sleep(3)
        if node:
            q = ("mutation($subjectId:ID!,$content:ReactionContent!){"
                 "addReaction(input:{subjectId:$subjectId,content:$content})"
                 "{reaction{content}}}")
            mac(f"gh api graphql -F subjectId={node} -F content={content} "
                f"-f 'query={q}'")
        return
    else:
        url = f"repos/{REPO}/pulls/comments/{c['id']}/reactions"
    mac(f"gh api -X POST {url} -f content={content}")


def load_watermark():
    if os.path.exists(WATERMARK):
        with open(WATERMARK) as f:
            wm = json.load(f)
        # Heal legacy raw-int entries (a 2026-09-18 manual seed stored bare comment
        # IDs). Both the mixed types AND the key mismatch caused a spurious re-dispatch
        # on 2026-09-19 and crashed sorted(seen) in save_watermark.
        seen = wm.get("seen", [])
        fixed = []
        for e in seen:
            if isinstance(e, int):
                fixed.append(f"rc:{e}")
                fixed.append(f"ic:{e}")
            else:
                fixed.append(e)
        wm["seen"] = fixed
        return wm
    return {"seen": [], "pr_state": {}}


def save_watermark(wm):
    os.makedirs(STATE_DIR, exist_ok=True)
    with open(WATERMARK, "w") as f:
        json.dump({**wm, "seen": sorted(set(wm.get("seen", [])), key=str)}, f, indent=2)


def pr_head_branch(n):
    out = mac(f"cd {PROJECT_DIR} && gh pr view {n} --json state,headRefName --jq '\"\\(.state) \\(.headRefName)\"'")
    state, _, branch = out.partition(" ")
    return state, branch


def review_comments(n):
    # Full comment objects as a JSON array. We need original_line,
    # original_commit_id and diff_hunk to anchor feedback to the code Shukant
    # actually saw: GitHub's re-tracked `line` drifts when a revision inserts
    # lines above, and once landed his comment on the wrong class (#908).
    out = mac(f"cd {PROJECT_DIR} && gh api repos/{REPO}/pulls/{n}/comments "
              f"--jq '[.[] | {{id, user: .user.login, in_reply: .in_reply_to_id, "
              f"path, line, original_line, original_commit_id, diff_hunk, "
              f"created_at, body: (.body // \"\")[0:500]}}]'")
    comments = []
    for c in json.loads(out or "[]"):
        in_reply = c.get("in_reply")
        body = (c.get("body") or "").replace("\n", " ")
        comments.append({"id": str(c["id"]), "user": c.get("user"),
                         "in_reply": None if in_reply in (None, "null", "") else str(in_reply),
                         "path": c.get("path"), "line": c.get("line"),
                         "original_line": c.get("original_line"),
                         "original_commit_id": c.get("original_commit_id"),
                         "diff_hunk": c.get("diff_hunk") or "",
                         "created": c.get("created_at"), "body": body,
                         "marked": MARKER in body})
    return comments


def issue_comments(n):
    out = mac(f"cd {PROJECT_DIR} && gh pr view {n} --json comments "
              f"--jq '.comments[] | \"\\(.id)|\\(.author.login)|\\(.createdAt)|\\(.body[0:300])\"'")
    comments = []
    for line in out.splitlines():
        parts = line.split("|", 3)
        if len(parts) != 4:
            continue
        cid, user, created, body = parts
        comments.append({"id": cid, "user": user, "path": "(pr conversation)", "line": "",
                         "created": created, "body": body.replace("\n", " "),
                         "marked": MARKER in body.replace("\n", " ")})
    return comments


def review_bodies(n):
    # Reviews submitted with a body but no inline comments (the GitHub mobile
    # app's comment flow lands here). Neither pulls/{n}/comments nor
    # `gh pr view --json comments` sees them — the reviews endpoint is the only
    # place they appear. Gap found 2026-09-22 on #916: his throwIfInterrupted /
    # throwIfCancelled helper suggestions were review bodies and got missed.
    out = mac(f"cd {PROJECT_DIR} && gh api repos/{REPO}/pulls/{n}/reviews "
              f"--paginate --jq '[.[] | {{id, user: .user.login, "
              f"submitted_at, body: (.body // \"\")[0:500]}}]'")
    bodies = []
    for r in json.loads(out or "[]"):
        body = (r.get("body") or "").replace("\n", " ")
        if not body.strip():
            continue
        bodies.append({"id": str(r["id"]), "user": r.get("user"),
                       "in_reply": None, "path": "(review body)", "line": None,
                       "original_line": None, "original_commit_id": None,
                       "diff_hunk": "", "created": r.get("submitted_at"),
                       "body": body, "marked": MARKER in body})
    return bodies


PROMPT_TMPL = """# PR follow-up (watcher-dispatched): address Shukant's review comments on leveled#{pr}

## Context
PR [#{pr}](https://github.com/leveled-inc/leveled/pull/{pr}) is under Shukant's review.
He left new review comments. ADDRESS THEM — his comments are explicit.

## The feedback (address each)
{comments}

## Anchoring — read before touching code
Each item above names the exact class/struct it is about and quotes the code
as Shukant saw it (commit SHA included). That line number is where his comment
sat WHEN HE WROTE IT — later revisions have moved the code. NEVER navigate to
that line in the current head; find the named symbol in the current file
instead. When in doubt which code he meant, `git show <sha>:<path>` shows the
file exactly as he saw it.

## Instructions
1. FIRST: add a 👀 (eyes) reaction to EACH review comment to acknowledge receipt:
   `gh api -X POST repos/leveled-inc/leveled/pulls/comments/<id>/reactions -f content=eyes`
   (For PR-conversation comments use `repos/leveled-inc/leveled/issues/comments/<id>/reactions`.)
   Some items are full review BODIES, not inline comments — they have NO REST reaction
   endpoint. For those, get the node id then use GraphQL:
   `gh api repos/leveled-inc/leveled/pulls/<pr>/reviews/<id> --jq .node_id`
   `gh api graphql -F subjectId=<node_id> -F content=EYES -f 'query=mutation($subjectId:ID!,$content:ReactionContent!){addReaction(input:{subjectId:$subjectId,content:$content}){reaction{content}}}'`
   (The watcher already attempts this when it dispatches, so eyes may already be on.)
   If a comment already carries a 🚀 (rocket) reaction from this bot account, it was queued
   behind earlier work — delete the rocket first (list reactions, find the rocket, DELETE
   `.../reactions/<reaction_id>`), then add 👀.
2. Where he asks for a code change, make it. Where he asks a question, answer it in a threaded reply.
   Keep each threaded reply to one or two sentences — he reviews on his phone.
   Name the class/struct you addressed in the first sentence, so he can verify
   at a glance that you hit the right target. Threaded replies:
   `gh api -X POST repos/leveled-inc/leveled/pulls/comments/<id>/replies -f body="..."`
   EVERY threaded reply MUST start with this exact marker line, then a blank line, then your text.
   `gh` posts under his account — without the marker your reply looks like his own words:
   > 🤖 Codex (AI assistant) — posted by Shukant's review bot under his account.
3. Some feedback items are follow-up replies in an existing thread (parent comment quoted below).
   Read the thread first and respond to what he's actually saying NOW — if he already resolved
   it himself, just confirm briefly; don't re-argue a settled point.
4. Keep changes scoped to this PR. Run the relevant tests.
5. Push to the existing branch `{branch}` (no new PRs, no rebase onto main).
6. Verify CI: `gh pr checks {pr}` — the real gate is semgrep/ci + Medical Scribe (BuildBuddy).
   `scribes-stg-preview-deploy` is expected to fail (manually triggered) — do NOT flag it as a blocker.
7. Report back: per-comment status, commits pushed, CI result. Do NOT merge — Shukant merges.

## Constraints
- This task owns PR #{pr} until the new comments are confirmed addressed (reacted + fixed + replied + CI green).
- Manage thermals so the Mac's fan doesn't kick on; slower is fine. No caffeinate.
"""


def comment_key(c):
    if c["path"] == "(pr conversation)":
        return f"ic:{c['id']}"
    if c["path"] == "(review body)":
        return f"rv:{c['id']}"
    return f"rc:{c['id']}"


DECL_RE = re.compile(r"^[\w\s]*\b(class|struct|enum|actor|protocol)\s+([A-Za-z_]\w*)")


def hunk_new_lines(diff_hunk):
    """Yield (new-file line number, text) for added/context lines of a diff hunk."""
    new_no = None
    for ln in diff_hunk.splitlines():
        m = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", ln)
        if m:
            new_no = int(m.group(1))
            continue
        if new_no is None or ln.startswith("\\"):
            continue
        if ln.startswith("-") and not ln.startswith("---"):
            continue
        text = ln[1:] if ln[:1] in ("+", " ") else ln
        yield new_no, text
        new_no += 1


def resolve_symbol(diff_hunk, original_line):
    """Nearest type declaration above the commented line, from the hunk he saw.

    Returns e.g. 'struct AudioRecordingLifecycle', or None when it can't tell.
    Symbol names don't drift when later revisions insert lines above.
    """
    if not diff_hunk or not original_line:
        return None
    lines = list(hunk_new_lines(diff_hunk))
    idx = next((i for i, (no, _) in enumerate(lines) if no >= original_line), len(lines) - 1)
    for _, text in reversed(lines[:idx + 1]):
        m = DECL_RE.match(text)
        if m:
            return f"{m.group(1)} {m.group(2)}"
    return None


def hunk_window(diff_hunk, original_line, radius=18, max_lines=40):
    """Trimmed view of the code as he saw it; >>> marks his commented line."""
    if not diff_hunk or not original_line:
        return ""
    lines = list(hunk_new_lines(diff_hunk))
    sel = [(no, t) for (no, t) in lines if abs(no - original_line) <= radius][:max_lines]
    return "\n".join(f"{'>>>' if no == original_line else '   '} {no:>5}  {t}" for no, t in sel)


def dispatch(pr, branch, new_comments, parent_bodies):
    lines = []
    for c in new_comments:
        symbol = resolve_symbol(c.get("diff_hunk"), c.get("original_line"))
        sha = (c.get("original_commit_id") or "")[:7]
        if c.get("original_line"):
            anchor = f"right-side line {c.get('original_line')} at commit {sha}"
            if c.get("line") and c.get("line") != c.get("original_line"):
                anchor += (f" — GitHub re-tracked it to line {c['line']} on the current head, "
                           f"which may be the WRONG code; ignore that line number")
        else:
            # Review-body comment: no line anchor; the body quotes the code itself.
            anchor = f"review submitted {c.get('created') or 'unknown time'}"
        about = f"about `{symbol}` " if symbol else ""
        entry = (f"- {c['path']} {about}(comment id `{c['id']}`), "
                 f"originally {anchor}:\n  \"{c['body']}\"")
        window = hunk_window(c.get("diff_hunk") or "", c.get("original_line") or 0)
        if window:
            entry += f"\n  Code as he saw it (>>> marks his commented line):\n```\n{window}\n```"
        if c.get("in_reply") and c["in_reply"] in parent_bodies:
            entry += f"\n  (follow-up reply in thread; parent comment said: \"{parent_bodies[c['in_reply']]}\")"
        lines.append(entry)
    prompt = PROMPT_TMPL.format(pr=pr, branch=branch, comments="\n".join(lines))
    sessions = load_sessions()
    sess = sessions.get(str(pr), {})
    session_id = sess.get("session_id")
    if session_id:
        prompt = ("NOTE: you are CONTINUING your existing session on this PR "
                  "(resumed, not fresh). You have full context from before — "
                  "re-read the current branch state before changing anything.\n\n"
                  ) + prompt
    ts = time.strftime("%Y%m%d-%H%M%S")
    ppath = os.path.join(PROMPT_DIR, f"pr{pr}-watch-feedback-{ts}.md")
    os.makedirs(PROMPT_DIR, exist_ok=True)
    with open(ppath, "w") as f:
        f.write(prompt)
    # Serialize: never run two workers on the same PR/branch at once.
    # Queued comments get a 🚀 (rocket) reaction so Shukant can see they're
    # lined up; the worker swaps it for 👀 when it starts on them.
    # (GitHub's reaction API has no hourglass — rocket is the closest "queued".)
    running = pr_task_running(pr)
    if running:
        for c in new_comments:
            try:
                add_reaction(pr, c, "rocket")
            except Exception as e:
                log(f"#{pr}: rocket reaction failed for {c['id']} ({e})")
        log(f"#{pr}: task {running} already working it — queued with 🚀, will retry")
        return None, f"skipped: {running} already running"
    if session_id:
        p = subprocess.run([sys.executable, DEPT, "resume", PROJECT_DIR,
                            session_id, ppath, "--no-sop"],
                           capture_output=True, text=True, timeout=300)
        out = (p.stdout + p.stderr).strip()
        if p.returncode != 0:
            log(f"#{pr}: resume of {session_id[:8]} failed ({out[:200]}), falling back to start")
            session_id = None
        else:
            log(f"#{pr}: resumed session {session_id[:8]}")
    if not session_id:
        p = subprocess.run([sys.executable, DEPT, "start", PROJECT_DIR, ppath, "--no-sop"],
                           capture_output=True, text=True, timeout=300)
        out = (p.stdout + p.stderr).strip()
        if p.returncode != 0:
            raise RuntimeError(f"dept.py start failed: {out[:500]}")
    task_id = None
    for line in out.splitlines():
        if line.startswith("started ") or line.startswith("resumed "):
            task_id = line.split()[1]
    if task_id:
        # Record the live worker so pr_task_running() can serialize on it
        # deterministically (ledger prompt_head matching is only a fallback).
        sessions = load_sessions()
        sess = sessions.setdefault(str(pr), {})
        sess["active_task"] = task_id
        save_sessions(sessions)
    return task_id, out


def main():
    # One poll at a time: the 30s burst cron and the 5m baseline cron overlap.
    os.makedirs(STATE_DIR, exist_ok=True)
    _lock = open(LOCK_FILE, "w")
    try:
        fcntl.flock(_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return  # another poll in flight; its results cover this tick
    wm = load_watermark()
    seen = set(wm.get("seen", []))
    first_run = not os.path.exists(WATERMARK)
    dispatches = []
    notes = []
    activity = False  # any of his fresh comments seen -> extend the burst window

    for pr in PRS:
        try:
            state, branch = pr_head_branch(pr)
        except RuntimeError as e:
            notes.append(f"#{pr}: state check failed ({e})")
            continue
        wm.setdefault("pr_state", {})[str(pr)] = state
        if state != "OPEN":
            notes.append(f"#{pr} is {state}; skipping")
            continue
        fresh = []
        all_comments = review_comments(pr) + issue_comments(pr) + review_bodies(pr)
        parent_bodies = {c["id"]: c["body"][:300] for c in all_comments}
        for c in all_comments:
            key = comment_key(c)
            if c["user"] != "ShukantPal" or key in seen:
                continue
            if c.get("marked"):
                seen.add(key)  # worker's own reply — never dispatch on it
                continue
            # Top-level comments AND his threaded replies both dispatch;
            # only marker-bearing (worker) replies are skipped.
            # NOTE: not added to `seen` here — only after a real dispatch,
            # so a skipped-due-to-running comment is retried on the next poll.
            fresh.append(c)
        if first_run:
            for c in fresh:
                seen.add(comment_key(c))
            continue  # seed only
        if fresh:
            activity = True
            try:
                task_id, msg = dispatch(pr, branch, fresh, parent_bodies)
            except Exception as e:
                notes.append(f"#{pr}: dispatch failed ({e})")
                continue
            if task_id is None:
                notes.append(f"#{pr}: {msg}")
                continue
            for c in fresh:
                seen.add(comment_key(c))
            dispatches.append({"pr": pr, "task": task_id, "n": len(fresh),
                               "branch": branch,
                               "comments": [{"id": c["id"], "path": c["path"], "line": c["line"],
                                             "body": c["body"][:120]} for c in fresh]})

    save_watermark({"seen": sorted(seen), "pr_state": wm.get("pr_state", {})})
    notes.extend(_watcher_notes)
    if activity and not first_run:
        # Sliding window: his comments keep arriving -> stay in 30s burst mode.
        # Quiet for BURST_MINUTES -> burst expires -> back to the 5m baseline.
        until = burst_extend(reason="his comments active")
        notes.append(f"burst extended to {time.strftime('%H:%M', time.localtime(until))}")

    if first_run:
        print(f"SEEDED watermark for PRs {PRS} (no dispatches on first run)")
    if dispatches:
        for d in dispatches:
            print(f"DISPATCHED task={d['task']} pr=#{d['pr']} new_comments={d['n']} branch={d['branch']}")
            for c in d["comments"]:
                print(f"  - {c['path']}:{c['line']} id={c['id']} :: {c['body']}")
    elif not first_run:
        print("NOTHING_NEW")
    for n in notes:
        print(f"NOTE {n}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--burst", nargs="?", const=BURST_MINUTES, type=int, metavar="MIN",
                    help="enter 30s burst mode for MIN minutes (default 30); "
                         "use when Shukant asks to watch closely")
    ap.add_argument("--burst-off", action="store_true", help="leave burst mode now")
    ap.add_argument("--burst-poll", action="store_true",
                    help="poll only while burst mode is active, else exit quietly "
                         "(for the 30s cron)")
    a = ap.parse_args()
    if a.burst_off:
        burst_clear()
        print("burst cleared")
    elif a.burst is not None:
        until = burst_extend(a.burst, reason="manual")
        print(f"burst until {time.strftime('%H:%M', time.localtime(until))}")
    else:
        if a.burst_poll and not burst_active():
            sys.exit(0)  # quiet tick: no SSH, no output
        main()
