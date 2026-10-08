"""Zigzag control-plane configuration — the configurer's source.

Edit this file, then materialize:
    python3 dept/config.py --materialize > dept/config.materialized.json
The pre-commit hook does this automatically; CI verifies the committed JSON
matches (``--check``). Review-loop policy deliberately lives only in the
personal ``~/.zigzag/config.yaml`` contract and is absent here.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from dataclasses import asdict, dataclass


# --- Shared constants: one definition, referenced everywhere. ---------------
REPOS = {
    "zigzag": "ShukantPal/zigzag",
    "leveled": "leveled-inc/leveled",
}

QUIET_HOURS = {"start": "22:00", "end": "07:00"}

SA_EMAIL = "zigzag@shukant.iam.gserviceaccount.com"
DESIGN_DOCS_FOLDER_ID = "1W_iTcpdYGVXj_NTmkfcgOm_GGREk1Nj3"


# --- Schema: the daemon's contract. Plain data, no behavior. ----------------
@dataclass(frozen=True)
class LoopOwnership:
    loop: str
    owner: str


@dataclass(frozen=True)
class DocRoute:
    doc_id: str
    assigned_session: str


@dataclass(frozen=True)
class WatcherConfig:
    name: str
    interval_s: int
    enabled: bool = True


# --- The configuration itself: declarative, boring on purpose. --------------
LOOP_OWNERSHIP = [
    LoopOwnership("pr-comment-watcher", "mac"),
    LoopOwnership("dependabot", "mac"),
    LoopOwnership("doc-router", "mac"),
    LoopOwnership("chat-surface", "vm"),
]

DOC_ROUTES = [
    DocRoute(
        doc_id="1OJdICdFEo4dwlIvo5pc-2LoG8S7jyPOigPtsKp7mLBc",
        assigned_session="configurer",
    ),
]

WATCHERS = [
    WatcherConfig("pr-comment-watcher", 300),
    WatcherConfig("dependabot", 1800),
    WatcherConfig("doc-router", 300),
]

# Derived, not duplicated: the router watches exactly the routed docs.
DOC_ROUTER_WATCH_LIST = sorted(route.doc_id for route in DOC_ROUTES)


# --- Validation: fail at build time, never in the daemon. -------------------
MATERIALIZED_PATH = pathlib.Path(__file__).with_name("config.materialized.json")


def validate() -> list[str]:
    """Return all configuration errors so callers get one actionable report."""
    errors = []
    loops = [ownership.loop for ownership in LOOP_OWNERSHIP]
    if len(loops) != len(set(loops)):
        errors.append("duplicate loop ownership entries")
    for route in DOC_ROUTES:
        if not route.assigned_session:
            errors.append(f"doc {route.doc_id}: no assigned session")
    return errors


# --- Materialization: deterministic JSON, byte-identical per source. --------
def materialize() -> str:
    """Build the exact JSON contract consumed by the daemon."""
    if errors := validate():
        raise SystemExit("config invalid:\n" + "\n".join(f"  - {error}" for error in errors))
    payload = {
        "generated_by": "dept/config.py --materialize (do not hand-edit)",
        "repos": REPOS,
        "loop_ownership": [asdict(ownership) for ownership in LOOP_OWNERSHIP],
        "doc_routes": [asdict(route) for route in DOC_ROUTES],
        "doc_router_watch_list": DOC_ROUTER_WATCH_LIST,
        "watchers": [asdict(watcher) for watcher in WATCHERS],
        "quiet_hours": QUIET_HOURS,
        "service_account": SA_EMAIL,
        "design_docs_folder_id": DESIGN_DOCS_FOLDER_ID,
    }
    return json.dumps(payload, indent=2, sort_keys=True) + "\n"


def main(argv: list[str] | None = None) -> int:
    """Materialize the config or verify that the checked-in artifact is fresh."""
    parser = argparse.ArgumentParser()
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--materialize", action="store_true")
    action.add_argument(
        "--check", action="store_true", help="CI: fail if committed JSON is stale"
    )
    args = parser.parse_args(argv)
    rendered = materialize()
    if args.check:
        try:
            committed = MATERIALIZED_PATH.read_text()
        except OSError as error:
            print(f"cannot read {MATERIALIZED_PATH}: {error}", file=sys.stderr)
            return 1
        if committed != rendered:
            print(
                "config.materialized.json is stale: re-run --materialize",
                file=sys.stderr,
            )
            return 1
        print("config.materialized.json is fresh")
        return 0
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
