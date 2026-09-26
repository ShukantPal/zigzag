import datetime as dt
import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

from dept import dept
from dept.status import (
    Execution,
    StatusScreen,
    build_executions,
    detail_lines,
    duration_between,
    flags,
    merged_events,
    observed_duration,
    read_audit_events,
    relay_snapshot,
    main as status_main,
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

    def test_missing_audit_directory_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            events, warnings = read_audit_events(Path(directory) / "events.json")
        self.assertEqual(events, [])
        self.assertTrue(any("audit directory unavailable" in warning for warning in warnings))

    def test_bad_sequence_is_skipped_without_losing_valid_audit_events(self):
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "events.json"
            audit = state_file.with_suffix(".audit")
            audit.mkdir()
            bad = event("process_spawned", "2026-01-01T00:00:01.000Z", sequence="bad")
            valid = event("task_dispatched", "2026-01-01T00:00:00.000Z", sequence=1)
            (audit / "execution.jsonl").write_text(json.dumps(bad) + "\n" + json.dumps(valid) + "\n")
            events, warnings = read_audit_events(state_file)
        self.assertEqual([item["kind"] for item in events], ["task_dispatched"])
        self.assertTrue(any("sequence is not numeric" in warning for warning in warnings))

    def test_missing_token_degrades_to_audit_only_snapshot(self):
        recent, agents, lost, warnings = relay_snapshot("http://relay", Path("/not/a/token"))
        self.assertEqual((recent, agents, lost), ([], [], False))
        self.assertTrue(any("credentials unavailable" in warning for warning in warnings))

    def test_invalid_token_encoding_degrades_to_audit_only_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_bytes(b"\xff")
            recent, agents, lost, warnings = relay_snapshot("http://relay", token)
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

    def test_malformed_relay_shapes_warn_without_aborting(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            with patch("dept.status.get_json", side_effect=[{"events": None}, {"agents": None}]):
                recent, agents, lost, warnings = relay_snapshot("http://relay", token)
        self.assertEqual((recent, agents, lost), ([], [], False))
        self.assertEqual(len(warnings), 2)

    def test_agent_flags_relay_loss_and_duplicate_events_are_merged(self):
        first = event("task_dispatched", "2026-01-01T00:00:00.000Z", id="same")
        latest = event("process_spawned", "2026-01-01T00:00:01.000Z", id="same")
        merged = merged_events([first], [latest])
        execution = build_executions(
            merged,
            [{"execution_id": "execution", "task_id": "task", "state": "running", "audit_degraded": True, "log_degraded": True}],
            relay_lost=True,
        )
        self.assertEqual(len(merged), 1)
        self.assertEqual(merged[0]["kind"], "process_spawned")
        merged_execution = execution[0]
        self.assertEqual(merged_execution.agent_state, "running")
        self.assertIn("audit degraded", flags(merged_execution))
        self.assertIn("agent log degraded", flags(merged_execution))
        self.assertIn("relay events lost", flags(merged_execution))

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

    def test_selection_scrolls_into_the_visible_rows(self):
        screen = StatusScreen("http://relay", Path("events.json"), Path("token"), 1)
        screen.executions = [Execution(str(number)) for number in range(4)]
        screen.move_selection(3, 2)
        self.assertEqual((screen.selected, screen.offset), (3, 2))
        screen.move_selection(-3, 2)
        self.assertEqual((screen.selected, screen.offset), (0, 0))


class CommandTests(unittest.TestCase):
    def test_dept_entry_point_forwards_status_and_rejects_unknown_commands(self):
        with patch("dept.dept.status_main", return_value=17) as status:
            self.assertEqual(dept.main(["status", "--once"]), 17)
        status.assert_called_once_with(["--once"])
        with redirect_stderr(io.StringIO()):
            self.assertEqual(dept.main(["unknown"]), 2)

    def test_status_once_prints_a_snapshot_and_interval_validation_rejects_zero(self):
        def refresh(screen):
            screen.executions = [Execution("execution", "task", [event("process_spawned", "2026-01-01T00:00:00.000Z")])]
            screen.warnings = ["relay unavailable"]

        stdout, stderr = io.StringIO(), io.StringIO()
        with patch("dept.status.StatusScreen.refresh", refresh), redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(status_main(["--once"]), 0)
        self.assertIn("TASK\tPHASE", stdout.getvalue())
        self.assertIn("process_spawned", stdout.getvalue())
        self.assertIn("WARNING: relay unavailable", stderr.getvalue())
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            status_main(["--once", "--interval", "0"])
        self.assertEqual(error.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
