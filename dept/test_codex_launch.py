import os
import pathlib
import subprocess
import tempfile
import unittest


LAUNCHER = pathlib.Path(__file__).with_name("codex-launch.sh")


class LauncherTest(unittest.TestCase):
    def test_run_and_resume_forward_model_and_capture_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            task = root / "task"; task.mkdir()
            project = root / "project"; project.mkdir()
            for name, text in {"dir.txt": str(project), "prompt.txt": "hello", "resume.txt": "sid", "model.txt": "model"}.items():
                (task / name).write_text(text)
            fake = root / "codex"
            fake.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\nexit 0\n")
            fake.chmod(0o755)
            for mode in ("run", "resume"):
                if mode == "run":
                    (task / "model.txt").unlink()
                else:
                    (task / "model.txt").write_text("model")
                capture = root / mode
                env = dict(os.environ, CODEX=str(fake), CAPTURE=str(capture))
                self.assertEqual(subprocess.run([str(LAUNCHER), mode, str(task)], env=env).returncode, 0)
                self.assertEqual("-m" in capture.read_text().splitlines(), mode == "resume")
                self.assertTrue((task / "events.jsonl").exists())
                self.assertTrue((task / "stderr.log").exists())

    def test_missing_payload_and_codex_failure_propagate(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp); task = root / "task"; task.mkdir()
            self.assertEqual(subprocess.run([str(LAUNCHER), "run", str(task)]).returncode, 2)
            project = root / "project"; project.mkdir()
            (task / "dir.txt").write_text(str(project)); (task / "prompt.txt").write_text("x")
            fake = root / "codex"; fake.write_text("#!/bin/sh\nexit 7\n"); fake.chmod(0o755)
            self.assertEqual(subprocess.run([str(LAUNCHER), "run", str(task),],
                                            env=dict(os.environ, CODEX=str(fake))).returncode, 7)
