import datetime as dt
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from dept.status import (
    StatusScreen,
    build_executions,
    detail_lines,
    duration_between,
    flags,
    merged_events,
    observed_duration,
    read_audit_events,
    relay_snapshot,
)


def event(kind, when, clock="vm:boot", **extra):
    return {
        "id": kind + when,
        "task_id": "task",
        "execution_id": "execution",
        "kind": kind,
        "occurred_at": when,
        "clock": clock,
        "source": "vm-department",
        **extra,
    }


class PhaseDurationTests(unittest.TestCase):
    def test_same_clock_adjacent_events_are_measured(self):
        left = event("review_wait_started", "2026-01-01T00:00:00.000Z")
        right = event("review_work_started", "2026-01-01T00:00:03.250Z")
        self.assertEqual(duration_between(left, right), dt.timedelta(seconds=3, milliseconds=250))
        self.assertEqual(observed_duration([left, right]), (dt.timedelta(seconds=3, milliseconds=250), False))

    def test_cross_clock_handoff_is_rendered_but_not_subtracted(self):
        left = event("task_dispatched", "2026-01-01T00:00:00.000Z", "vm:boot")
        right = event("relay_accepted", "2026-01-01T00:00:01.000Z", "mac:boot")
        execution = build_executions([left, right], [], relay_lost=False)[0]
        self.assertIsNone(execution.current_elapsed())
        self.assertEqual(execution.total_elapsed(), (None, True))
        self.assertTrue(any("cross-clock" in line and "not subtracted" in line for line in detail_lines(execution)))

    def test_missing_pair_means_not_observed(self):
        execution = build_executions([event("human_wait_started", "2026-01-01T00:00:00.000Z")], [])[0]
        self.assertIsNone(execution.current_elapsed())
        self.assertEqual(execution.total_elapsed(), (None, False))

    def test_invalid_or_backwards_same_clock_time_is_not_a_clock_boundary(self):
        left = event("review_wait_started", "2026-01-01T00:00:03.000Z", received_at="2026-01-01T00:00:00.000Z")
        backwards = event("review_work_started", "2026-01-01T00:00:00.000Z", received_at="2026-01-01T00:00:01.000Z")
        invalid = event("human_wait_ended", "not-a-timestamp", received_at="2026-01-01T00:00:02.000Z")
        execution = build_executions([left, backwards, invalid], [])[0]
        self.assertEqual(execution.total_elapsed(), (None, False))
        lines = detail_lines(execution)
        self.assertTrue(any("timing not observed" in line for line in lines))
        self.assertFalse(any("cross-clock" in line for line in lines))


class SnapshotIngestionTests(unittest.TestCase):
    def test_partial_audit_line_retains_other_events(self):
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "events.json"
            audit = state_file.with_suffix(".audit")
            audit.mkdir()
            (audit / "execution.jsonl").write_text(json.dumps(event("task_dispatched", "2026-01-01T00:00:00.000Z")) + "\n{partial")
            events, warnings = read_audit_events(state_file)
        self.assertEqual([item["kind"] for item in events], ["task_dispatched"])
        self.assertTrue(any("degraded audit log" in warning for warning in warnings))

    def test_missing_token_degrades_to_audit_only_snapshot(self):
        recent, agents, lost, warnings = relay_snapshot("http://relay", Path("/not/a/token"))
        self.assertEqual((recent, agents, lost), ([], [], False))
        self.assertTrue(any("credentials unavailable" in warning for warning in warnings))

    def test_relay_snapshot_reads_live_events_and_running_agents(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            with patch(
                "dept.status.get_json",
                side_effect=[
                    {"events": [event("process_spawned", "2026-01-01T00:00:00.000Z")], "lost": True},
                    {"agents": [{"execution_id": "execution", "state": "running"}]},
                ],
            ) as get_json:
                recent, agents, lost, warnings = relay_snapshot("http://relay/", token)
        self.assertEqual([item["kind"] for item in recent], ["process_spawned"])
        self.assertEqual(agents[0]["state"], "running")
        self.assertTrue(lost)
        self.assertEqual(warnings, [])
        self.assertEqual(
            [call.args[0] for call in get_json.call_args_list],
            ["http://relay/v1/events?after=0&timeout=0", "http://relay/v1/agents?state=running"],
        )

    def test_agent_flags_relay_loss_and_duplicate_events_are_merged(self):
        first = event("task_dispatched", "2026-01-01T00:00:00.000Z", id="same")
        latest = event("process_spawned", "2026-01-01T00:00:01.000Z", id="same")
        merged = merged_events([first], [latest])
        execution = build_executions(
            merged,
            [{"execution_id": "agent-only", "task_id": "other", "state": "running", "audit_degraded": True, "log_degraded": True}],
            relay_lost=True,
        )
        self.assertEqual(len(merged), 1)
        agent_only = next(item for item in execution if item.execution_id == "agent-only")
        self.assertIn("audit degraded", flags(agent_only))
        self.assertIn("agent log degraded", flags(agent_only))
        self.assertIn("relay events lost", flags(agent_only))

    def test_status_refresh_keeps_audit_events_when_relay_requests_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "events.json"
            audit = state_file.with_suffix(".audit")
            audit.mkdir()
            (audit / "execution.jsonl").write_text(json.dumps(event("task_dispatched", "2026-01-01T00:00:00.000Z")) + "\n")
            token = Path(directory) / "token"
            token.write_text("token")
            with patch("dept.status.get_json", side_effect=OSError("relay down")):
                screen = StatusScreen("http://relay", state_file, token, 1)
                screen.refresh()
        self.assertEqual([item.execution_id for item in screen.executions], ["execution"])
        self.assertTrue(any("unavailable" in warning for warning in screen.warnings))


if __name__ == "__main__":
    unittest.main()
