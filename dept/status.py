"""Read-only, Mac-local execution state model and curses renderer for dept."""

from __future__ import annotations

import argparse
import curses
import datetime as dt
import json
import os
import sys
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

DEFAULT_STATE_FILE = Path("~/.codex/zigzag/events.json").expanduser()
DEFAULT_TOKEN_FILE = Path("~/.codex/zigzag/zigzag.token").expanduser()


def parse_time(value: object) -> dt.datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed if parsed.tzinfo is not None else None
    except ValueError:
        return None


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
        return (
            str(event.get("received_at") or event.get("occurred_at") or ""),
            int(event.get("sequence") or 0),
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
    degraded: list[str] = field(default_factory=list)
    relay_lost: bool = False

    @property
    def phase(self) -> str:
        return str(self.events[-1].get("kind", "not observed")) if self.events else "not observed"

    @property
    def latest_event(self) -> str:
        if not self.events:
            return "not observed"
        return str(self.events[-1].get("occurred_at") or self.events[-1].get("received_at") or "not observed")

    def current_elapsed(self) -> dt.timedelta | None:
        if len(self.events) < 2:
            return None
        return duration_between(self.events[-2], self.events[-1])

    def total_elapsed(self) -> tuple[dt.timedelta | None, bool]:
        return observed_duration(self.events)


def build_executions(
    events: Iterable[dict[str, Any]], agents: Iterable[dict[str, Any]], *, relay_lost: bool = False,
) -> list[Execution]:
    grouped: dict[str, Execution] = {}
    for event in sort_events(events):
        execution_id = execution_key(event)
        if execution_id is None:
            continue
        execution = grouped.setdefault(execution_id, Execution(execution_id))
        execution.task_id = str(event.get("task_id") or execution.task_id)
        execution.events.append(event)
    for agent in agents:
        execution_id = agent.get("execution_id")
        if not isinstance(execution_id, str) or not execution_id:
            continue
        execution = grouped.setdefault(execution_id, Execution(execution_id, str(agent.get("task_id") or "?")))
        execution.agent_state = str(agent.get("state") or "not observed")
        if agent.get("audit_degraded"):
            execution.degraded.append("audit degraded")
        if agent.get("log_degraded"):
            execution.degraded.append("agent log degraded")
    for execution in grouped.values():
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


def read_audit_events(state_file: Path) -> tuple[list[dict[str, Any]], list[str]]:
    events: list[dict[str, Any]] = []
    warnings: list[str] = []
    directory = audit_directory(state_file)
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
            if isinstance(decoded, dict):
                events.append(decoded)
            else:
                warnings.append(f"degraded audit log {path.name}:{line_number}: event is not an object")
    return events, warnings


def get_json(url: str, token: str) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}", "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=10) as response:
        decoded = json.loads(response.read())
    if not isinstance(decoded, dict):
        raise ValueError("relay returned a non-object JSON response")
    return decoded


def relay_snapshot(url: str, token_file: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]], bool, list[str]]:
    warnings: list[str] = []
    recent: list[dict[str, Any]] = []
    agents: list[dict[str, Any]] = []
    lost = False
    try:
        token = token_file.read_text().strip()
    except OSError as error:
        return recent, agents, lost, [f"relay credentials unavailable: {error}"]
    if not token:
        return recent, agents, lost, ["relay credentials unavailable: token is empty"]
    root = url.rstrip("/")
    try:
        message = get_json(root + "/v1/events?after=0&timeout=0", token)
        recent = [event for event in message.get("events", []) if isinstance(event, dict)]
        lost = bool(message.get("lost"))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        warnings.append(f"recent relay events unavailable: {error}")
    try:
        message = get_json(root + "/v1/agents?state=running", token)
        agents = [agent for agent in message.get("agents", []) if isinstance(agent, dict)]
    except (OSError, ValueError, json.JSONDecodeError) as error:
        warnings.append(f"running agents unavailable: {error}")
    return recent, agents, lost, warnings


def merged_events(audit: Iterable[dict[str, Any]], recent: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    unique: dict[str, dict[str, Any]] = {}
    anonymous: list[dict[str, Any]] = []
    for event in list(audit) + list(recent):
        event_id = event.get("id")
        if isinstance(event_id, str) and event_id:
            unique[event_id] = event
        else:
            anonymous.append(event)
    return sort_events([*unique.values(), *anonymous])


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


class StatusScreen:
    def __init__(self, url: str, state_file: Path, token_file: Path, interval: float) -> None:
        self.url, self.state_file, self.token_file, self.interval = url, state_file, token_file, interval
        self.executions: list[Execution] = []
        self.warnings: list[str] = []
        self.selected = 0

    def refresh(self) -> None:
        audit, audit_warnings = read_audit_events(self.state_file)
        recent, agents, lost, relay_warnings = relay_snapshot(self.url, self.token_file)
        self.executions = build_executions(merged_events(audit, recent), agents, relay_lost=lost)
        self.warnings = [*audit_warnings, *relay_warnings]
        self.selected = min(self.selected, max(len(self.executions) - 1, 0))

    def run(self, screen: curses.window) -> None:
        curses.curs_set(0)
        screen.timeout(max(100, int(self.interval * 1000)))
        while True:
            self.refresh()
            self.draw(screen)
            key = screen.getch()
            if key in (ord("q"), 27):
                return
            if key in (curses.KEY_UP, ord("k")):
                self.selected = max(0, self.selected - 1)
            if key in (curses.KEY_DOWN, ord("j")):
                self.selected = min(max(len(self.executions) - 1, 0), self.selected + 1)

    def draw(self, screen: curses.window) -> None:
        screen.erase()
        height, width = screen.getmaxyx()
        header = "dept status — read-only  ↑↓ select  q quit"
        screen.addnstr(0, 0, header, width - 1, curses.A_BOLD)
        columns = "TASK                 PHASE                    PHASE ELAPSED  OBSERVED TOTAL  AGENT       LAST EVENT"
        screen.addnstr(1, 0, columns, width - 1, curses.A_UNDERLINE)
        rows = max(1, height // 2 - 2)
        for row, execution in enumerate(self.executions[:rows], start=2):
            total, boundary = execution.total_elapsed()
            total_text = format_duration(total) + ("*" if boundary else "")
            line = f"{execution.task_id[:20]:20} {execution.phase[:24]:24} {format_duration(execution.current_elapsed()):14} {total_text:15} {execution.agent_state[:11]:11} {execution.latest_event()[:24]}"
            screen.addnstr(row, 0, line, width - 1, curses.A_REVERSE if row - 2 == self.selected else 0)
        divider = rows + 2
        screen.hline(divider, 0, "-", width - 1)
        if self.executions:
            lines = detail_lines(self.executions[self.selected], max(1, height - divider - 3))
        else:
            lines = ["No execution events observed."]
        lines.extend(f"WARNING: {warning}" for warning in self.warnings)
        for offset, line in enumerate(lines[: height - divider - 1], start=divider + 1):
            screen.addnstr(offset, 0, line, width - 1)
        screen.refresh()


def print_once(executions: list[Execution], warnings: list[str]) -> None:
    print("TASK\tPHASE\tPHASE ELAPSED\tOBSERVED TOTAL\tAGENT\tLAST EVENT\tFLAGS")
    for execution in executions:
        total, boundary = execution.total_elapsed()
        total_text = format_duration(total) + (" (cross-clock)" if boundary else "")
        print("\t".join((execution.task_id, execution.phase, format_duration(execution.current_elapsed()), total_text, execution.agent_state, execution.latest_event(), flags(execution))))
    for warning in warnings:
        print(f"WARNING: {warning}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="read-only live Zigzag execution status")
    parser.add_argument("--url", default=os.environ.get("ZIGZAG_URL", "http://127.0.0.1:8765"))
    parser.add_argument("--state-file", type=Path, default=Path(os.environ.get("ZIGZAG_STATE_FILE", DEFAULT_STATE_FILE)))
    parser.add_argument("--token-file", type=Path, default=Path(os.environ.get("ZIGZAG_SECRET_FILE", DEFAULT_TOKEN_FILE)))
    parser.add_argument("--interval", type=float, default=2.0)
    parser.add_argument("--once", action="store_true", help="print one snapshot without curses")
    args = parser.parse_args(argv)
    if args.interval <= 0:
        parser.error("--interval must be positive")
    screen = StatusScreen(args.url, args.state_file.expanduser(), args.token_file.expanduser(), args.interval)
    if args.once:
        screen.refresh()
        print_once(screen.executions, screen.warnings)
        return 0
    curses.wrapper(screen.run)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
