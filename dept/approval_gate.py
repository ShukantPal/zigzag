#!/usr/bin/env python3
"""Approval-gate check for Codex PRs (oversight system, 2026-09-20).

A PR passes the gate only when BOTH hold on the CURRENT head:
  1. Required CI is green: semgrep + BuildBuddy successful. The
     `scribes-stg-preview-deploy` manual trigger (ACTION_REQUIRED/NEUTRAL) and
     gcbrun noise are not blockers (Shukant's leveled CI rule).
  2. Every required review lens has a latest verdict comment of APPROVE whose
     HEAD equals the PR's current head SHA. Stale approvals (older head) and
     CHANGES REQUESTED verdicts fail the gate.

Verdict comments are top-level PR comments posted by review-team workers
(land as ShukantPal via shared gh auth, so they carry the marker line):

  > \U0001F916 Codex (AI assistant) \u2014 [correctness] review verdict
  VERDICT: APPROVE
  HEAD: <full 40-hex sha reviewed>
  <short summary; findings when CHANGES REQUESTED>

Lenses: correctness, simplicity, tests (+ security when the round seeds it).
The PR comment watcher skips marker-bearing comments, so verdicts never
re-dispatch. Verdicts are NOT `gh pr review --approve` reviews on purpose:
formal approvals would count toward branch protection as ShukantPal.

Usage: approval_gate.py <owner/repo> <pr> [--lenses correctness,simplicity,tests]
Exit 0 with {"pass": true, ...}; exit 1 with {"pass": false, "reasons": [...]}.
Runs gh over SSH on Shukant's Mac (same transport as the other watchers).
"""
import json
import os
import re
import subprocess
import sys

HOME = os.path.expanduser("~")

SSH_BASE = [
    "ssh", "-i", os.path.join(HOME, ".ssh/id_ed25519"),
    "-o", "BatchMode=yes", "-o", "PasswordAuthentication=no",
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "UserKnownHostsFile=/home/hatch/.ssh/known_hosts",
    "-o", "ProxyCommand=python3 ~/workspace/tailscale/proxy_connect.py %h %p",
    "shukant@100.101.237.83",
]

# Checks that must be green for the gate. Everything else failing is a warning.
BLOCKING_CHECKS = re.compile(r"semgrep|buildbuddy", re.IGNORECASE)
# Known noise: never a blocker.
IGNORED_CHECKS = re.compile(r"scribes-stg-preview-deploy", re.IGNORECASE)

MARKER = "\U0001F916"  # 🤖 — what the comment watcher keys its skip on
VERDICT_RE = re.compile(r"^VERDICT:\s*(APPROVE|CHANGES REQUESTED)\s*$",
                        re.IGNORECASE | re.MULTILINE)
HEAD_RE = re.compile(r"^HEAD:\s*([0-9a-f]{40})\s*$", re.IGNORECASE | re.MULTILINE)
LENS_RE = re.compile(r"\[([a-z]+)\]\s+review verdict", re.IGNORECASE)


def mac(cmd):
    env = dict(os.environ)
    hp = env.get("HTTPS_PROXY", "")
    env["TUNNEL_PROXY"] = hp.rsplit(":", 1)[0] + ":3130" if ":" in hp else ""
    p = subprocess.run(SSH_BASE + [cmd], capture_output=True, text=True,
                       env=env, timeout=180)
    if p.returncode != 0:
        raise RuntimeError(f"mac cmd failed: {cmd[:100]} :: {p.stderr.strip()[:200]}")
    return p.stdout


def fetch_pr(repo, pr):
    out = mac(
        f"gh pr view {pr} --repo {repo} "
        "--json headRefOid,comments,statusCheckRollup "
        "-q '{head: .headRefOid, comments: [.comments[] | "
        "{author: .author.login, body, createdAt, id}], "
        "checks: [.statusCheckRollup[] | {name, conclusion, status}]}'"
    )
    return json.loads(out)


def latest_verdicts(comments):
    """Latest verdict per lens. Returns {lens: (verdict, head, createdAt)}."""
    verdicts = {}
    for c in comments:
        body = c.get("body") or ""
        if MARKER not in body:
            continue
        lens_m = LENS_RE.search(body)
        verdict_m = VERDICT_RE.search(body)
        head_m = HEAD_RE.search(body)
        if not (lens_m and verdict_m and head_m):
            continue
        lens = lens_m.group(1).lower()
        key = (c.get("createdAt") or "", c.get("id") or 0)
        if lens not in verdicts or key > verdicts[lens][3]:
            verdicts[lens] = (verdict_m.group(1).upper(), head_m.group(1).lower(),
                              c.get("author"), key)
    return {l: v[:3] for l, v in verdicts.items()}


def check(repo, pr, lenses):
    data = fetch_pr(repo, pr)
    head = (data.get("head") or "").lower()
    reasons = []
    warnings = []

    # --- CI gate ---
    for ch in data.get("checks") or []:
        name = ch.get("name") or ""
        conclusion = (ch.get("conclusion") or "").upper()
        status = (ch.get("status") or "").upper()
        if IGNORED_CHECKS.search(name):
            continue
        failed = conclusion in ("FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED") or \
                 (status == "COMPLETED" and conclusion not in ("SUCCESS", "SKIPPED", "NEUTRAL"))
        if not failed:
            continue
        if BLOCKING_CHECKS.search(name):
            reasons.append(f"blocking check failing: {name} ({conclusion or status})")
        else:
            warnings.append(f"non-blocking check failing: {name} ({conclusion or status})")

    # --- review-team approvals ---
    verdicts = latest_verdicts(data.get("comments") or [])
    approvals = {}
    for lens in lenses:
        v = verdicts.get(lens)
        if v is None:
            reasons.append(f"no verdict from [{lens}] reviewer")
            approvals[lens] = {"verdict": None}
            continue
        verdict, vhead, author = v
        approvals[lens] = {"verdict": verdict, "head": vhead,
                           "on_current_head": vhead == head}
        if verdict != "APPROVE":
            reasons.append(f"[{lens}] latest verdict is {verdict} (not APPROVE)")
        elif vhead != head:
            reasons.append(f"[{lens}] APPROVE is stale: reviewed {vhead[:8]}, "
                           f"PR head is {head[:8]}")

    result = {"pass": not reasons, "reasons": reasons, "warnings": warnings,
              "repo": repo, "pr": pr, "head": head, "approvals": approvals}
    return result


def main():
    if len(sys.argv) < 3 or sys.argv[1] in ("-h", "--help"):
        sys.exit("usage: approval_gate.py <owner/repo> <pr> "
                 "[--lenses correctness,simplicity,tests]")
    repo, pr = sys.argv[1], sys.argv[2]
    lenses = ["correctness", "simplicity", "tests"]
    if "--lenses" in sys.argv:
        lenses = sys.argv[sys.argv.index("--lenses") + 1].split(",")
    try:
        result = check(repo, pr, lenses)
    except Exception as e:
        print(json.dumps({"pass": False, "reasons": [f"gate check error: {e}"]},
                         indent=2))
        sys.exit(1)
    print(json.dumps(result, indent=2))
    sys.exit(0 if result["pass"] else 1)


if __name__ == "__main__":
    main()
