#!/usr/bin/env python3
"""Approval-gate check for Codex PRs (oversight system, 2026-09-20).

A PR passes the gate only when BOTH hold on the CURRENT head:
  1. Required CI is green: semgrep + BuildBuddy successful. The
     `scribes-stg-preview-deploy` manual trigger (ACTION_REQUIRED/NEUTRAL) and
     gcbrun noise are not blockers (Shukant's leveled CI rule).
  2. Every required review lens has a latest model-advisory verdict comment of
     APPROVE whose HEAD equals the PR's current head SHA, and a separately
     authenticated, allowlisted human has formally approved that exact head.
     Stale approvals and CHANGES REQUESTED verdicts fail the gate.

Verdict comments are top-level PR comments posted by review-team workers
(land as ShukantPal via shared gh auth, so they carry the marker line):

  > \U0001F916 Codex (AI assistant) \u2014 [correctness] review verdict
  VERDICT: APPROVE
  HEAD: <full 40-hex sha reviewed>
  ROUND: <positive review-round number>
  LENSES: <comma-separated lenses seeded for this round>
  <short summary; findings when CHANGES REQUESTED>

Lenses: correctness, simplicity, tests (+ security when the round seeds it).
The PR comment watcher skips marker-bearing comments, so verdicts never
re-dispatch. Model verdicts are never treated as human proof. Human approval
must be a formal GitHub review from an actor named in the deployment-only
`approval_gate.human_review_actors` config; shared automation actors are
explicitly excluded from that allowlist.

Usage: approval_gate.py <owner/repo> <pr> [--lenses correctness,simplicity,tests]
Exit 0 with {"pass": true, ...}; exit 1 with {"pass": false, "reasons": [...]}.
Runs gh over SSH on Shukant's Mac (same transport as the other watchers).
"""
import json
import re
import subprocess
import sys
from dept_config import load_config, ssh_base, ssh_env
from dispatch_review_round import stored_rounds

CONFIG = load_config()
SSH_BASE = ssh_base(CONFIG.get("connection", {}))

# Checks that must be green for the gate. Everything else failing is a warning.
REQUIRED_CHECKS = {
    "semgrep": re.compile(r"semgrep", re.IGNORECASE),
    "BuildBuddy": re.compile(r"buildbuddy", re.IGNORECASE),
}
# Known noise: never a blocker.
IGNORED_CHECKS = re.compile(r"scribes-stg-preview-deploy", re.IGNORECASE)

MARKER = "\U0001F916"  # 🤖 — what the comment watcher keys its skip on
# Verdicts land through the shared GitHub account; marker text is copyable, but
# this actor identity is retrieved from GitHub's API and is not user-supplied.
TRUSTED_REVIEW_ACTORS = frozenset({"ShukantPal"})
VERDICT_RE = re.compile(r"^VERDICT:\s*(APPROVE|CHANGES REQUESTED)\s*$",
                        re.IGNORECASE | re.MULTILINE)
HEAD_RE = re.compile(r"^HEAD:\s*([0-9a-f]{40})\s*$", re.IGNORECASE | re.MULTILINE)
LENS_RE = re.compile(r"\[([a-z]+)\]\s+review verdict", re.IGNORECASE)
ROUND_RE = re.compile(r"^ROUND:\s*([1-9][0-9]*)\s*$", re.MULTILINE)
LENSES_RE = re.compile(r"^LENSES:\s*([a-z]+(?:,[a-z]+)*)\s*$", re.MULTILINE)


def human_review_actors():
    configured = CONFIG.get("approval_gate", {}).get("human_review_actors", [])
    if not isinstance(configured, list):
        return frozenset()
    actors = frozenset(a for a in configured if isinstance(a, str) and a)
    return actors - TRUSTED_REVIEW_ACTORS


def mac(cmd):
    p = subprocess.run(SSH_BASE + [cmd], capture_output=True, text=True,
                       env=ssh_env(), timeout=180)
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
    data = json.loads(out)
    pages = json.loads(mac(
        f"gh api --paginate --slurp repos/{repo}/pulls/{pr}/reviews"))
    data["reviews"] = [
        {"author": (review.get("user") or {}).get("login"),
         "state": review.get("state"), "submittedAt": review.get("submitted_at"),
         "id": review.get("id"), "commit": review.get("commit_id")}
        for page in pages for review in page
    ]
    return data


def latest_verdicts(comments):
    """Latest unambiguous model verdict per lens."""
    verdicts = {}
    for c in comments:
        body = c.get("body") or ""
        # GitHub comments normally retain newlines, but a reviewer once posted
        # literal `\\n` separators. Parse that harmless formatting mistake too.
        body = re.sub(r"\\+n", "\n", body)
        if MARKER not in body or c.get("author") not in TRUSTED_REVIEW_ACTORS:
            continue
        lenses = LENS_RE.findall(body)
        verdicts_found = VERDICT_RE.findall(body)
        heads = HEAD_RE.findall(body)
        rounds = ROUND_RE.findall(body)
        manifests = LENSES_RE.findall(body)
        if (len(lenses) != 1 or len(verdicts_found) != 1 or len(heads) != 1 or
                len(rounds) != 1 or len(manifests) != 1):
            continue
        lens = lenses[0].lower()
        round_number = int(rounds[0])
        manifest = tuple(sorted(manifests[0].split(",")))
        key = (round_number, c.get("createdAt") or "", c.get("id") or 0)
        if lens not in verdicts or key > verdicts[lens][5]:
            verdicts[lens] = (verdicts_found[0].upper(), heads[0].lower(),
                              c.get("author"), round_number, manifest, key)
    return {l: v[:5] for l, v in verdicts.items()}


def current_human_approval(reviews, head, actors):
    """Return a latest current-head approval from a distinct trusted actor."""
    latest = {}
    for position, review in enumerate(reviews):
        author = review.get("author")
        state = (review.get("state") or "").upper()
        if author not in actors or state not in ("APPROVED", "CHANGES_REQUESTED",
                                                 "DISMISSED"):
            continue
        review_id = review.get("id")
        key = (review.get("submittedAt") or "",
               review_id if isinstance(review_id, int) else position)
        if author not in latest or key > latest[author][0]:
            latest[author] = (key, review)
    for author, (_, review) in latest.items():
        if ((review.get("state") or "").upper() == "APPROVED" and
                (review.get("commit") or "").lower() == head):
            return {"author": author, "commit": head,
                    "submittedAt": review.get("submittedAt")}
    return None


def latest_seeded_round(repo, pr):
    """Return the newest persisted review-round contract, if one exists."""
    candidates = []
    for _path, data in stored_rounds(repo, pr):
        reviewers = data.get("reviewers")
        try:
            number = int(data.get("round"))
        except (TypeError, ValueError):
            continue
        if number < 1 or not isinstance(reviewers, dict):
            continue
        lenses = tuple(sorted({lens for lens in reviewers.values()
                               if isinstance(lens, str) and lens}))
        if not lenses:
            continue
        candidates.append((number, data, lenses))
    if not candidates:
        return None
    number, data, lenses = max(candidates, key=lambda item: item[0])
    return {"round": number, "head": (data.get("head") or "").lower(),
            "lenses": lenses, "status": data.get("status")}


def check(repo, pr, lenses):
    data = fetch_pr(repo, pr)
    head = (data.get("head") or "").lower()
    reasons = []
    warnings = []

    # --- CI gate ---
    checks = data.get("checks") or []
    for label, pattern in REQUIRED_CHECKS.items():
        matches = [ch for ch in checks if pattern.search(ch.get("name") or "")]
        if not matches:
            reasons.append(f"required check missing: {label}")
            continue
        for ch in matches:
            name = ch.get("name") or label
            conclusion = (ch.get("conclusion") or "").upper()
            status = (ch.get("status") or "").upper()
            if status != "COMPLETED" or conclusion != "SUCCESS":
                reasons.append(
                    f"required check not green: {name} "
                    f"({conclusion or status or 'missing status'})"
                )

    for ch in checks:
        name = ch.get("name") or ""
        conclusion = (ch.get("conclusion") or "").upper()
        status = (ch.get("status") or "").upper()
        if IGNORED_CHECKS.search(name):
            continue
        failed = conclusion in ("FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED") or \
                 (status == "COMPLETED" and conclusion not in ("SUCCESS", "SKIPPED", "NEUTRAL"))
        if not failed:
            continue
        if not any(pattern.search(name) for pattern in REQUIRED_CHECKS.values()):
            warnings.append(f"non-blocking check failing: {name} ({conclusion or status})")

    # --- review-team approvals ---
    verdicts = latest_verdicts(data.get("comments") or [])
    seeded_round = latest_seeded_round(repo, pr)
    latest_round = (seeded_round["round"] if seeded_round else
                    max((v[3] for v in verdicts.values()), default=None))
    manifests = {v[4] for v in verdicts.values() if v[3] == latest_round}
    declared_lenses = set()
    if seeded_round:
        declared_lenses.update(seeded_round["lenses"])
        if seeded_round["status"] != "published":
            reasons.append(
                f"latest seeded review round is {seeded_round['status'] or 'status unknown'}")
        if seeded_round["head"] != head:
            reasons.append(
                f"latest seeded review round targets {seeded_round['head'][:8]}, "
                f"PR head is {head[:8]}")
        if manifests and manifests != {seeded_round["lenses"]}:
            reasons.append("latest review verdicts do not match the seeded lens manifest")
    elif len(manifests) != 1:
        reasons.append("latest review round has no single consistent lens manifest")
    else:
        declared_lenses.update(next(iter(manifests)))
    required_lenses = list(dict.fromkeys([*lenses, *sorted(declared_lenses)]))
    approvals = {}
    for lens in required_lenses:
        v = verdicts.get(lens)
        if v is None:
            reasons.append(f"no verdict from [{lens}] reviewer")
            approvals[lens] = {"verdict": None}
            continue
        verdict, vhead, author, round_number, manifest = v
        approvals[lens] = {"verdict": verdict, "head": vhead,
                           "on_current_head": vhead == head,
                           "round": round_number}
        if round_number != latest_round:
            reasons.append(f"[{lens}] verdict is from round {round_number}; "
                           f"latest round is {latest_round}")
        elif verdict != "APPROVE":
            reasons.append(f"[{lens}] latest verdict is {verdict} (not APPROVE)")
        elif vhead != head:
            reasons.append(f"[{lens}] APPROVE is stale: reviewed {vhead[:8]}, "
                           f"PR head is {head[:8]}")

    actors = human_review_actors()
    human_approval = current_human_approval(data.get("reviews") or [], head, actors)
    if not actors:
        reasons.append("no separate human review actors configured")
    elif not human_approval:
        reasons.append("no current-head APPROVED review from an allowlisted human")

    result = {"pass": not reasons, "reasons": reasons, "warnings": warnings,
              "repo": repo, "pr": pr, "head": head, "approvals": approvals,
              "review_round": latest_round, "seeded_round": seeded_round,
              "human_approval": human_approval}
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
