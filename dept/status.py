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
        agent_id = agent.get("id")
        if isinstance(agent_id, str) and agent_id:
            execution.agent_id = agent_id
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


def relay_snapshot(url: str, token_file: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]], bool, list[str]]:
    warnings: list[str] = []
    recent: list[dict[str, Any]] = []
    agents: list[dict[str, Any]] = []
    lost = False
    try:
        token = token_file.read_text().strip()
    except (OSError, UnicodeError) as error:
        return recent, agents, lost, [f"relay credentials unavailable: {error}"]
    if not token:
        return recent, agents, lost, ["relay credentials unavailable: token is empty"]
    root = url.rstrip("/")
    try:
        message = get_json(root + "/v1/events?after=0&timeout=0", token)
        values = message.get("events", [])
        if not isinstance(values, list):
            warnings.append("recent relay events unavailable: events is not an array")
        else:
            recent = [event for event in values if isinstance(event, dict) and valid_sequence(event)]
            if len(recent) != len(values):
                warnings.append("recent relay events unavailable: invalid event record")
        lost = bool(message.get("lost"))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        warnings.append(f"recent relay events unavailable: {error}")
    try:
        message = get_json(root + "/v1/agents", token)
        values = message.get("agents", [])
        if not isinstance(values, list):
            warnings.append("relay agents unavailable: agents is not an array")
        else:
            agents = [agent for agent in values if isinstance(agent, dict)]
            if len(agents) != len(values):
                warnings.append("relay agents unavailable: invalid agent record")
    except (OSError, ValueError, json.JSONDecodeError) as error:
        warnings.append(f"relay agents unavailable: {error}")
    return recent, agents, lost, warnings


def mac_dept_root() -> Path:
    """Return the configured Mac task root, not the host running this UI."""
    configured = os.environ.get("CODEX_DEPT_REMOTE_DIR")
    if not configured:
        connection = load_config().get("connection", {})
        configured = connection.get("remote_dept") if isinstance(connection, dict) else None
    return Path(configured) if isinstance(configured, str) and configured else Path("~/.codex/dept").expanduser()


def transcript_path(task_id: str, root: Path | None = None) -> Path:
    """Return the Mac-local final-message path created for every dept task."""
    return (root or mac_dept_root()) / task_id / "last-message.txt"


def transcript_command(task_id: str, root: Path | None = None) -> str:
    return f"tail -f {shlex.quote(str(transcript_path(task_id, root)))}"


def relay_output(url: str, token_file: Path, agent_id: str, *, tail: int = 12_000) -> tuple[list[str], str | None]:
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
        output: list[str] = []
        for record in records:
            if isinstance(record, dict) and isinstance(record.get("data"), str):
                output.extend(record["data"].splitlines())
        return output[-40:], None
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
    path = transcript_path(execution.task_id, root)
    lines = [
        f"Transcript for {execution.task_id} / {execution.execution_id}",
        f"Source: {path}",
        f"Command: {transcript_command(execution.task_id, root)}",
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
    ) -> None:
        self.url, self.state_file, self.token_file, self.interval = url, state_file, token_file, interval
        self.task_root = task_root or mac_dept_root()
        self.executions: list[Execution] = []
        self.warnings: list[str] = []
        self.selected = 0
        self.offset = 0
        self.show_transcript = False
        self.transcript_output: list[str] = []
        self.transcript_error: str | None = None

    def refresh(self) -> None:
        audit, audit_warnings = read_audit_events(self.state_file)
        recent, agents, lost, relay_warnings = relay_snapshot(self.url, self.token_file)
        self.executions = build_executions(merged_events(audit, recent), agents, relay_lost=lost)
        self.warnings = [*audit_warnings, *relay_warnings]
        self.selected = min(self.selected, max(len(self.executions) - 1, 0))
        self.offset = min(self.offset, self.selected)
        if self.show_transcript and self.executions:
            execution = self.executions[self.selected]
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
            self.transcript_output, self.transcript_error = [], None

    def visible_rows(self, height: int) -> int:
        return 1 if self.show_transcript else max(1, height // 2 - 2)

    def toggle_transcript(self) -> None:
        self.show_transcript = not self.show_transcript
        self.offset = self.selected
        self.transcript_output, self.transcript_error = [], None

    def run(self, screen: curses.window) -> None:
        curses.curs_set(0)
        screen.timeout(max(100, int(self.interval * 1000)))
        while True:
            self.refresh()
            self.draw(screen)
            key = screen.getch()
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

    def draw(self, screen: curses.window) -> None:
        screen.erase()
        height, width = screen.getmaxyx()
        header = "dept status — read-only  ↑↓ select  Enter/t transcript  q quit"
        if self.show_transcript:
            header = "dept status — transcript (read-only)  q/Esc back"
        screen.addnstr(0, 0, header, width - 1, curses.A_BOLD)
        columns = "TASK                 PHASE                    PHASE ELAPSED  OBSERVED TOTAL  AGENT       LAST EVENT"
        screen.addnstr(1, 0, columns, width - 1, curses.A_UNDERLINE)
        rows = self.visible_rows(height)
        for row, execution in enumerate(self.executions[self.offset:self.offset + rows], start=2):
            total, boundary = execution.total_elapsed()
            total_text = format_duration(total) + ("*" if boundary else "")
            line = f"{execution.task_id[:20]:20} {execution.phase[:24]:24} {format_duration(execution.current_elapsed()):14} {total_text:15} {execution.agent_state[:11]:11} {execution.latest_event[:24]}"
            screen.addnstr(row, 0, line, width - 1, curses.A_REVERSE if self.offset + row - 2 == self.selected else 0)
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
            screen.addnstr(offset, 0, line, width - 1)
        screen.refresh()


def print_once(executions: list[Execution], warnings: list[str]) -> None:
    print("TASK\tPHASE\tPHASE ELAPSED\tOBSERVED TOTAL\tAGENT\tLAST EVENT\tFLAGS")
    for execution in executions:
        total, boundary = execution.total_elapsed()
        total_text = format_duration(total) + (" (cross-clock)" if boundary else "")
        print("\t".join((execution.task_id, execution.phase, format_duration(execution.current_elapsed()), total_text, execution.agent_state, execution.latest_event, flags(execution))))
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
