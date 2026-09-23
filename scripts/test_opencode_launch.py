#!/usr/bin/env python3
"""Offline contract tests for scripts/opencode-launch."""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import textwrap
import unittest


RUNNER_PATH = Path(__file__).with_name("opencode-launch")
loader = importlib.machinery.SourceFileLoader("opencode_launch", str(RUNNER_PATH))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class OpenCodeLaunchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.project = self.root / "project"
        self.project.mkdir()
        self.task = self.root / "task"
        self.task.mkdir()
        self.fake_opencode = self.root / "opencode"
        self.fake_opencode.write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env python3
                import json
                import os
                from pathlib import Path
                import sys
                if sys.stdin.read():
                    raise SystemExit("stdin was not closed")
                Path(os.environ["OPENCODE_LAUNCH_TEST_ARGV"]).write_text(json.dumps(sys.argv[1:]))
                print(json.dumps({"type": "step_start", "sessionID": "ses_test"}))
                print(json.dumps({"type": "text", "sessionID": "ses_test", "part": {"text": "zen-ok"}}))
                print(json.dumps({"type": "step_finish", "sessionID": "ses_test", "part": {"tokens": {"input": 3, "output": 2, "reasoning": 1, "cache": {"read": 4, "write": 5}}, "cost": 0}}))
                """
            )
        )
        self.fake_opencode.chmod(0o755)
        self.original_bin = runner.OPENCODE_BIN
        self.original_roots = runner.ALLOWED_PROJECT_ROOTS
        self.original_argv_path = os.environ.get("OPENCODE_LAUNCH_TEST_ARGV")
        self.argv_path = self.root / "argv.json"
        os.environ["OPENCODE_LAUNCH_TEST_ARGV"] = str(self.argv_path)
        runner.OPENCODE_BIN = self.fake_opencode
        runner.ALLOWED_PROJECT_ROOTS = (self.project.resolve(),)

    def tearDown(self) -> None:
        runner.OPENCODE_BIN = self.original_bin
        runner.ALLOWED_PROJECT_ROOTS = self.original_roots
        if self.original_argv_path is None:
            del os.environ["OPENCODE_LAUNCH_TEST_ARGV"]
        else:
            os.environ["OPENCODE_LAUNCH_TEST_ARGV"] = self.original_argv_path
        self.temporary.cleanup()

    def stage(self, **runtime: str) -> None:
        (self.task / runner.PROMPT_FILE).write_text("reply with exactly: zen-ok", encoding="utf-8")
        values = {"project_dir": str(self.project), **runtime}
        (self.task / runner.RUNTIME_FILE).write_text(json.dumps(values), encoding="utf-8")

    def test_run_writes_structured_output_and_closes_stdin(self) -> None:
        self.stage(title="smoke")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 0)
        self.assertEqual((self.task / runner.RESULT_FILE).read_text(), "zen-ok")
        self.assertEqual((self.task / runner.SESSION_FILE).read_text(), "ses_test\n")
        self.assertEqual(
            json.loads((self.task / runner.USAGE_FILE).read_text()),
            {
                "runtime": "opencode",
                "operation": "run",
                "exit_code": 0,
                "input_tokens": 3,
                "output_tokens": 2,
                "reasoning_tokens": 1,
                "cache_read_tokens": 4,
                "cache_write_tokens": 5,
                "cost": 0,
            },
        )
        recorded = json.loads((self.task / runner.RUN_FILE).read_text())
        self.assertEqual(recorded["model"], runner.DEFAULT_MODEL)
        self.assertEqual(
            json.loads(self.argv_path.read_text()),
            [
                "run",
                "--format",
                "json",
                "--dir",
                str(self.project.resolve()),
                "--model",
                runner.DEFAULT_MODEL,
                "--title",
                "smoke",
                "reply with exactly: zen-ok",
            ],
        )

    def test_resume_uses_the_staged_session(self) -> None:
        self.stage(session_id="ses_prior", model="opencode/big-pickle")
        self.assertEqual(runner.main(["resume", "--task-dir", str(self.task)]), 0)
        recorded = json.loads((self.task / runner.RUN_FILE).read_text())
        self.assertEqual(recorded["session_id"], "ses_prior")
        self.assertEqual(recorded["model"], "opencode/big-pickle")
        self.assertEqual(
            json.loads(self.argv_path.read_text()),
            [
                "run",
                "--format",
                "json",
                "--dir",
                str(self.project.resolve()),
                "--session",
                "ses_prior",
                "reply with exactly: zen-ok",
            ],
        )

    def test_rejects_non_free_models_and_projects_outside_the_allowlist(self) -> None:
        self.stage(model="openai/gpt-5")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.stage(project_dir=str(self.root))
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)


if __name__ == "__main__":
    unittest.main()
