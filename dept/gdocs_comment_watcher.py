#!/usr/bin/env python3
"""Watch registered Google Docs for new Shukant comments, ack with 👀, dispatch a worker.

Mirrors pr_comment_watcher.py's playbook for design docs. Runs from a platform
cron (every 5 min) — stateless, no long-lived process.

The Drive API has no emoji reactions on comments, so the acknowledgment is a
marked reply: the watcher posts "👀 Muse (AI assistant) — picked up ..." the
moment it sees his comment (fast ack, like 🚀 on GitHub), then resumes the
doc's owning worker session to ADDRESS the feedback.

ATTRIBUTION: replies post as Shukant (shared Google auth), so every
watcher/worker reply MUST start with the MARKER. The watcher skips comments
already carrying a marker-bearing reply, so worker replies never re-dispatch.

TOOLING SPLIT: the Mac has no Google Workspace CLI. Workers (Mac) do doc
source + docx rebuild + code changes and report back; the main agent (VM) does
all Docs API operations (uploads, reply updates, resolves). Dispatched prompts
must tell the worker to put substantive reply texts + docx path in its final
message in the parsed FORMAT below.

State: <state-dir>/gdocs-watch/watermark.json
  {"seen": [<comment-id>...], "dispatched": {<comment-id>: <watcher-reply-id>}}
First run seeds the watermark without dispatching.
"""
import argparse
import fcntl
import json
import os
import subprocess
import sys
import time
from dept_config import ROOT, load_config, state_dir

# Marker every watcher/worker Docs reply must start with. Detection is
# substring-based and tolerant (workers paraphrase).
MARKER = "Muse (AI assistant)"
EYES = "\U0001F440"

CONFIG = load_config()
WATCHER = CONFIG.get("gdocs_comment_watcher", {})
DOCS = WATCHER.get("docs", {})
GWS = WATCHER.get("gws", "")
STATE_DIR = os.path.join(state_dir(CONFIG), "gdocs-watch")
WATERMARK = os.path.join(STATE_DIR, "watermark.json")
PROMPT_DIR = os.path.join(state_dir(CONFIG), "prompts")
DEPT = os.path.join(ROOT, "dept.py")
LOCK_FILE = os.path.join(STATE_DIR, "watcher.lock")
HIS_NAME = "Shukant Pal"

ACK_TEXT = (
    f"{EYES} {MARKER} \u2014 picked up, addressing it now."
)

PROMPT_TEMPLATE = """# Design-doc feedback — address Shukant's new Google Doc comments

Shukant left {n} new comment(s) on the "{title}" Google Doc
(id `{doc_id}`). I have already posted \U0001F440 acknowledgment replies on
each (reply ids below). Your job: ADDRESS every comment — update the doc
source, rebuild the docx with your established flow, make any matching code
changes, and report back. Do not just report; implement.

IMPORTANT TOOLING NOTE: the Mac has no Google Workspace CLI. You CANNOT touch
the Google Doc, its comments, or replies yourself. I (main agent, on the VM)
do all Docs API operations (docx upload, reply updates, resolves). Your
deliverables:
  (a) the rebuilt .docx saved in your task dir, and
  (b) substantive reply texts in your final message, clearly delimited
      (see FORMAT below).
I will upload the docx, verify formatting, update your \U0001F440 replies in
place, and resolve the threads.

## Comments
{comments}

## Doc rebuild
Update the markdown source, rebuild the .docx with the same flow used before
(tables as plain text pre-upload — Drive upload drops docx tables), save it in
your task dir, and report its exact path. Body copy 11pt Proxima Nova, no
negative statements ("there is no X" — describe the mechanism), Mermaid
rendered to images for surviving diagrams.

## Code push rules
If a comment changes the design, make the matching code changes in the owning
stack, keeping each PR to its layer. Push with the git hook intact
(no --no-verify); semgrep + Medical Scribe BuildBuddy green on each touched
PR; verify each PR shows only its own work via `gh pr view` (commits+files).
No merges. One heavy build at a time; thermals managed, fan stays off.
Name your model tier in the report.

## Final message FORMAT (parsed to do the Docs updates)
DOCX_PATH: /Users/shukant/.codex/dept/<task-id>/<name>.docx
{reply_lines}
Resolve any thread whose comment is fully addressed with a `RESOLVE <comment-id>` line.
Reply texts: start each with "\U0001F440 Muse (AI assistant) \u2014 ", state what
changed and where (doc section + PR numbers), keep each under ~120 words.
"""


def gws(*args):
    r = subprocess.run([GWS] + list(args), capture_output=True, text=True, timeout=60)
    try:
        obj, _ = json.JSONDecoder().raw_decode(r.stdout)
    except Exception:
        raise RuntimeError(f"gws failed: {r.stdout[:500]} {r.stderr[:300]}")
    if isinstance(obj, dict) and "error" in obj:
        raise RuntimeError(f"gws error: {json.dumps(obj['error'])[:400]}")
    return obj


def load_watermark():
    try:
        with open(WATERMARK) as f:
            d = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        d = {}
    seen = d.get("seen", [])
    # heal legacy bare ids (same lesson as the PR watcher: always namespaced)
    norm = set()
    for i in seen:
        s = str(i)
        norm.add(s if s.startswith("gc:") else f"gc:{s}")
    return {"seen": norm, "dispatched": d.get("dispatched", {})}


def save_watermark(wm):
    os.makedirs(STATE_DIR, exist_ok=True)
    with open(WATERMARK, "w") as f:
        json.dump({"seen": sorted(wm["seen"]), "dispatched": wm["dispatched"]}, f, indent=2)


def list_comments(doc_id):
    return gws(
        "drive", "comments", "list", "--params", json.dumps({
            "fileId": doc_id,
            "fields": "comments(id,content,quotedFileContent(value),author(displayName),createdTime,resolved,"
                      "replies(id,content,author(displayName),createdTime))",
            "pageSize": 100,
        }),
    ).get("comments", [])


def post_eyes_reply(doc_id, comment_id):
    r = gws(
        "drive", "replies", "create", "--params", json.dumps({
            "fileId": doc_id, "commentId": comment_id, "fields": "id",
        }), "--json", json.dumps({"content": ACK_TEXT}),
    )
    return r.get("id")


def has_marker_reply(comment):
    for rep in comment.get("replies", []) or []:
        if MARKER in (rep.get("content") or ""):
            return True
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", action="store_true",
                    help="Seed watermark from current comments without dispatching.")
    args = ap.parse_args()

    os.makedirs(STATE_DIR, exist_ok=True)
    lock = open(LOCK_FILE, "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        print("another watcher holds the lock; exiting")
        return 0

    wm = load_watermark()
    if args.seed:
        for doc_id in DOCS:
            for c in list_comments(doc_id):
                wm["seen"].add("gc:" + c["id"])
                for rep in c.get("replies", []) or []:
                    wm["seen"].add("gc:" + rep["id"])
        save_watermark(wm)
        print(f"seeded {len(wm['seen'])} ids")
        return 0

    for doc_id, meta in DOCS.items():
        try:
            comments = list_comments(doc_id)
        except Exception as e:
            print(f"{doc_id}: list failed: {e}", file=sys.stderr)
            continue
        fresh = []
        for c in comments:
            key = "gc:" + c["id"]
            if key in wm["seen"]:
                continue
            if c.get("resolved"):
                wm["seen"].add(key)
                continue
            if (c.get("author") or {}).get("displayName") != HIS_NAME:
                wm["seen"].add(key)  # not his — never dispatch
                continue
            if has_marker_reply(c):
                wm["seen"].add(key)  # already acked/handled
                continue
            fresh.append(c)

        if not fresh:
            save_watermark(wm)
            continue

        # Post 👀 acks first (fast visible ack), then dispatch once for the batch.
        acked = []
        for c in fresh:
            try:
                rid = post_eyes_reply(doc_id, c["id"])
            except Exception as e:
                print(f"{c['id']}: eyes reply failed: {e}", file=sys.stderr)
                continue
            acked.append((c, rid))
            wm["seen"].add("gc:" + c["id"])
            wm["seen"].add("gc:" + rid)

        if not acked:
            save_watermark(wm)
            continue

        comments_txt = ""
        reply_lines = ""
        for c, rid in acked:
            anchor = ((c.get("quotedFileContent") or {}).get("value") or "")[:200].replace("\n", " ")
            comments_txt += (
                f"\n### id `{c['id']}`, 👀 reply id `{rid}` "
                f"({c.get('createdTime')})\n"
                f"Anchor: {anchor}\n"
                f"Text: {c.get('content')}\n"
            )
            reply_lines += f"REPLY {rid}: <substantive marked reply>\n"

        os.makedirs(PROMPT_DIR, exist_ok=True)
        stamp = time.strftime("%Y%m%d-%H%M%S")
        pfile = os.path.join(PROMPT_DIR, f"gdocs-{doc_id[:8]}-{stamp}.md")
        with open(pfile, "w") as f:
            f.write(PROMPT_TEMPLATE.format(
                n=len(acked), title=meta["title"], doc_id=doc_id,
                comments=comments_txt, reply_lines=reply_lines.rstrip(),
            ))

        r = subprocess.run(
            [DEPT, "resume", meta["project"], meta["session"], pfile],
            capture_output=True, text=True, timeout=180,
        )
        m = {}
        for line in r.stdout.splitlines():
            if line.startswith("resumed "):
                # "resumed t-XXXX proc=... session=... project=... (via relay)"
                m["task"] = line.split()[1]
        if r.returncode == 0 and m.get("task"):
            for c, rid in acked:
                wm["dispatched"]["gc:" + c["id"]] = rid
            print(f"{meta['title']}: dispatched {m['task']} for {len(acked)} comment(s)")
        else:
            # 👀 acks are up but dispatch failed: leave out of `dispatched` so
            # the next poll retries the resume (acks already exist, no double 👀).
            print(f"{meta['title']}: DISPATCH FAILED: {r.stdout[:300]} {r.stderr[:300]}",
                  file=sys.stderr)
        save_watermark(wm)
    return 0


if __name__ == "__main__":
    sys.exit(main())
