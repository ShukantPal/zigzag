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

    def test_relay_reviewer_state_comes_from_dept_status_not_a_pid(self):
        result = SimpleNamespace(stdout="t-relay: RUNNING\n", stderr="", returncode=0)
        with patch.object(watcher.subprocess, "run", return_value=result), \
             patch.object(watcher, "mac") as mac:
            self.assertEqual(watcher.reviewer_states(["t-relay"]), {"t-relay": "running"})
        mac.assert_not_called()


if __name__ == "__main__":
    unittest.main()
