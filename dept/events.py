"""Schema-v1 department audit event construction and delivery.

This module is intentionally small so department transition owners can publish
facts without duplicating the relay envelope or accidentally changing an event
identifier while retrying.
"""

from __future__ import annotations

import datetime as dt
import json
import socket
import urllib.request
from pathlib import Path
from typing import Any, Callable

DEPARTMENT_KINDS = frozenset(
    {
        "review_wait_started",
        "review_feedback_received",
        "review_work_started",
        "human_wait_started",
        "human_wait_ended",
    }
)


def utc_millis(now: dt.datetime | None = None) -> str:
    now = now or dt.datetime.now(dt.timezone.utc)
    return now.astimezone(dt.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def department_clock() -> str:
    """Return a per-boot-ish identifier without exposing process arguments."""
    boot = "boot-unknown"
    try:
        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip() or boot
    except OSError:
        pass
    return f"vm-department:{socket.gethostname() or 'unknown-host'}:{boot}"


def transition_event(
    *,
    event_id: str,
    task_id: str,
    execution_id: str,
    kind: str,
    payload: dict[str, Any] | None = None,
    occurred_at: str | None = None,
    clock: str | None = None,
) -> dict[str, Any]:
    """Build a safe, idempotent schema-v1 event for a department transition."""
    if kind not in DEPARTMENT_KINDS:
        raise ValueError(f"unsupported department transition: {kind}")
    if not event_id or not task_id or not execution_id:
        raise ValueError("event_id, task_id, and execution_id are required")
    return {
        "schema_version": 1,
        "id": event_id,
        "task_id": task_id,
        "execution_id": execution_id,
        "kind": kind,
        "source": "vm-department",
        "occurred_at": occurred_at or utc_millis(),
        "clock": clock or department_clock(),
        "payload": payload or {},
    }


def post_event(
    event: dict[str, Any], *, url: str, token_file: Path, attempts: int = 2,
    opener: Callable[..., Any] = urllib.request.urlopen,
) -> None:
    """Publish an event, preserving its id and complete body on every retry."""
    if attempts < 1:
        raise ValueError("attempts must be positive")
    token = token_file.read_text().strip()
    request = urllib.request.Request(
        url.rstrip("/") + "/v1/events",
        data=json.dumps(event, separators=(",", ":")).encode(),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        method="POST",
    )
    last_error: OSError | None = None
    for _ in range(attempts):
        try:
            with opener(request, timeout=10) as response:
                response.read()
            return
        except OSError as error:
            last_error = error
    assert last_error is not None
    raise last_error
