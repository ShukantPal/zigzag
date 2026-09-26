#!/usr/bin/env python3
"""Department command entry point available in this repository."""

from __future__ import annotations

import sys

try:  # `python3 dept/dept.py` and `python3 -m dept.dept` are both supported.
    from .status import main as status_main
except ImportError:  # pragma: no cover - direct script execution path
    from status import main as status_main


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] in {"-h", "--help"}:
        print("usage: dept status [--url URL] [--state-file PATH] [--token-file PATH] [--interval SECONDS] [--once]")
        return 0
    if argv[0] != "status":
        print(f"dept: unsupported command: {argv[0]}", file=sys.stderr)
        return 2
    return status_main(argv[1:])


if __name__ == "__main__":
    raise SystemExit(main())
