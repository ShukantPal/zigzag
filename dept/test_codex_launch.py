import os
import pathlib
import subprocess
import tempfile
import unittest


LAUNCHER = pathlib.Path(__file__).with_name("codex-launch.sh")


class LauncherTest(unittest.TestCase):
    def test_run_and_resume_forward_exact_model_arguments_and_capture_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            task = root / "task"; task.mkdir()
            project = root / "project"; project.mkdir()
            for name, text in {"dir.txt": str(project), "prompt.txt": "hello", "resume.txt": "sid", "model.txt": "model"}.items():
                (task / name).write_text(text)
            (task / "read-only.txt").touch()
            fake = root / "codex"
            fake.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\nexit 0\n")
            fake.chmod(0o755)
            for mode, expected in (
                ("run", ["exec", "--json", "--sandbox", "read-only", "--skip-git-repo-check", "-m", "model", "-C", str(project), "-o", str(task / "last-message.txt"), "hello"]),
                ("resume", ["exec", "--json", "--sandbox", "read-only", "--skip-git-repo-check", "-m", "model", "resume", "sid", "hello", "-o", str(task / "last-message.txt")]),
            ):
                capture = root / mode
                env = dict(os.environ, CODEX=str(fake), CAPTURE=str(capture))
                self.assertEqual(subprocess.run([str(LAUNCHER), mode, str(task)], env=env).returncode, 0)
                self.assertEqual(capture.read_text().splitlines(), expected)
                self.assertTrue((task / "events.jsonl").exists())
                self.assertTrue((task / "stderr.log").exists())

    def test_run_without_model_omits_model_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp); task = root / "task"; task.mkdir(); project = root / "project"; project.mkdir()
            (task / "dir.txt").write_text(str(project)); (task / "prompt.txt").write_text("hello")
            fake = root / "codex"; fake.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n"); fake.chmod(0o755)
            capture = root / "capture"
            self.assertEqual(subprocess.run([str(LAUNCHER), "run", str(task)], env=dict(os.environ, CODEX=str(fake), CAPTURE=str(capture))).returncode, 0)
            self.assertNotIn("-m", capture.read_text().splitlines())

    def test_model_override_cannot_inject_arguments(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp); task = root / "task"; task.mkdir(); project = root / "project"; project.mkdir()
            (task / "dir.txt").write_text(str(project)); (task / "prompt.txt").write_text("hello")
            (task / "model.txt").write_text("model --approve-for-me")
            fake = root / "codex"; fake.write_text("#!/bin/sh\nexit 0\n"); fake.chmod(0o755)
            result = subprocess.run([str(LAUNCHER), "run", str(task)],
                                    env=dict(os.environ, CODEX=str(fake)))
            self.assertEqual(result.returncode, 2)
            self.assertIn("invalid model identifier", (task / "stderr.log").read_text())

    def test_missing_payload_and_codex_failure_propagate(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp); task = root / "task"; task.mkdir()
            self.assertEqual(subprocess.run([str(LAUNCHER), "run", str(task)]).returncode, 2)
            self.assertIn("codex-launch start", (task / "stderr.log").read_text())
            project = root / "project"; project.mkdir()
            (task / "dir.txt").write_text(str(project)); (task / "prompt.txt").write_text("x")
            fake = root / "codex"; fake.write_text("#!/bin/sh\nexit 7\n"); fake.chmod(0o755)
            self.assertEqual(subprocess.run([str(LAUNCHER), "run", str(task),],
                                            env=dict(os.environ, CODEX=str(fake))).returncode, 7)
