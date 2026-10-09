"""Read-only, Mac-local execution state model and curses renderer for dept."""

from __future__ import annotations

import argparse
import curses
import datetime as dt
import json
import os
import shlex
import sys
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

try:  # `python3 dept/status.py` and `python3 -m dept.status` are both supported.
    from .dept_config import load_config
except ImportError:  # pragma: no cover - direct script execution path
    from dept_config import load_config

DEFAULT_STATE_FILE = Path("~/.codex/zigzag/events.json").expanduser()
DEFAULT_TOKEN_FILE = Path("~/.codex/zigzag/zigzag.token").expanduser()
# Keep this in step with relay-core's bounded durable agent spool.  Requesting
# its complete retained window means a few large pipe-read chunks cannot hide
# the latest 40 logical lines from the transcript view.
RELAY_OUTPUT_TAIL_BYTES = 32 * 1024 * 1024


# Task ids the relay emits for its own internal maintenance.  They are real
# executions, but they clutter the default view, so they stay hidden unless
# --all is passed.
INTERNAL_TASK_IDS = frozenset({"relay-update"})

# Bound on the in-memory event window for a long-lived TUI session.  The
# server's retention already bounds the bootstrap window; this only stops
# unbounded growth while streaming.
MAX_STREAM_EVENTS = 10_000


def dept_task_id(task_id: str) -> str:
    """Map a relay spawn id back to its dept task directory.

    dept.py spawns relay work as ``codex-<tid>`` while the on-disk task dir
    is ``~/.codex/dept/<tid>/``.  Without this mapping the DIR column (and
    the local transcript path) never resolves for relay tasks.
    """
    return task_id[len("codex-"):] if task_id.startswith("codex-") else task_id


def parse_time(value: object) -> dt.datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed if parsed.tzinfo is not None else None
    except ValueError:
        return None


def parse_started_at(value: object) -> dt.datetime | None:
    """Parse the RFC 3339 or Unix-second timestamp returned by `/v1/agents`."""
    parsed = parse_time(value)
    if parsed is not None:
        return parsed
    try:
        seconds = float(value) if isinstance(value, (str, int, float)) else None
        return dt.datetime.fromtimestamp(seconds, tz=dt.timezone.utc) if seconds is not None else None
    except (OverflowError, OSError, TypeError, ValueError):
        return None


def agent_worktrees(state_file: Path = DEFAULT_STATE_FILE) -> dict[str, str]:
    """Return persisted API-agent worktrees, keyed by relay task id.

    The relay's status endpoint deliberately omits ``worktree_path`` even
    though it is durably recorded in its adjacent ``events.agents.json``
    registry.  This is a best-effort, read-only enrichment for the status UI;
    unavailable or malformed local state simply leaves DIR empty.
    """
    registry_file = state_file.with_suffix(".agents.json")
    try:
        decoded = json.loads(registry_file.read_text())
    except (OSError, UnicodeError, json.JSONDecodeError):
        return {}
    if not isinstance(decoded, dict) or not isinstance(decoded.get("agents"), list):
        return {}
    worktrees: dict[str, str] = {}
    for record in decoded["agents"]:
        if not isinstance(record, dict):
            continue
        task_id, worktree = record.get("task_id"), record.get("worktree_path")
        if isinstance(task_id, str) and task_id and isinstance(worktree, str) and worktree:
            worktrees[task_id] = worktree
    return worktrees


def compact_path(path: str, width: int) -> str:
    """Fit a path in a table cell while preserving its useful right end."""
    if width <= 0:
        return ""
    if len(path) <= width:
        return path
    if width <= 3:
        return "." * width
    return "..." + path[-(width - 3):]


def event_time(event: dict[str, Any]) -> dt.datetime | None:
    return parse_time(event.get("occurred_at"))


def duration_between(left: dict[str, Any], right: dict[str, Any]) -> dt.timedelta | None:
    """Only subtract consecutive facts when the emitting clock is identical."""
    if not left.get("clock") or left.get("clock") != right.get("clock"):
        return None
    start, end = event_time(left), event_time(right)
    if start is None or end is None or end < start:
        return None
    return end - start


def is_cross_clock(left: dict[str, Any], right: dict[str, Any]) -> bool:
    """A boundary is only a disagreement between two present clock ids."""
    return bool(left.get("clock") and right.get("clock") and left.get("clock") != right.get("clock"))


def observed_duration(events: list[dict[str, Any]]) -> tuple[dt.timedelta | None, bool]:
    """Sum adjacent same-clock intervals; report any unmeasurable boundary."""
    total = dt.timedelta()
    measured = False
    boundary = False
    for left, right in zip(events, events[1:]):
        duration = duration_between(left, right)
        if duration is None:
            boundary |= is_cross_clock(left, right)
        else:
            total += duration
            measured = True
    return (total if measured else None), boundary


def format_duration(duration: dt.timedelta | None) -> str:
    if duration is None:
        return "not observed"
    seconds = int(duration.total_seconds())
    if seconds < 60:
        return f"{seconds}s"
    minutes, seconds = divmod(seconds, 60)
    if minutes < 60:
        return f"{minutes}m {seconds:02d}s"
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h {minutes:02d}m"


def sort_events(events: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    def key(event: dict[str, Any]) -> tuple[str, int, str]:
        try:
            sequence = int(event.get("sequence") or 0)
        except (TypeError, ValueError):
            sequence = 0
        return (
            str(event.get("received_at") or event.get("occurred_at") or ""),
            sequence,
            str(event.get("id") or ""),
        )
    return sorted(events, key=key)


def execution_key(event: dict[str, Any]) -> str | None:
    value = event.get("execution_id")
    return value if isinstance(value, str) and value else None


@dataclass
class Execution:
    execution_id: str
    task_id: str = "?"
    events: list[dict[str, Any]] = field(default_factory=list)
    agent_state: str = "not observed"
    agent_id: str | None = None
    agent_started_at: dt.datetime | None = None
    worktree_path: str | None = None
    degraded: list[str] = field(default_factory=list)
    relay_lost: bool = False

    @property
    def phase(self) -> str:
        if self.events:
            return str(self.events[-1].get("kind", "not observed"))
        if self.agent_state != "not observed":
            return f"agent {self.agent_state}"
        return "not observed"

    @property
    def latest_event(self) -> str:
        if not self.events:
            return "not observed"
        return str(self.events[-1].get("occurred_at") or self.events[-1].get("received_at") or "not observed")

    def running_started_at(self) -> dt.datetime | None:
        if self.agent_started_at is not None:
            return self.agent_started_at
        return next(
            (
                event_time(event)
                for event in self.events
                if event.get("kind") == "process_spawned" and event_time(event) is not None
            ),
            None,
        )

    def current_elapsed(self, now: dt.datetime | None = None) -> dt.timedelta | None:
        if self.agent_state == "running":
            started_at = self.running_started_at()
            if started_at is not None:
                now = now or dt.datetime.now(dt.timezone.utc)
                if now.tzinfo is None:
                    now = now.replace(tzinfo=dt.timezone.utc)
                return max(now - started_at, dt.timedelta())
        if len(self.events) < 2:
            return None
        return duration_between(self.events[-2], self.events[-1])

    def total_elapsed(self, now: dt.datetime | None = None) -> tuple[dt.timedelta | None, bool]:
        if self.agent_state == "running":
            elapsed = self.current_elapsed(now)
            if elapsed is not None:
                return elapsed, False
        return observed_duration(self.events)


def build_executions(
    events: Iterable[dict[str, Any]], agents: Iterable[dict[str, Any]], *, relay_lost: bool = False,
    include_internal: bool = False, persisted_agent_worktrees: dict[str, str] | None = None,
) -> list[Execution]:
    grouped: dict[str, Execution] = {}
    for event in sort_events(events):
        execution_id = execution_key(event)
        if execution_id is None:
            continue
        if not include_internal and str(event.get("task_id") or "") in INTERNAL_TASK_IDS:
            continue
        execution = grouped.setdefault(execution_id, Execution(execution_id))
        execution.task_id = str(event.get("task_id") or execution.task_id)
        execution.events.append(event)
    for agent in agents:
        if not include_internal and str(agent.get("task_id") or "") in INTERNAL_TASK_IDS:
            continue
        task_id = str(agent.get("task_id") or "?")
        execution_id = agent.get("execution_id")
        if not isinstance(execution_id, str) or not execution_id:
            if not task_id.startswith("agent-"):
                continue
            # Agents created through POST /v1/agents use relay-owned
            # `agent-*` task ids.  Older retained rows can lack an execution
            # id, so attach by that unique task id before using a stable
            # synthetic key.  This still renders their live agent status.
            matches = [item for item in grouped.values() if item.task_id == task_id]
            execution_id = matches[0].execution_id if len(matches) == 1 else f"agent-task:{task_id}"
        execution = grouped.setdefault(execution_id, Execution(execution_id, task_id))
        if task_id.startswith("agent-"):
            execution.task_id = task_id
        execution.agent_state = str(agent.get("state") or "not observed")
        agent_id = agent.get("id")
        if isinstance(agent_id, str) and agent_id:
            execution.agent_id = agent_id
        started_at = parse_started_at(agent.get("started_at"))
        if started_at is not None:
            execution.agent_started_at = started_at
        # Newer endpoints may provide either spelling directly.  Current
        # relay status rows omit both, so enrich them from the local durable
        # agent registry by handle.
        worktree_path = agent.get("worktree_path") or agent.get("worktree")
        if not isinstance(worktree_path, str) or not worktree_path:
            worktree_path = (persisted_agent_worktrees or {}).get(task_id)
        if isinstance(worktree_path, str) and worktree_path:
            execution.worktree_path = worktree_path
        if agent.get("audit_degraded"):
            execution.degraded.append("audit degraded")
        if agent.get("log_degraded"):
            execution.degraded.append("agent log degraded")
    for execution in grouped.values():
        if not execution.worktree_path and execution.task_id.startswith("agent-"):
            execution.worktree_path = (persisted_agent_worktrees or {}).get(execution.task_id)
        execution.relay_lost = relay_lost
    def newest_first(execution: Execution) -> float:
        if not execution.events:
            return float("inf")
        observed = event_time(execution.events[-1])
        return -(observed.timestamp() if observed else 0.0)

    return sorted(
        grouped.values(),
        key=lambda execution: (execution.agent_state != "running", newest_first(execution)),
    )


def audit_directory(state_file: Path) -> Path:
    return state_file.with_suffix(".audit")


def valid_sequence(event: dict[str, Any]) -> bool:
    try:
        int(event.get("sequence") or 0)
    except (TypeError, ValueError):
        return False
    return True


def read_audit_events(state_file: Path) -> tuple[list[dict[str, Any]], list[str]]:
    events: list[dict[str, Any]] = []
    warnings: list[str] = []
    directory = audit_directory(state_file)
    if not directory.is_dir():
        return events, [f"audit directory unavailable: {directory} does not exist"]
    try:
        files = list(directory.glob("*.jsonl"))
    except OSError as error:
        return events, [f"audit directory unavailable: {error}"]
    for path in files:
        try:
            lines = path.read_text().splitlines()
        except (OSError, UnicodeError) as error:
            warnings.append(f"degraded audit log {path.name}: {error}")
            continue
        for line_number, line in enumerate(lines, start=1):
            if not line.strip():
                continue
            try:
                decoded = json.loads(line)
            except json.JSONDecodeError as error:
                warnings.append(f"degraded audit log {path.name}:{line_number}: {error.msg}")
                continue
            if not isinstance(decoded, dict):
                warnings.append(f"degraded audit log {path.name}:{line_number}: event is not an object")
            elif not valid_sequence(decoded):
                warnings.append(f"degraded audit log {path.name}:{line_number}: sequence is not numeric")
            else:
                events.append(decoded)
    return events, warnings


def get_json(url: str, token: str) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}", "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=10) as response:
        decoded = json.loads(response.read())
    if not isinstance(decoded, dict):
        raise ValueError("relay returned a non-object JSON response")
    return decoded


class EventStream:
    """Subscribe to the relay's /v1/events long-poll endpoint.

    Bootstrap once (full retained window + agent table), then poll() blocks
    up to ``timeout`` seconds for newly pushed events and folds them into the
    in-memory model.  The agent table is patched from process lifecycle
    events, so the steady state issues no repeated /v1/agents fetches either.
    """

    def __init__(self, url: str, token: str) -> None:
        self.url = url.rstrip("/")
        self.token = token
        self.epoch = ""
        self.after = 0
        self.events: list[dict[str, Any]] = []
        self.agents: dict[str, dict[str, Any]] = {}
        self.lost = False
        self.poll_warnings: list[str] = []

    def _get(self, path: str, timeout: float) -> dict[str, Any]:
        request = urllib.request.Request(
            self.url + path,
            headers={"Authorization": f"Bearer {self.token}", "Accept": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=timeout + 15) as response:
            decoded = json.loads(response.read())
        if not isinstance(decoded, dict):
            raise ValueError("relay returned a non-object JSON response")
        return decoded

    def _seed_agents(self) -> None:
        try:
            values = self._get("/v1/agents", 10).get("agents", [])
        except (OSError, ValueError, json.JSONDecodeError) as error:
            self.poll_warnings.append(f"relay agents unavailable: {error}")
            return
        if not isinstance(values, list):
            self.poll_warnings.append("relay agents unavailable: agents is not an array")
            return
        for agent in values:
            if isinstance(agent, dict) and isinstance(agent.get("id"), str):
                self.agents[agent["id"]] = agent

    def bootstrap(self) -> None:
        """One full-window fetch plus the agent table; the only heavy requests."""
        self._ingest(self._get("/v1/events?after=0&timeout=0", 10))
        self._seed_agents()

    def _apply_agent_event(self, event: dict[str, Any]) -> None:
        kind = event.get("kind")
        payload = event.get("payload")
        if not isinstance(payload, dict):
            return
        agent_id = payload.get("agent_id")
        if not isinstance(agent_id, str) or not agent_id:
            return
        if kind == "process_spawned":
            self.agents[agent_id] = {
                "id": agent_id,
                "task_id": event.get("task_id"),
                "execution_id": event.get("execution_id"),
                "state": "running",
                "started_at": event.get("occurred_at"),
            }
        elif kind in ("process_completed", "process_failed"):
            agent = self.agents.get(agent_id)
            if agent is None:
                return
            agent["state"] = str(
                payload.get("state") or ("failed" if kind == "process_failed" else "succeeded")
            )
            if payload.get("exit_code") is not None:
                agent["exit_code"] = payload["exit_code"]

    def _ingest(self, message: dict[str, Any]) -> bool:
        """Fold one /v1/events response into the model; True if it changed."""
        self.poll_warnings = []
        epoch = message.get("epoch")
        if not isinstance(epoch, str):
            self.poll_warnings.append("relay events unavailable: epoch is not a string")
            return False
        if message.get("reset"):
            # The server rolled its epoch: drop the window and re-seed.  The
            # response already starts from the new beginning.
            self.events, self.agents = [], {}
            self.epoch, self.after = "", 0
            self._seed_agents()
        values = message.get("events", [])
        if not isinstance(values, list):
            self.poll_warnings.append("relay events unavailable: events is not an array")
            new: list[dict[str, Any]] = []
        else:
            new = [event for event in values if isinstance(event, dict) and valid_sequence(event)]
            if len(new) != len(values):
                self.poll_warnings.append("relay events unavailable: invalid event record")
        changed = bool(new)
        if new:
            for event in new:
                self._apply_agent_event(event)
            self.events = merged_events(self.events, new)[-MAX_STREAM_EVENTS:]
            if any(
                event.get("kind") in ("process_completed", "process_failed")
                and isinstance((event.get("payload") or {}).get("agent_id"), str)
                and (event.get("payload") or {}).get("agent_id") not in self.agents
                for event in new
            ):
                # A lifecycle event for an agent we never saw (e.g. the relay
                # restarted mid-flight): re-seed the table instead of polling.
                self._seed_agents()
        self.epoch = epoch
        try:
            self.after = int(message.get("next") or 0)
        except (TypeError, ValueError):
            pass
        if message.get("lost"):
            self.lost = True
        return changed

    def poll(self, timeout: float) -> bool:
        """Block up to ``timeout`` seconds for pushed events; True if changed."""
        try:
            message = self._get(
                f"/v1/events?after={self.after}"
                f"&epoch={urllib.parse.quote(self.epoch, safe='')}"
                f"&timeout={max(1, int(timeout))}",
                timeout,
            )
        except (OSError, ValueError, json.JSONDecodeError) as error:
            self.poll_warnings = [f"relay events unavailable: {error}"]
            return False
        return self._ingest(message)


def mac_dept_root() -> Path:
    """Return the configured Mac task root, not the host running this UI."""
    configured = os.environ.get("CODEX_DEPT_REMOTE_DIR")
    if not configured:
        connection = load_config().get("connection", {})
        configured = connection.get("remote_dept") if isinstance(connection, dict) else None
    return Path(configured) if isinstance(configured, str) and configured else Path("~/.codex/dept").expanduser()


def transcript_path(task_id: str, root: Path | None = None) -> Path:
    """Return the Mac-local final-message path created for every dept task."""
    return (root or mac_dept_root()) / dept_task_id(task_id) / "last-message.txt"


def execution_transcript_path(execution: Execution, root: Path | None = None) -> Path:
    """Use an API-created agent's worktree before falling back to dept layout."""
    if execution.worktree_path:
        return Path(execution.worktree_path) / "last-message.txt"
    return transcript_path(execution.task_id, root)


def task_workdir(task_id: str, root: Path | None = None) -> str:
    """Return the task working directory from dir.txt, or "" if unavailable."""
    try:
        text = ((root or mac_dept_root()) / dept_task_id(task_id) / "dir.txt").read_text()
        return text.strip()
    except (OSError, UnicodeError):
        return ""


def transcript_command(task_id: str, root: Path | None = None) -> str:
    # BSD tail exits when a path has not been created yet; -F retries it.
    return f"tail -F {shlex.quote(str(transcript_path(task_id, root)))}"


def relay_output(
    url: str, token_file: Path, agent_id: str, *, tail: int = RELAY_OUTPUT_TAIL_BYTES,
) -> tuple[list[str], str | None]:
    """Read a bounded, read-only relay spool window and keep its last 40 lines."""
    try:
        token = token_file.read_text().strip()
    except (OSError, UnicodeError) as error:
        return [], f"relay credentials unavailable: {error}"
    if not token:
        return [], "relay credentials unavailable: token is empty"
    try:
        message = get_json(
            f"{url.rstrip('/')}/v1/agents/{urllib.parse.quote(agent_id, safe='')}/logs?stream=both&tail={tail}&follow=0",
            token,
        )
        records = message.get("records")
        if not isinstance(records, list):
            return [], "relay output unavailable: records is not an array"
        # Relay records are pipe-read chunks, rather than line records.  Join
        # them in cursor order before splitting so a logical line that crosses
        # a read (or stream) boundary is rendered once, intact.
        chunks: list[str] = []
        for record in records:
            if isinstance(record, dict) and isinstance(record.get("data"), str):
                chunks.append(record["data"])
        return "".join(chunks).splitlines()[-40:], None
    except (OSError, ValueError, json.JSONDecodeError) as error:
        return [], f"relay output unavailable: {error}"


def merged_events(audit: Iterable[dict[str, Any]], recent: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    unique: dict[str, dict[str, Any]] = {}
    anonymous: list[dict[str, Any]] = []
    for event in list(audit) + list(recent):
        event_id = event.get("id")
        if isinstance(event_id, str) and event_id:
            unique[event_id] = event
        else:
            anonymous.append(event)
    return [*unique.values(), *anonymous]


def flags(execution: Execution) -> str:
    entries = list(dict.fromkeys(execution.degraded))
    if execution.relay_lost:
        entries.append("relay events lost")
    _, cross_clock = execution.total_elapsed()
    if cross_clock:
        entries.append("cross-clock boundary")
    return ", ".join(entries) or "healthy"


def detail_lines(execution: Execution, limit: int = 12) -> list[str]:
    lines = [f"{execution.task_id} / {execution.execution_id} — {flags(execution)}"]
    shown = execution.events[-limit:]
    for previous, event in zip([None, *shown], shown):
        if previous is not None and is_cross_clock(previous, event):
            lines.append(
                f"  ↳ cross-clock: {previous.get('occurred_at', '?')} → {event.get('occurred_at', '?')} (not subtracted)"
            )
        elif previous is not None and duration_between(previous, event) is None:
            lines.append("  ↳ timing not observed: invalid, missing, or out-of-order same-clock timestamp")
        lines.append(
            f"  {event.get('occurred_at', '?')}  {event.get('kind', '?')}  [{event.get('source', '?')}]"
        )
    return lines


def transcript_lines(
    execution: Execution, output: list[str], error: str | None, root: Path | None = None,
) -> list[str]:
    path = execution_transcript_path(execution, root)
    lines = [
        f"Transcript for {execution.task_id} / {execution.execution_id}",
        f"Source: {path}",
        f"Command: tail -F {shlex.quote(str(path))}",
    ]
    if execution.agent_id is None:
        lines.append("Relay output unavailable: no retained supervised agent matches this execution.")
    elif error:
        lines.append(error)
    elif output:
        lines.extend(["Relay output (last 40 lines):", *output])
    else:
        lines.append("Relay output: no spool output yet.")
    return lines


class StatusScreen:
    def __init__(
        self, url: str, state_file: Path, token_file: Path, interval: float, task_root: Path | None = None,
        include_internal: bool = False,
    ) -> None:
        self.url, self.state_file, self.token_file, self.interval = url, state_file, token_file, interval
        self.task_root = task_root or mac_dept_root()
        self.include_internal = include_internal
        self.executions: list[Execution] = []
        self.warnings: list[str] = []
        self.audit_events: list[dict[str, Any]] = []
        self.audit_warnings: list[str] = []
        self.stream: EventStream | None = None
        self.selected = 0
        self.offset = 0
        self.h_offset = 0
        self.show_transcript = False
        self.transcript_output: list[str] = []
        self.transcript_error: str | None = None
        self.transcript_key: tuple | None = None

    def _read_token(self) -> str | None:
        try:
            token = self.token_file.read_text().strip()
        except (OSError, UnicodeError) as error:
            self.warnings.append(f"relay credentials unavailable: {error}")
            return None
        if not token:
            self.warnings.append("relay credentials unavailable: token is empty")
            return None
        return token

    def bootstrap(self) -> None:
        """One-time load: local audit log plus a single event-window/agent fetch."""
        self.audit_events, self.audit_warnings = read_audit_events(self.state_file)
        self.warnings = list(self.audit_warnings)
        token = self._read_token()
        if token is None:
            self.stream = None
        else:
            self.stream = EventStream(self.url, token)
            try:
                self.stream.bootstrap()
            except (OSError, ValueError, json.JSONDecodeError) as error:
                self.warnings.append(f"relay events unavailable: {error}")
                self.stream = None
            else:
                self.warnings.extend(self.stream.poll_warnings)
        self._rebuild()

    def _rebuild(self) -> None:
        recent = self.stream.events if self.stream is not None else []
        agents = list(self.stream.agents.values()) if self.stream is not None else []
        lost = self.stream.lost if self.stream is not None else False
        self.executions = build_executions(
            merged_events(self.audit_events, recent), agents,
            relay_lost=lost, include_internal=self.include_internal,
            persisted_agent_worktrees=agent_worktrees(self.state_file),
        )
        self.selected = min(self.selected, max(len(self.executions) - 1, 0))
        self.offset = min(self.offset, self.selected)

    def update(self) -> bool:
        """Long-poll the event stream; rebuild the view when pushed events arrive."""
        if self.stream is None:
            return False
        changed = self.stream.poll(self.interval)
        self.warnings = [*self.audit_warnings, *self.stream.poll_warnings]
        if changed:
            self._rebuild()
            self._refresh_transcript_if_stale()
        return changed

    def _refresh_transcript_if_stale(self) -> None:
        # The transcript spool is fetched on demand (selection change or new
        # events for the selected execution), never on a refresh tick.
        if not self.show_transcript or not self.executions:
            return
        execution = self.executions[self.selected]
        key = (execution.execution_id, len(execution.events))
        if key == self.transcript_key:
            return
        self.transcript_key = key
        if execution.agent_id is None:
            self.transcript_output, self.transcript_error = [], None
        else:
            self.transcript_output, self.transcript_error = relay_output(
                self.url, self.token_file, execution.agent_id,
            )

    def move_selection(self, delta: int, rows: int) -> None:
        previous = self.selected
        self.selected = min(max(self.selected + delta, 0), max(len(self.executions) - 1, 0))
        if self.selected < self.offset:
            self.offset = self.selected
        elif self.selected >= self.offset + rows:
            self.offset = self.selected - rows + 1
        if self.selected != previous:
            self.transcript_key = None
            self.transcript_output, self.transcript_error = [], None
            self._refresh_transcript_if_stale()

    def move_horizontal(self, delta: int) -> None:
        self.h_offset = max(0, self.h_offset + delta)

    def visible_rows(self, height: int) -> int:
        return 1 if self.show_transcript else max(1, height // 2 - 2)

    def toggle_transcript(self) -> None:
        self.show_transcript = not self.show_transcript
        self.offset = self.selected
        self.transcript_key = None
        self.transcript_output, self.transcript_error = [], None
        self._refresh_transcript_if_stale()

    def run(self, screen: curses.window) -> None:
        curses.curs_set(0)
        screen.timeout(100)
        self.bootstrap()
        self.draw(screen)
        while True:
            # The long-poll inside update() blocks up to `interval` seconds
            # for pushed events, so the UI wakes the moment work happens and
            # issues no request at all while idle.
            self.update()
            # A long-poll expiry is also a render tick.  Running-agent
            # elapsed time is computed from started_at during each render,
            # so it advances even when no new lifecycle event arrives.
            self.draw(screen)
            key = screen.getch()
            if key == -1:
                continue
            if key in (ord("q"), 27):
                if self.show_transcript:
                    self.toggle_transcript()
                else:
                    return
            elif key in (curses.KEY_ENTER, 10, 13, ord("t")):
                self.toggle_transcript()
            elif key in (curses.KEY_UP, ord("k")):
                self.move_selection(-1, self.visible_rows(screen.getmaxyx()[0]))
            elif key in (curses.KEY_DOWN, ord("j")):
                self.move_selection(1, self.visible_rows(screen.getmaxyx()[0]))
            elif key in (curses.KEY_LEFT, ord("h")):
                self.move_horizontal(-8)
            elif key in (curses.KEY_RIGHT, ord("l")):
                self.move_horizontal(8)
            else:
                continue
            self.draw(screen)

    def draw(self, screen: curses.window) -> None:
        screen.erase()
        height, width = screen.getmaxyx()
        header = "dept status — read-only  ↑↓ select  ←→/h/l scroll  Enter/t transcript  q quit"
        if self.show_transcript:
            header = "dept status — transcript (read-only)  q/Esc back"
        screen.addnstr(0, 0, header, width - 1, curses.A_BOLD)
        columns = "TASK                 PHASE                    PHASE ELAPSED  OBSERVED TOTAL  STATE       AGENT ID             DIR                            LAST EVENT"
        screen.addnstr(1, 0, columns[self.h_offset:self.h_offset + width - 1], width - 1, curses.A_UNDERLINE)
        rows = self.visible_rows(height)
        now = dt.datetime.now(dt.timezone.utc)
        for row, execution in enumerate(self.executions[self.offset:self.offset + rows], start=2):
            total, boundary = execution.total_elapsed(now)
            total_text = format_duration(total) + ("*" if boundary else "")
            workdir = execution.worktree_path or task_workdir(execution.task_id, self.task_root)
            line = f"{execution.task_id[:20]:20} {execution.phase[:24]:24} {format_duration(execution.current_elapsed(now)):14} {total_text:15} {execution.agent_state[:11]:11} {(execution.agent_id or '-')[:20]:20} {compact_path(workdir, 30):30} {execution.latest_event[:24]}"
            screen.addnstr(row, 0, line[self.h_offset:self.h_offset + width - 1], width - 1, curses.A_REVERSE if self.offset + row - 2 == self.selected else 0)
        divider = rows + 2
        screen.hline(divider, 0, "-", width - 1)
        if self.executions and self.show_transcript:
            lines = transcript_lines(
                self.executions[self.selected], self.transcript_output, self.transcript_error, self.task_root,
            )
        elif self.executions:
            lines = detail_lines(self.executions[self.selected], max(1, height - divider - 3))
        else:
            lines = ["No execution events observed."]
        lines.extend(f"WARNING: {warning}" for warning in self.warnings)
        for offset, line in enumerate(lines[: height - divider - 1], start=divider + 1):
            screen.addnstr(offset, 0, line[self.h_offset:self.h_offset + width - 1], width - 1)
        screen.refresh()


def print_once(executions: list[Execution], warnings: list[str], root = None) -> None:
    print("TASK\tPHASE\tPHASE ELAPSED\tOBSERVED TOTAL\tSTATE\tAGENT ID\tDIR\tLAST EVENT\tFLAGS")
    now = dt.datetime.now(dt.timezone.utc)
    for execution in executions:
        total, boundary = execution.total_elapsed(now)
        total_text = format_duration(total) + (" (cross-clock)" if boundary else "")
        workdir = execution.worktree_path or task_workdir(execution.task_id, root)
        print("\t".join((execution.task_id, execution.phase, format_duration(execution.current_elapsed(now)), total_text, execution.agent_state, execution.agent_id or "-", workdir, execution.latest_event, flags(execution))))
    for warning in warnings:
        print(f"WARNING: {warning}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="read-only live Zigzag execution status")
    parser.add_argument("--url", default=os.environ.get("ZIGZAG_URL", "http://127.0.0.1:8765"))
    parser.add_argument("--state-file", type=Path, default=Path(os.environ.get("ZIGZAG_STATE_FILE", DEFAULT_STATE_FILE)))
    parser.add_argument("--token-file", type=Path, default=Path(os.environ.get("ZIGZAG_SECRET_FILE", DEFAULT_TOKEN_FILE)))
    parser.add_argument("--interval", type=float, default=2.0)
    parser.add_argument("--once", action="store_true", help="print one snapshot without curses")
    parser.add_argument("--all", action="store_true", help="include the relay's internal maintenance tasks")
    args = parser.parse_args(argv)
    if args.interval <= 0:
        parser.error("--interval must be positive")
    screen = StatusScreen(
        args.url, args.state_file.expanduser(), args.token_file.expanduser(), args.interval,
        include_internal=args.all,
    )
    if args.once:
        screen.bootstrap()
        print_once(screen.executions, screen.warnings, screen.task_root)
        return 0
    curses.wrapper(screen.run)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
