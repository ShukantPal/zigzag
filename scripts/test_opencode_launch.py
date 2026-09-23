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
        self.outside_project = self.root / "outside-project"
        self.outside_project.mkdir()
        self.task = self.root / "task"
        self.task.mkdir()
        self.calls_path = self.root / "calls.jsonl"
        self.fake_opencode = self.root / "opencode"
        self.fake_state = self.root / "opencode-state.json"
        self.fake_opencode.write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env python3
                import json
                import os
                from pathlib import Path
                import signal
                import sys

                state = json.loads(Path(__file__).with_name("opencode-state.json").read_text())
                args = sys.argv[1:]
                with Path(state["calls"]).open("a") as calls:
                    calls.write(json.dumps(args) + "\\n")
                if sys.stdin.buffer.read():
                    raise SystemExit("stdin was not closed")
                for name in ("OPENCODE_CONFIG", "OPENCODE_CONFIG_CONTENT", "OPENCODE_CONFIG_DIR", "OPENCODE_MODELS_URL"):
                    if os.environ.get(name) == state["poison"]:
                        raise SystemExit("caller OpenCode config leaked into subprocess")
                mode = state["mode"]
                if args[:2] == ["models", "opencode"]:
                    models = [
                        {"id": "muse-spark-1.3-contributor-free", "providerID": "opencode", "api": {"url": "https://opencode.ai/zen/v1"}, "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}},
                        {"id": "big-pickle", "providerID": "opencode", "api": {"url": "https://opencode.ai/zen/v1"}, "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}},
                        {"id": "wrong-provider", "providerID": "openai", "api": {"url": "https://opencode.ai/zen/v1"}, "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}},
                        {"id": "wrong-endpoint", "providerID": "opencode", "api": {"url": "https://example.test"}, "cost": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}},
                        {"id": "paid-listed", "providerID": "opencode", "api": {"url": "https://opencode.ai/zen/v1"}, "cost": {"input": 1, "output": 0, "cache": {"read": 0, "write": 0}}},
                    ]
                    for model in models:
                        print(json.dumps(model))
                    raise SystemExit(0)
                if args[:1] == ["export"]:
                    model = "paid-listed" if mode in ("bad_session_model", "postflight_bad_model") else "big-pickle"
                    if mode not in ("bad_session_model", "postflight_bad_model"):
                        prior = [json.loads(line) for line in Path(state["calls"]).read_text().splitlines()]
                        for call in reversed(prior):
                            if call[:1] == ["run"] and "--model" in call:
                                model = call[call.index("--model") + 1].split("/", 1)[1]
                                break
                    project = "/wrong-project" if mode == "wrong_session_project" else state["project"]
                    print(json.dumps({"info": {"directory": project, "model": {"providerID": "opencode", "id": model}}}))
                    raise SystemExit(0)
                if mode == "timeout":
                    signal.pause()
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
                if mode == "no_completion":
                    raise SystemExit(0)
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
        self.poison = "caller-controlled"
        self.original_environment = {name: os.environ.get(name) for name in (
            "OPENCODE_CONFIG", "OPENCODE_CONFIG_CONTENT", "OPENCODE_CONFIG_DIR", "OPENCODE_MODELS_URL"
        )}
        for name in self.original_environment:
            os.environ[name] = self.poison
        runner.OPENCODE_BIN = self.fake_opencode
        runner.ALLOWED_PROJECT_ROOTS = (self.project.resolve(),)
        self.set_state()

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

    def set_state(self, mode: str = "success") -> None:
        self.fake_state.write_text(json.dumps({
            "calls": str(self.calls_path),
            "mode": mode,
            "project": str(self.project.resolve()),
            "poison": self.poison,
        }), encoding="utf-8")

    def stage(self, prompt: str = "reply with exactly: zen-ok", **runtime: str) -> None:
        (self.task / runner.PROMPT_FILE).write_text(prompt, encoding="utf-8")
        (self.task / runner.RUNTIME_FILE).write_text(
            json.dumps({"project_dir": str(self.project), **runtime}), encoding="utf-8"
        )

    def calls(self) -> list[list[str]]:
        if not self.calls_path.exists():
            return []
        return [json.loads(line) for line in self.calls_path.read_text().splitlines()]

    def run_calls(self) -> list[list[str]]:
        return [call for call in self.calls() if call[:1] == ["run"]]

    def usage(self) -> dict[str, object]:
        return json.loads((self.task / runner.USAGE_FILE).read_text())

    def clear_calls(self) -> None:
        self.calls_path.unlink(missing_ok=True)

    def assert_preflight_failure(self, operation: str = "run") -> None:
        self.assertEqual(self.run_calls(), [])
        self.assertEqual(self.usage()["exit_code"], 2)
        self.assertEqual(json.loads((self.task / runner.RUN_FILE).read_text())["state"], "failed")
        self.assertFalse((self.task / runner.SESSION_FILE).exists())
        self.clear_calls()

    def test_run_uses_staged_model_terminates_prompt_options_and_aggregates_usage(self) -> None:
        self.stage(prompt="--model=openai/gpt-5", model="opencode/big-pickle", title="smoke")
        targets = []
        for name in runner.OUTPUT_FILES:
            target = self.root / f"outside-{name}"
            target.write_text("unchanged", encoding="utf-8")
            (self.task / name).symlink_to(target)
            targets.append(target)
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 0)
        self.assertEqual((self.task / runner.RESULT_FILE).read_text(), "zen-ok")
        self.assertTrue(all(target.read_text() == "unchanged" for target in targets))
        self.assertEqual((self.task / runner.SESSION_FILE).read_text(), "ses_test\n")
        self.assertEqual(self.usage()["input_tokens"], 10)
        self.assertEqual(self.usage()["cache_write_tokens"], 24)
        self.assertEqual(
            self.run_calls(),
            [["run", "--pure", "--format", "json", "--dir", str(self.project.resolve()), "--model", "opencode/big-pickle", "--title", "smoke", "--", "--model=openai/gpt-5"]],
        )

    def test_default_model_and_resume_are_attested_after_execution(self) -> None:
        self.stage()
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 0)
        self.assertIn(runner.DEFAULT_MODEL, self.run_calls()[0])
        self.clear_calls()
        self.stage(session_id="ses_prior")
        self.assertEqual(runner.main(["resume", "--task-dir", str(self.task)]), 0)
        self.assertIn(["export", "ses_prior"], self.calls())
        self.assertIn(["export", "ses_test"], self.calls())
        self.assertIn("--session", self.run_calls()[0])

    def test_metadata_filter_rejects_listed_non_zen_nonfree_and_foreign_models(self) -> None:
        for model in ("opencode/wrong-endpoint", "opencode/paid-listed", "openai/wrong-provider", "anthropic/claude"):
            with self.subTest(model=model):
                self.stage(model=model)
                (self.task / runner.SESSION_FILE).write_text("stale\n", encoding="utf-8")
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
                self.assert_preflight_failure()

    def test_rejects_absolute_outside_root_and_invalid_staged_files(self) -> None:
        self.stage(project_dir=str(self.outside_project))
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure()
        cases = ((runner.PROMPT_FILE, b"\xff"), (runner.RUNTIME_FILE, b"\xff"))
        for name, contents in cases:
            with self.subTest(name=name):
                self.stage()
                (self.task / name).write_bytes(contents)
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
                self.assert_preflight_failure()
        self.stage()
        (self.task / runner.PROMPT_FILE).unlink()
        (self.task / runner.PROMPT_FILE).symlink_to(self.root / "outside-prompt")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure()
        self.stage()
        (self.task / runner.PROMPT_FILE).write_text("x" * (runner.MAX_PROMPT_BYTES + 1), encoding="utf-8")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure()
        self.stage()
        (self.task / runner.RUNTIME_FILE).unlink()
        (self.task / runner.RUNTIME_FILE).symlink_to(self.root / "outside-runtime")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure()

    def test_missing_completion_and_postflight_model_mismatch_fail(self) -> None:
        for mode, expected_error in (("no_completion", "no step_finish"), ("postflight_bad_model", "does not attest")):
            with self.subTest(mode=mode):
                self.stage()
                self.set_state(mode)
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 1)
                self.assertIn(expected_error, self.usage()["stream_error"])
                self.assertEqual(json.loads((self.task / runner.RUN_FILE).read_text())["state"], "failed")
                self.set_state()
                self.clear_calls()

    def test_failure_error_malformed_binary_and_deterministic_timeout_write_artifacts(self) -> None:
        cases = (("failure", 7, None), ("error", 1, "error event"), ("malformed", 1, "malformed"), ("binary", 1, "utf-8"), ("timeout", 124, "empty event stream"))
        for mode, expected_code, expected_error in cases:
            with self.subTest(mode=mode):
                self.stage()
                self.set_state(mode)
                if mode == "timeout":
                    runner.PROCESS_TIMEOUT_SECONDS = 0.1
                self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), expected_code)
                if expected_error:
                    self.assertIn(expected_error, self.usage()["stream_error"])
                self.assertEqual(self.usage()["timed_out"], mode == "timeout")
                runner.PROCESS_TIMEOUT_SECONDS = self.original_timeout
                self.set_state()
                self.clear_calls()

    def test_rejects_bad_resume_session_and_unknown_runtime_setting(self) -> None:
        self.stage(session_id="--help")
        self.assertEqual(runner.main(["resume", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure("resume")
        self.stage(agent="untrusted")
        self.assertEqual(runner.main(["run", "--task-dir", str(self.task)]), 2)
        self.assert_preflight_failure()


if __name__ == "__main__":
    unittest.main()
