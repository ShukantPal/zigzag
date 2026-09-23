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
        self.calls_path = self.root / "calls.jsonl"
        self.fake_opencode = self.root / "opencode"
        self.fake_opencode.write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env python3
                import json
                import os
                from pathlib import Path
                import sys
                import time

                args = sys.argv[1:]
                with Path(os.environ["OPENCODE_LAUNCH_TEST_CALLS"]).open("a") as calls:
                    calls.write(json.dumps(args) + "\\n")
                if sys.stdin.buffer.read():
                    raise SystemExit("stdin was not closed")
                mode = os.environ.get("OPENCODE_LAUNCH_TEST_MODE", "success")
                if args[:2] == ["models", "opencode"]:
                    for identifier in ("muse-spark-1.3-contributor-free", "big-pickle"):
                        print(json.dumps({"id": identifier, "providerID": "opencode", "api": {"url": "https://opencode.ai/zen/v1"}, "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}}))
                    raise SystemExit(0)
                if args[:1] == ["export"]:
                    model = "paid-model" if mode == "bad_session_model" else "big-pickle"
                    project = "/wrong-project" if mode == "wrong_session_project" else os.environ["OPENCODE_LAUNCH_TEST_PROJECT"]
                    print(json.dumps({"info": {"directory": project, "model": {"providerID": "opencode", "id": model}}}))
                    raise SystemExit(0)
                if mode == "timeout":
                    time.sleep(0.5)
                    raise SystemExit(0)
                if mode == "binary":
                    sys.stdout.buffer.write(b"\\xff")
                    raise SystemExit(0)
                if mode == "malformed":
                    print("not-json")
                    raise SystemExit(0)
                if mode == "error":
                    print(json.dumps({"type": "error", "sessionID": "ses_error"}))
                    raise SystemExit(0)
                def finish(input_tokens, output_tokens, reasoning_tokens, cache_read, cache_write, cost):
                    return {"type": "step_finish", "sessionID": "ses_test", "part": {"tokens": {"input": input_tokens, "output": output_tokens, "reasoning": reasoning_tokens, "cache": {"read": cache_read, "write": cache_write}}, "cost": cost}}
                print(json.dumps({"type": "step_start", "sessionID": "ses_test"}))
                print(json.dumps({"type": "text", "sessionID": "ses_test", "part": {"text": "intermediate"}}))
                print(json.dumps(finish(3, 2, 1, 4, 5, 0)))
                print(json.dumps({"type": "text", "sessionID": "ses_test", "part": {"text": "zen-ok"}}))
                print(json.dumps(finish(7, 11, 13, 17, 19, 0)))
                raise SystemExit(7 if mode == "failure" else 0)
                """
            )
        )
        self.fake_opencode.chmod(0o755)
        self.original_bin = runner.OPENCODE_BIN
        self.original_roots = runner.ALLOWED_PROJECT_ROOTS
        self.original_timeout = runner.PROCESS_TIMEOUT_SECONDS
        self.original_environment = {
            name: os.environ.get(name)
            for name in (
                "OPENCODE_LAUNCH_TEST_CALLS",
                "OPENCODE_LAUNCH_TEST_MODE",
                "OPENCODE_LAUNCH_TEST_PROJECT",
            )
        }
        os.environ["OPENCODE_LAUNCH_TEST_CALLS"] = str(self.calls_path)
        os.environ["OPENCODE_LAUNCH_TEST_PROJECT"] = str(self.project.resolve())
        os.environ["OPENCODE_LAUNCH_TEST_MODE"] = "success"
        runner.OPENCODE_BIN = self.fake_opencode
        runner.ALLOWED_PROJECT_ROOTS = (self.project.resolve(),)

    def tearDown(self) -> None:
        runner.OPENCODE_BIN = self.original_bin
        runner.ALLOWED_PROJECT_ROOTS = self.original_roots
        runner.PROCESS_TIMEOUT_SECONDS = self.original_timeout
        for name, value in self.original_environment.items():
            if value is None:
                del os.environ[name]
            else:
                os.environ[name] = value
        self.temporary.cleanup()

    def stage(self, prompt: str = "reply with exactly: zen-ok", **runtime: str) -> None:
        (self.task / runner.PROMPT_FILE).write_text(prompt, encoding="utf-8")
        values = {"project_dir": str(self.project), **runtime}
        (self.task / runner.RUNTIME_FILE).write_text(json.dumps(values), encoding="utf-8")

    def calls(self) -> list[list[str]]:
        if not self.calls_path.exists():
            return []
        return [json.loads(line) for line in self.calls_path.read_text().splitlines()]

    def run_calls(self) -> list[list[str]]:
        return [call for call in self.calls() if call[:1] == ["run"]]

    def usage(self) -> dict[str, object]:
        return json.loads((self.task / runner.USAGE_FILE).read_text())

    def test_run_uses_staged_model_terminates_prompt_options_and_aggregates_usage(self) -> None:
        self.stage(prompt="--model=openai/gpt-5", model="opencode/big-pickle", title="smoke", agent="build")
        outside = self.root / "outside.txt"
        outside.write_text("unchanged", encoding="utf-8")
        (self.task / runner.RESULT_FILE).symlink_to(outside)
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 0)
        self.assertEqual((self.task / runner.RESULT_FILE).read_text(), "zen-ok")
        self.assertEqual(outside.read_text(), "unchanged")
        self.assertEqual((self.task / runner.SESSION_FILE).read_text(), "ses_test\n")
        self.assertEqual(
            self.usage(),
            {
                "runtime": "opencode",
                "operation": "run",
                "exit_code": 0,
                "timed_out": False,
                "completed": True,
                "stream_error": None,
                "input_tokens": 10,
                "output_tokens": 13,
                "reasoning_tokens": 14,
                "cache_read_tokens": 21,
                "cache_write_tokens": 24,
                "cost": 0,
            },
        )
        self.assertEqual(
            self.run_calls(),
            [["run", "--format", "json", "--dir", str(self.project.resolve()), "--model", "opencode/big-pickle", "--title", "smoke", "--agent", "build", "--", "--model=openai/gpt-5"]],
        )

    def test_run_uses_the_default_free_zen_model(self) -> None:
        self.stage()
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 0)
        self.assertIn(runner.DEFAULT_MODEL, self.run_calls()[0])

    def test_resume_attests_its_session_model_and_project(self) -> None:
        self.stage(session_id="ses_prior")
        self.assertEqual(runner.main(["resume", "--task-dir", str(self.task)]), 0)
        self.assertIn(["export", "ses_prior"], self.calls())
        self.assertEqual(
            self.run_calls(),
            [["run", "--format", "json", "--dir", str(self.project.resolve()), "--session", "ses_prior", "--", "reply with exactly: zen-ok"]],
        )
        self.assertEqual(json.loads((self.task / runner.RUN_FILE).read_text())["model"], "opencode/big-pickle")

    def test_rejects_disallowed_models_without_running_opencode(self) -> None:
        for model in ("openai/gpt-5", "anthropic/claude", "opencode/paid-model", "../opencode/big-pickle"):
            with self.subTest(model=model):
                self.stage(model=model)
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
                self.assertEqual(self.run_calls(), [])
                self.calls_path.unlink(missing_ok=True)

    def test_rejects_malformed_staged_tasks_without_running_opencode(self) -> None:
        cases = [
            ("not-json", "reply with exactly: zen-ok", "run"),
            ("[]", "reply with exactly: zen-ok", "run"),
            (json.dumps({"project_dir": str(self.project), "unknown": "x"}), "reply with exactly: zen-ok", "run"),
            (json.dumps({"project_dir": "relative"}), "reply with exactly: zen-ok", "run"),
            (json.dumps({"project_dir": str(self.project)}), "reply with exactly: zen-ok", "resume"),
            (json.dumps({"project_dir": str(self.project), "session_id": "--help"}), "reply with exactly: zen-ok", "resume"),
            (json.dumps({"project_dir": str(self.project)}), "", "run"),
        ]
        for runtime, prompt, operation in cases:
            with self.subTest(runtime=runtime, prompt=prompt, operation=operation):
                (self.task / runner.PROMPT_FILE).write_text(prompt, encoding="utf-8")
                (self.task / runner.RUNTIME_FILE).write_text(runtime, encoding="utf-8")
                self.assertEqual(runner.main([operation, "--task-dir", str(self.task)]), 2)
                self.assertEqual(self.run_calls(), [])
                self.calls_path.unlink(missing_ok=True)

    def test_rejects_cli_model_flag_and_project_symlinks(self) -> None:
        self.stage()
        with self.assertRaises(SystemExit):
            runner.main(["run", "--task-dir", str(self.task), "--model", "opencode/big-pickle"])
        self.assertEqual(self.run_calls(), [])
        linked_project = self.root / "linked-project"
        linked_project.symlink_to(self.project, target_is_directory=True)
        self.stage(project_dir=str(linked_project))
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assertEqual(self.run_calls(), [])

    def test_failure_error_malformed_binary_and_timeout_write_explicit_artifacts(self) -> None:
        cases = (("failure", 7, False, None), ("error", 1, False, "error event"), ("malformed", 1, False, "malformed"), ("binary", 1, False, "utf-8"), ("timeout", 124, True, "empty event stream"))
        for mode, expected_code, timed_out, error_text in cases:
            with self.subTest(mode=mode):
                self.stage()
                (self.task / runner.SESSION_FILE).write_text("stale\n", encoding="utf-8")
                os.environ["OPENCODE_LAUNCH_TEST_MODE"] = mode
                if mode == "timeout":
                    runner.PROCESS_TIMEOUT_SECONDS = 0.1
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), expected_code)
                usage = self.usage()
                self.assertEqual(usage["timed_out"], timed_out)
                if error_text is None:
                    self.assertTrue(usage["completed"])
                    self.assertEqual((self.task / runner.RESULT_FILE).read_text(), "zen-ok")
                else:
                    self.assertIn(error_text, usage["stream_error"])
                    if mode == "error":
                        self.assertEqual((self.task / runner.SESSION_FILE).read_text(), "ses_error\n")
                    else:
                        self.assertFalse((self.task / runner.SESSION_FILE).exists())
                runner.PROCESS_TIMEOUT_SECONDS = self.original_timeout
                os.environ["OPENCODE_LAUNCH_TEST_MODE"] = "success"
                self.calls_path.unlink(missing_ok=True)

    def test_rejects_non_free_or_wrong_project_resumed_sessions(self) -> None:
        for mode in ("bad_session_model", "wrong_session_project"):
            with self.subTest(mode=mode):
                self.stage(session_id="ses_prior")
                os.environ["OPENCODE_LAUNCH_TEST_MODE"] = mode
                self.assertEqual(runner.main(["resume", "--task-dir", str(self.task)]), 2)
                self.assertEqual(self.run_calls(), [])
                os.environ["OPENCODE_LAUNCH_TEST_MODE"] = "success"
                self.calls_path.unlink(missing_ok=True)


if __name__ == "__main__":
    unittest.main()
