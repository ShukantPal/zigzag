#!/usr/bin/env python3
"""VM compatibility wrapper for the Mac daemon's canonical approval gate.

The VM deliberately does not parse review policy, GitHub comments, or CI
checks. During shadow rollout it asks the Mac binary for the complete gate
decision, keeping admission and policy semantics identical on both sides.

Usage: approval_gate.py <owner/repo> <pr>
Exit 0 with {"pass": true, ...}; exit 1 with {"pass": false, "reasons": [...]}.
Runs the canonical gate through the authenticated GUI-session Zigzag relay.
"""
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dept_config import load_config

CONFIG = load_config()
CONNECTION = CONFIG.get("connection", {})
ZIGZAG_URL = os.environ.get("ZIGZAG_URL", CONNECTION.get("zigzag_url", ""))
ZIGZAG_TOKEN_FILE = os.path.expanduser(CONNECTION.get("zigzag_token_file", ""))


def relay_call(path):
    if not ZIGZAG_URL or not ZIGZAG_TOKEN_FILE:
        raise RuntimeError("zigzag relay connection is not configured")
    with open(ZIGZAG_TOKEN_FILE) as f:
        token = f.read().strip()
    request = urllib.request.Request(
        ZIGZAG_URL + path,
        headers={"Authorization": f"Bearer {token}"},
        method="GET",
    )
    proxy = os.environ.get("HTTPS_PROXY", "")
    proxy = proxy.rsplit(":", 1)[0] + ":3130" if ":" in proxy else ""
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    try:
        with opener.open(request, timeout=180) as response:
            return json.loads(response.read().decode())
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"review gate failed with HTTP {error.code}") from error


def fetch_gate(repo, pr):
    query = urllib.parse.urlencode({"repository": repo, "pull_request": str(pr)})
    return relay_call(f"/v1/review-gate?{query}")


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
