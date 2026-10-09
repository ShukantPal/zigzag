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
    EventStream,
    Execution,
    RELAY_OUTPUT_TAIL_BYTES,
    StatusScreen,
    agent_worktrees,
    build_executions,
    compact_path,
    detail_lines,
    dept_task_id,
    duration_between,
    flags,
    mac_dept_root,
    merged_events,
    observed_duration,
    read_audit_events,
    relay_output,
    main as status_main,
    task_workdir,
    transcript_command,
    transcript_lines,
    transcript_path,
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


class FakeScreen:
    def __init__(self, keys: list[int], height: int = 12, width: int = 120):
        self.keys = iter(keys)
        self.height, self.width = height, width
        self.frame: list[str] = []
        self.frames: list[list[str]] = []

    def timeout(self, _value):
        pass

    def getmaxyx(self):
        return self.height, self.width

    def erase(self):
        self.frame = []

    def addnstr(self, _row, _column, text, _width, _style=0):
        self.frame.append(text)

    def hline(self, *_args):
        pass

    def refresh(self):
        self.frames.append(self.frame[:])

    def getch(self):
        return next(self.keys)


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

    def test_api_agent_task_id_and_status_render_without_lifecycle_events(self):
        agents = [{
            "id": "agent-handle-123",
            "task_id": "agent-4c4d2e1a",
            # Older retained rows may not have execution_id.  API-created
            # agent task ids still need a useful live status row.
            "state": "running",
            "started_at": "2026-01-01T00:00:00Z",
            "worktree_path": "/worktrees/api-agent",
        }]
        execution = build_executions([], agents)[0]
        self.assertEqual(execution.task_id, "agent-4c4d2e1a")
        self.assertEqual(execution.agent_id, "agent-handle-123")
        self.assertEqual(execution.agent_state, "running")
        self.assertEqual(execution.phase, "agent running")

        screen = StatusScreen("http://relay", Path("events.json"), Path("token"), 1)
        screen.executions = [execution]
        fake = FakeScreen([], width=180)
        screen.draw(fake)
        row = "\n".join(fake.frames[-1])
        self.assertIn("agent-4c4d2e1a", row)
        self.assertIn("agent-handle-123", row)
        self.assertIn("/worktrees/api-agent", row)

    def test_api_agent_uses_worktree_from_local_relay_registry(self):
        agent = {
            "id": "agent-handle-123",
            "task_id": "agent-4c4d2e1a",
            "state": "running",
        }
        execution = build_executions(
            [], [agent],
            persisted_agent_worktrees={"agent-4c4d2e1a": "/private/tmp/fix-pr69-ci"},
        )[0]
        self.assertEqual(execution.worktree_path, "/private/tmp/fix-pr69-ci")

    def test_event_only_api_agent_uses_worktree_from_local_relay_registry(self):
        api_event = event("process_spawned", "2026-01-01T00:00:00.000Z",
                          task_id="agent-4c4d2e1a", execution_id="relay-attempt-123")
        execution = build_executions(
            [api_event], [],
            persisted_agent_worktrees={"agent-4c4d2e1a": "/private/tmp/fix-pr69-ci"},
        )[0]
        self.assertEqual(execution.worktree_path, "/private/tmp/fix-pr69-ci")

    def test_compact_path_preserves_the_right_end(self):
        self.assertEqual(
            compact_path("/Users/shukant/Workspace/ShukantPal/zigzag/fix-pr69-ci", 22),
            ".../zigzag/fix-pr69-ci",
        )

    def test_running_elapsed_uses_started_at_on_each_render(self):
        execution = build_executions([], [{
            "id": "agent-handle-123",
            "task_id": "agent-4c4d2e1a",
            "execution_id": "relay-attempt-123",
            "state": "running",
            # The live /v1/agents endpoint sends Unix seconds as a string.
            "started_at": "1767225600",
        }])[0]
        first = dt.datetime(2026, 1, 1, 0, 0, 5, tzinfo=dt.timezone.utc)
        second = dt.datetime(2026, 1, 1, 0, 0, 8, tzinfo=dt.timezone.utc)
        self.assertEqual(execution.current_elapsed(first), dt.timedelta(seconds=5))
        self.assertEqual(execution.current_elapsed(second), dt.timedelta(seconds=8))
        self.assertEqual(execution.total_elapsed(second), (dt.timedelta(seconds=8), False))


class SnapshotIngestionTests(unittest.TestCase):
    def test_agent_worktrees_reads_local_relay_registry(self):
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "events.json"
            state_file.with_suffix(".agents.json").write_text(json.dumps({"agents": [
                {"id": "agent-handle-123", "task_id": "agent-4c4d2e1a", "worktree_path": "/private/tmp/fix-pr69-ci"},
                {"id": "missing-path"},
            ]}))
            self.assertEqual(
                agent_worktrees(state_file),
                {"agent-4c4d2e1a": "/private/tmp/fix-pr69-ci"},
            )

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
        screen = StatusScreen("http://relay", Path("events.json"), Path("/not/a/token"), 1)
        screen.bootstrap()
        self.assertEqual(screen.executions, [])
        self.assertTrue(any("credentials unavailable" in warning for warning in screen.warnings))

    def test_invalid_token_encoding_degrades_to_audit_only_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_bytes(b"\xff")
            screen = StatusScreen("http://relay", Path("events.json"), token, 1)
            screen.bootstrap()
        self.assertEqual(screen.executions, [])
        self.assertTrue(any("credentials unavailable" in warning for warning in screen.warnings))

    def test_event_stream_bootstrap_reads_live_events_and_running_agents(self):
        stream = EventStream("http://relay/", "token")
        with patch.object(
            EventStream,
            "_get",
            side_effect=[
                {
                    "epoch": "e1", "reset": False, "lost": True, "next": 3,
                    "events": [event("process_spawned", "2026-01-01T00:00:00.000Z")],
                },
                {"agents": [{"id": "agent", "execution_id": "execution", "state": "running"}]},
            ],
        ) as get:
            stream.bootstrap()
        self.assertEqual([item["kind"] for item in stream.events], ["process_spawned"])
        self.assertEqual(stream.agents["agent"]["state"], "running")
        self.assertTrue(stream.lost)
        self.assertEqual((stream.epoch, stream.after), ("e1", 3))
        self.assertEqual(
            [call.args[0] for call in get.call_args_list],
            ["/v1/events?after=0&timeout=0", "/v1/agents"],
        )

    def test_malformed_relay_shapes_warn_without_aborting(self):
        stream = EventStream("http://relay", "token")
        with patch.object(
            EventStream, "_get",
            side_effect=[{"epoch": "e1", "events": None, "next": 0}, {"agents": None}],
        ):
            stream.bootstrap()
        self.assertEqual((stream.events, stream.agents, stream.lost), ([], {}, False))
        self.assertEqual(len(stream.poll_warnings), 2)

    def test_event_stream_poll_long_polls_with_cursor_and_folds_new_events(self):
        stream = EventStream("http://relay", "token")
        stream.epoch, stream.after = "e1", 7
        spawned = event("process_spawned", "2026-01-01T00:00:00.000Z", sequence=8)
        spawned["payload"] = {"agent_id": "agent-1"}
        completed = event("process_completed", "2026-01-01T00:00:01.000Z", sequence=9)
        completed["payload"] = {"agent_id": "agent-1", "state": "succeeded", "exit_code": 0}
        with patch.object(
            EventStream, "_get",
            return_value={"epoch": "e1", "reset": False, "lost": False, "next": 10,
                          "events": [spawned, completed]},
        ) as get:
            changed = stream.poll(25)
        self.assertTrue(changed)
        self.assertEqual(
            get.call_args.args[0],
            "/v1/events?after=7&epoch=e1&timeout=25",
        )
        self.assertEqual((stream.epoch, stream.after), ("e1", 10))
        self.assertEqual(stream.agents["agent-1"]["state"], "succeeded")
        self.assertEqual(stream.agents["agent-1"]["exit_code"], 0)

    def test_event_stream_poll_reports_no_change_on_empty_batch(self):
        stream = EventStream("http://relay", "token")
        stream.epoch, stream.after = "e1", 7
        with patch.object(
            EventStream, "_get",
            return_value={"epoch": "e1", "reset": False, "lost": False, "next": 7, "events": []},
        ):
            self.assertFalse(stream.poll(25))

    def test_event_stream_resets_on_epoch_roll(self):
        stream = EventStream("http://relay", "token")
        stream.epoch, stream.after = "old", 99
        stream.events = [{"id": "stale"}]
        stream.agents = {"gone": {"id": "gone"}}
        fresh = event("process_spawned", "2026-01-01T00:00:00.000Z", sequence=1)
        with patch.object(
            EventStream, "_get",
            side_effect=[
                {"epoch": "new", "reset": True, "lost": False, "next": 2, "events": [fresh]},
                {"agents": [{"id": "agent", "execution_id": "execution", "state": "running"}]},
            ],
        ):
            self.assertTrue(stream.poll(25))
        self.assertEqual((stream.epoch, stream.after), ("new", 2))
        self.assertEqual([item["kind"] for item in stream.events], ["process_spawned"])
        self.assertEqual(set(stream.agents), {"agent"})

    def test_agent_flags_relay_loss_and_duplicate_events_are_merged(self):
        first = event("task_dispatched", "2026-01-01T00:00:00.000Z", id="same")
        latest = event("process_spawned", "2026-01-01T00:00:01.000Z", id="same")
        merged = merged_events([first], [latest])
        execution = build_executions(
            merged,
            [{"id": "agent", "execution_id": "execution", "task_id": "task", "state": "running", "audit_degraded": True, "log_degraded": True}],
            relay_lost=True,
        )
        self.assertEqual(len(merged), 1)
        self.assertEqual(merged[0]["kind"], "process_spawned")
        merged_execution = execution[0]
        self.assertEqual(merged_execution.agent_state, "running")
        self.assertIn("audit degraded", flags(merged_execution))
        self.assertIn("agent log degraded", flags(merged_execution))
        self.assertIn("relay events lost", flags(merged_execution))
        self.assertEqual(merged_execution.agent_id, "agent")

    def test_status_bootstrap_keeps_audit_events_when_relay_requests_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "events.json"
            audit = state_file.with_suffix(".audit")
            audit.mkdir()
            (audit / "execution.jsonl").write_text(json.dumps(event("task_dispatched", "2026-01-01T00:00:00.000Z")) + "\n")
            token = Path(directory) / "token"
            token.write_text("token")
            with patch.object(EventStream, "_get", side_effect=OSError("relay down")):
                screen = StatusScreen("http://relay", state_file, token, 1)
                screen.bootstrap()
        self.assertEqual([item.execution_id for item in screen.executions], ["execution"])
        self.assertTrue(any("unavailable" in warning for warning in screen.warnings))

    def test_selection_scrolls_into_the_visible_rows(self):
        screen = StatusScreen("http://relay", Path("events.json"), Path("token"), 1)
        screen.executions = [Execution(str(number)) for number in range(4)]
        screen.move_selection(3, 2)
        self.assertEqual((screen.selected, screen.offset), (3, 2))
        screen.move_selection(-3, 2)
        self.assertEqual((screen.selected, screen.offset), (0, 0))


class TranscriptTests(unittest.TestCase):
    def test_transcript_source_and_command_use_the_mac_dept_layout(self):
        self.assertEqual(
            transcript_path("t-example"),
            Path.home() / ".codex/dept/t-example/last-message.txt",
        )
        self.assertEqual(
            transcript_command("t-example"),
            f"tail -F {Path.home() / '.codex/dept/t-example/last-message.txt'}",
        )

    def test_transcript_source_uses_configured_mac_task_root(self):
        with patch("dept.status.load_config", return_value={"connection": {"remote_dept": "/Users/mac/.codex/dept"}}):
            root = mac_dept_root()
        self.assertEqual(root, Path("/Users/mac/.codex/dept"))
        self.assertEqual(transcript_path("t-example", root), root / "t-example/last-message.txt")

    def test_relay_output_reads_bounded_spool_and_keeps_last_40_lines(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            text = "".join(f"line {number}\n" for number in range(45))
            records = [
                {"data": text[:17]},
                {"data": text[17:101]},
                {"data": text[101:]},
            ]
            with patch("dept.status.get_json", return_value={"records": records}) as get_json:
                lines, error = relay_output("http://relay/", token, "agent id")
        self.assertIsNone(error)
        self.assertEqual(lines, [f"line {number}" for number in range(5, 45)])
        self.assertEqual(
            get_json.call_args.args[0],
            f"http://relay/v1/agents/agent%20id/logs?stream=both&tail={RELAY_OUTPUT_TAIL_BYTES}&follow=0",
        )

    def test_transcript_detail_shows_path_command_and_spool(self):
        execution = Execution("execution", "t-example", agent_id="agent")
        lines = transcript_lines(execution, ["first", "second"], None)
        self.assertIn(f"Source: {transcript_path('t-example')}", lines)
        self.assertIn(f"Command: {transcript_command('t-example')}", lines)
        self.assertEqual(lines[-3:], ["Relay output (last 40 lines):", "first", "second"])

    def test_relay_output_degradations_replace_stale_transcript_content(self):
        execution = Execution("execution", "t-example", agent_id="agent")
        missing, error = relay_output("http://relay", Path("/not/a/token"), "agent")
        self.assertEqual(missing, [])
        self.assertIn("credentials unavailable", error)
        self.assertEqual(transcript_lines(execution, ["stale"], error)[-1], error)

        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            with patch("dept.status.get_json", side_effect=OSError("relay down")):
                output, error = relay_output("http://relay", token, "agent")
            self.assertEqual(output, [])
            self.assertIn("relay output unavailable", error)
            self.assertEqual(transcript_lines(execution, ["stale"], error)[-1], error)

            with patch("dept.status.get_json", return_value={"records": None}):
                output, error = relay_output("http://relay", token, "agent")
            self.assertEqual(output, [])
            self.assertEqual(error, "relay output unavailable: records is not an array")
            self.assertEqual(transcript_lines(execution, ["stale"], error)[-1], error)

            with patch("dept.status.get_json", return_value={"records": []}):
                output, error = relay_output("http://relay", token, "agent")
            self.assertEqual((output, error), ([], None))
            self.assertEqual(transcript_lines(execution, output, error)[-1], "Relay output: no spool output yet.")

    def test_transcript_mode_fetches_selected_spool_and_keeps_selected_row_visible(self):
        events = [
            event("first", "2026-01-01T00:00:00.000Z", task_id="task-zero", execution_id="zero"),
            event("second", "2026-01-01T00:00:01.000Z", task_id="task-one", execution_id="one"),
        ]
        agents = [
            {"id": "agent-zero", "execution_id": "zero", "task_id": "task-zero", "state": "running"},
            {"id": "agent-one", "execution_id": "one", "task_id": "task-one", "state": "running"},
        ]
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            screen = StatusScreen("http://relay", Path("events.json"), token, 1, Path("/Mac/dept"))
            fake = FakeScreen([10, ord("j"), ord("q"), 10, 27, ord("q")])
            with patch("dept.status.curses.curs_set"), \
                 patch("dept.status.read_audit_events", return_value=(events, [])), \
                 patch("dept.status.EventStream") as stream_cls, \
                 patch("dept.status.relay_output", side_effect=[(["one output"], None), (["zero output"], None), (["zero output"], None)]) as output:
                stream = stream_cls.return_value
                stream.events = []
                stream.agents = {agent["id"]: agent for agent in agents}
                stream.lost = False
                stream.poll_warnings = []
                stream.poll.return_value = False
                screen.run(fake)
        self.assertEqual([call.args[2] for call in output.call_args_list], ["agent-one", "agent-zero", "agent-zero"])
        one_output = next("\n".join(frame) for frame in fake.frames if "one output" in "\n".join(frame))
        zero_output = next("\n".join(frame) for frame in fake.frames if "zero output" in "\n".join(frame))
        self.assertIn("task-one", one_output)
        self.assertIn("task-zero", zero_output)
        self.assertTrue(any("read-only  \u2191\u2193 select" in "\n".join(frame) for frame in fake.frames[3:]))
        self.assertEqual((screen.selected, screen.offset, screen.show_transcript), (1, 1, False))

class CommandTests(unittest.TestCase):
    def test_dept_entry_point_forwards_status_and_rejects_unknown_commands(self):
        with patch("dept.dept.status_main", return_value=17) as status:
            self.assertEqual(dept.main(["status", "--once"]), 17)
        status.assert_called_once_with(["--once"])
        with redirect_stderr(io.StringIO()):
            self.assertEqual(dept.main(["unknown"]), 2)

    def test_status_once_prints_a_snapshot_and_interval_validation_rejects_zero(self):
        def bootstrap(screen):
            screen.executions = [Execution("execution", "task", [event("process_spawned", "2026-01-01T00:00:00.000Z")])]
            screen.warnings = ["relay unavailable"]

        stdout, stderr = io.StringIO(), io.StringIO()
        with patch("dept.status.StatusScreen.bootstrap", bootstrap), redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(status_main(["--once"]), 0)
        self.assertIn("TASK\tPHASE", stdout.getvalue())
        self.assertIn("process_spawned", stdout.getvalue())
        self.assertIn("WARNING: relay unavailable", stderr.getvalue())
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            status_main(["--once", "--interval", "0"])
        self.assertEqual(error.exception.code, 2)

if __name__ == "__main__":
    unittest.main()


class RelayIdMappingTests(unittest.TestCase):
    def test_dept_task_id_strips_relay_spawn_prefix(self):
        self.assertEqual(dept_task_id("codex-t-abc123"), "t-abc123")
        self.assertEqual(dept_task_id("t-abc123"), "t-abc123")
        self.assertEqual(dept_task_id("relay-update"), "relay-update")

    def test_task_workdir_resolves_relay_spawn_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "t-abc123").mkdir()
            (root / "t-abc123" / "dir.txt").write_text("/work/tree\n")
            self.assertEqual(task_workdir("codex-t-abc123", root), "/work/tree")
            self.assertEqual(task_workdir("t-abc123", root), "/work/tree")
            self.assertEqual(task_workdir("codex-t-missing", root), "")

    def test_transcript_path_strips_relay_spawn_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(
                transcript_path("codex-t-abc123", root),
                root / "t-abc123" / "last-message.txt",
            )


class InternalTaskFilterTests(unittest.TestCase):
    def test_relay_update_hidden_from_default_view(self):
        update = event("relay_update_check_started", "2026-01-01T00:00:00.000Z",
                       task_id="relay-update", execution_id="update-1")
        user = event("process_spawned", "2026-01-01T00:00:01.000Z",
                     task_id="t-one", execution_id="exec-1")
        executions = build_executions([update, user], [])
        self.assertEqual([execution.task_id for execution in executions], ["t-one"])

    def test_relay_update_visible_with_all_flag(self):
        update = event("relay_update_check_started", "2026-01-01T00:00:00.000Z",
                       task_id="relay-update", execution_id="update-1")
        agents = [{"id": "agent", "execution_id": "update-1", "task_id": "relay-update", "state": "running"}]
        executions = build_executions([update], agents, include_internal=True)
        self.assertEqual([execution.task_id for execution in executions], ["relay-update"])
        self.assertEqual(executions[0].agent_state, "running")


class StreamUpdateTests(unittest.TestCase):
    def test_idle_long_poll_expiry_redraws_running_elapsed_time(self):
        screen = StatusScreen("http://relay", Path("events.json"), Path("token"), 1)
        screen.executions = [Execution(
            "execution", "agent-task", agent_state="running",
            agent_started_at=dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
        )]
        screen.bootstrap = lambda: None
        fake = FakeScreen([ord("q")])
        with patch("dept.status.curses.curs_set"), patch.object(screen, "update", return_value=False) as update:
            screen.run(fake)
        self.assertEqual(update.call_count, 1)
        # One initial frame plus a frame after the eventless long-poll tick.
        self.assertEqual(len(fake.frames), 2)

    def test_update_rebuilds_only_on_pushed_events(self):
        with tempfile.TemporaryDirectory() as directory:
            token = Path(directory) / "token"
            token.write_text("token")
            screen = StatusScreen("http://relay", Path("events.json"), token, 1)
            with patch("dept.status.read_audit_events", return_value=([], [])), \
                 patch("dept.status.EventStream") as stream_cls:
                stream = stream_cls.return_value
                stream.events = []
                stream.agents = {}
                stream.lost = False
                stream.poll_warnings = []
                stream.poll.return_value = False
                screen.bootstrap()
                self.assertEqual(screen.executions, [])
                self.assertFalse(screen.update())
                stream.events = [event("process_spawned", "2026-01-01T00:00:00.000Z")]
                stream.poll.return_value = True
                self.assertTrue(screen.update())
                self.assertEqual([execution.task_id for execution in screen.executions], ["task"])
