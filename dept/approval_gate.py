#!/usr/bin/env python3
"""VM compatibility wrapper for the Mac daemon's canonical approval gate.

The VM deliberately does not parse review policy, GitHub comments, or CI
checks. During shadow rollout it asks the Mac binary for the complete gate
decision, keeping admission and policy semantics identical on both sides.

Usage: approval_gate.py <owner/repo> <pr>
Exit 0 with {"pass": true, ...}; exit 1 with {"pass": false, "reasons": [...]}.
Runs the canonical gate over SSH on Shukant's Mac.
"""
import json
import shlex
import subprocess
import sys
from dept_config import load_config, ssh_base, ssh_env

CONFIG = load_config()
SSH_BASE = ssh_base(CONFIG.get("connection", {}))


def mac(cmd):
    p = subprocess.run(SSH_BASE + [cmd], capture_output=True, text=True,
                       env=ssh_env(), timeout=180)
    if p.returncode != 0:
        raise RuntimeError(f"mac cmd failed: {cmd[:100]} :: {p.stderr.strip()[:200]}")
    return p.stdout


def fetch_gate(repo, pr):
    command = f"zigzag review-gate {shlex.quote(repo)} {shlex.quote(str(pr))}"
    return json.loads(mac(command))


def main():
    if len(sys.argv) != 3 or sys.argv[1] in ("-h", "--help"):
        sys.exit("usage: approval_gate.py <owner/repo> <pr>")
    repo, pr = sys.argv[1], sys.argv[2]
    try:
        result = fetch_gate(repo, pr)
    except Exception as e:
        print(json.dumps({"pass": False, "reasons": [f"gate check error: {e}"]},
                         indent=2))
        sys.exit(1)
    print(json.dumps(result, indent=2))
    sys.exit(0 if result["pass"] else 1)


if __name__ == "__main__":
    main()
