import importlib
import json
import pathlib
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
watcher = importlib.import_module("review_round_watcher")


class ProjectBusyTest(unittest.TestCase):
    def test_project_ledger_field_blocks_second_worker(self):
        with tempfile.NamedTemporaryFile("w", delete=False) as ledger:
            ledger.write(json.dumps({"id": "t-live", "project": "/work/project"}) + "\n")
            ledger_path = ledger.name
        self.addCleanup(lambda: pathlib.Path(ledger_path).unlink(missing_ok=True))
        result = SimpleNamespace(stdout="t-live: RUNNING\n", stderr="", returncode=0)
        with patch.object(watcher, "LEDGER", ledger_path), \
             patch.object(watcher.subprocess, "run", return_value=result) as run:
            self.assertTrue(watcher.project_dir_busy("/work/project"))
        self.assertEqual(run.call_args.args[0][-2:], ["status", "t-live"])

    def test_project_busy_scans_entries_older_than_previous_tail_limit(self):
        with tempfile.NamedTemporaryFile("w", delete=False) as ledger:
            ledger.write(json.dumps({"id": "t-old", "project": "/work/project"}) + "\n")
            for n in range(70):
                ledger.write(json.dumps({"id": f"t-{n}", "project": "/other"}) + "\n")
            ledger_path = ledger.name
        self.addCleanup(lambda: pathlib.Path(ledger_path).unlink(missing_ok=True))
        result = SimpleNamespace(stdout="t-old: RUNNING\n", stderr="", returncode=0)
        with patch.object(watcher, "LEDGER", ledger_path), \
             patch.object(watcher.subprocess, "run", return_value=result):
            self.assertTrue(watcher.project_dir_busy("/work/project"))

    def test_relay_reviewer_state_comes_from_dept_status_not_a_pid(self):
        result = SimpleNamespace(stdout="t-relay: RUNNING\n", stderr="", returncode=0)
        with patch.object(watcher.subprocess, "run", return_value=result), \
             patch.object(watcher, "mac") as mac:
            self.assertEqual(watcher.reviewer_states(["t-relay"]), {"t-relay": "running"})
        mac.assert_not_called()

    def test_completed_reviewer_collects_findings(self):
        result = SimpleNamespace(stdout="t-relay: DONE\n", stderr="", returncode=0)
        with patch.object(watcher.subprocess, "run", return_value=result), \
             patch.object(watcher, "mac", return_value="t-relay done\n"):
            self.assertEqual(watcher.reviewer_states(["t-relay"]), {"t-relay": "done"})

    def test_fetch_findings_parses_multiple_outputs_without_trailing_newline(self):
        output = "\n@@@t-correct@@@\nfirst finding\n@@@t-tests@@@\nsecond finding"
        with patch.object(watcher, "mac", return_value=output) as mac:
            findings = watcher.fetch_findings(["t-correct", "t-tests"])
        self.assertEqual(findings, {
            "t-correct": "first finding",
            "t-tests": "second finding",
        })
        self.assertIn("@@@t-correct@@@", mac.call_args.args[0])

    def test_dead_empty_reviewers_reach_attention_after_miss_limit(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "round.json"
            path.write_text(json.dumps({
                "pr": 7, "project_dir": "/project", "owning_session": "session",
                "branch": "branch", "reviewers": {"t-a": "tests"},
                "status": "collecting", "misses": {"t-a": watcher.MISS_LIMIT - 1},
            }))
            with patch.object(watcher, "reviewer_states", return_value={"t-a": "dead-empty"}):
                result = watcher.process_round(str(path))
            round_ = json.loads(path.read_text())
        self.assertIn("ATTENTION", result)
        self.assertEqual(round_["status"], "attention")

    def test_all_done_dispatches_owning_session_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "round.json"
            path.write_text(json.dumps({
                "pr": 7, "project_dir": "/project", "owning_session": "session",
                "branch": "branch", "reviewers": {"t-a": "tests"},
                "status": "collecting",
            }))
            result = SimpleNamespace(stdout="resumed t-owner proc=x\n", stderr="", returncode=0)
            with patch.object(watcher, "PROMPT_DIR", tmp), \
                 patch.object(watcher, "reviewer_states", return_value={"t-a": "done"}), \
                 patch.object(watcher, "project_dir_busy", return_value=False), \
                 patch.object(watcher, "mac", return_value="\n@@@t-a@@@\nclean"), \
                 patch.object(watcher.subprocess, "run", return_value=result) as run:
                message = watcher.process_round(str(path))
            round_ = json.loads(path.read_text())
            prompt = pathlib.Path(round_["prompt_file"]).read_text()
        self.assertIn("resumed owning session as t-owner", message)
        self.assertEqual(round_["status"], "dispatched")
        self.assertEqual(run.call_args.args[0][1:3], [watcher.DEPT, "resume"])
        self.assertIn("clean", prompt)


if __name__ == "__main__":
    unittest.main()
