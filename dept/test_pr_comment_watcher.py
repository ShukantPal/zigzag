import importlib
from contextlib import nullcontext
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
watcher = importlib.import_module("pr_comment_watcher")


class WatermarkTest(unittest.TestCase):
    def test_save_writes_supplied_watermark_under_serialized_poll(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "watermark.json"
            path.write_text(json.dumps({"seen": ["ic:old"], "pr_state": {"1": "OPEN"}}))
            with patch.object(watcher, "WATERMARK", str(path)), \
                 patch.object(watcher, "STATE_DIR", tmp):
                watcher.save_watermark({"seen": ["rc:new"], "pr_state": {"2": "CLOSED"}})
            data = json.loads(path.read_text())
        self.assertEqual(data["seen"], ["rc:new"])
        self.assertEqual(data["pr_state"], {"2": "CLOSED"})

    def test_pr_task_running_fails_closed_when_active_task_status_errors(self):
        with tempfile.TemporaryDirectory() as tmp:
            sessions = pathlib.Path(tmp) / "sessions.json"
            sessions.write_text(json.dumps({watcher.session_key("owner/repo", 3): {"active_task": "t-live"}}))
            with patch.object(watcher, "REPO", "owner/repo"), \
                 patch.object(watcher, "SESSIONS_FILE", str(sessions)), \
                 patch.object(watcher, "dept_status_text", return_value=None):
                self.assertEqual(watcher.pr_task_running(3), "t-live")

    def test_pr_task_running_fails_closed_when_exit_is_unknown(self):
        with tempfile.TemporaryDirectory() as tmp:
            sessions = pathlib.Path(tmp) / "sessions.json"
            sessions.write_text(json.dumps({watcher.session_key("owner/repo", 3): {"active_task": "t-live"}}))
            with patch.object(watcher, "REPO", "owner/repo"), \
                 patch.object(watcher, "SESSIONS_FILE", str(sessions)), \
                 patch.object(watcher, "dept_status_text",
                              return_value="t-live: DONE (exit unknown)\n"):
                self.assertEqual(watcher.pr_task_running(3), "t-live")

    def test_pr_task_running_releases_pruned_relay_task(self):
        with tempfile.TemporaryDirectory() as tmp:
            sessions = pathlib.Path(tmp) / "sessions.json"
            sessions.write_text(json.dumps({watcher.session_key("owner/repo", 3):
                                            {"active_task": "t-old"}}))
            with patch.object(watcher, "REPO", "owner/repo"), \
                 patch.object(watcher, "SESSIONS_FILE", str(sessions)), \
                 patch.object(watcher, "LEDGER", str(pathlib.Path(tmp) / "missing-ledger")), \
                 patch.object(watcher, "dept_status_text",
                              return_value="t-old: DONE (pruned)\n"):
                self.assertIsNone(watcher.pr_task_running(3))

    def test_dispatch_records_repo_scoped_active_task(self):
        with tempfile.TemporaryDirectory() as tmp:
            prompt_dir = pathlib.Path(tmp) / "prompts"
            result = type("R", (), {"returncode": 0, "stdout": "started t-new proc=x", "stderr": ""})()
            with patch.object(watcher, "REPO", "owner/repo"), \
                 patch.object(watcher, "PROMPT_DIR", str(prompt_dir)), \
                 patch.object(watcher, "pr_task_running", return_value=None), \
                 patch.object(watcher, "project_dir_busy", return_value=False), \
                 patch.object(watcher.subprocess, "run", return_value=result), \
                 patch.object(watcher, "set_active_task") as set_active:
                tid, _ = watcher._dispatch_locked(3, None, "prompt", [])
        self.assertEqual(tid, "t-new")
        set_active.assert_called_once_with("owner/repo", 3, "t-new")

    def test_dispatch_resumes_legacy_numeric_session(self):
        with patch.object(watcher, "REPO", "owner/repo"), \
             patch.object(watcher, "worker_dispatch_lock",
                          return_value=nullcontext(True)), \
             patch.object(watcher, "load_sessions",
                          return_value={"3": {"session_id": "legacy-session"}}), \
             patch.object(watcher, "_dispatch_locked", return_value=("t-new", "ok")) as dispatch:
            watcher.dispatch(3, "branch", [], {})
        self.assertEqual(dispatch.call_args.args[1], "legacy-session")


if __name__ == "__main__":
    unittest.main()
