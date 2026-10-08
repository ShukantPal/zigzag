import importlib
import pathlib
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
department = importlib.import_module("dept")


class SessionResolutionTest(unittest.TestCase):
    def test_session_meta_fixture_returns_cwd(self):
        fixture = DEPT_DIR / "testdata" / "session-with-cwd.jsonl"
        with fixture.open() as f:
            self.assertEqual(
                department.session_meta_cwd(f),
                "/Users/shukant/Workspace/example",
            )

    def test_session_meta_ignores_absent_malformed_and_missing_cwd(self):
        self.assertIsNone(department.session_meta_cwd([]))
        self.assertIsNone(department.session_meta_cwd(["not json\n"]))
        self.assertIsNone(department.session_meta_cwd([
            '{"type": "session_meta", "payload": {}}\n',
            '{"type": "session_meta", "payload": {"cwd": 4}}\n',
        ]))

    def test_resolve_requires_unambiguous_exact_session_identifier(self):
        self.assertIsNone(department.resolve_session_cwd("bad*id"))
        with patch.object(department, "ssh", return_value=SimpleNamespace(
                returncode=0, stdout=b'["/one/project"]\n')) as ssh:
            self.assertEqual(department.resolve_session_cwd("session_123"), "/one/project")
        script = ssh.call_args.kwargs["stdin_data"].decode()
        self.assertIn("glob.escape(sid)", script)
        self.assertIn("rollout-*-", script)
        self.assertIn("json.dumps(sorted(cwds))", script)

    def test_multiple_session_cwds_are_refused(self):
        with patch.object(department, "ssh", return_value=SimpleNamespace(
                returncode=0, stdout=b'["/one/project", "/another/project"]\n')):
            self.assertIsNone(department.resolve_session_cwd("session_123"))


class ResumeCliTest(unittest.TestCase):
    def setUp(self):
        self.prompt = tempfile.NamedTemporaryFile("wb", delete=False)
        self.prompt.write(b"continue the task\n")
        self.prompt.close()
        self.addCleanup(lambda: pathlib.Path(self.prompt.name).unlink(missing_ok=True))

    def assert_missing_directory_stops_before_dispatch(self, argv, expected_dir, resolved=True):
        with patch.object(department, "resolve_session_cwd", return_value="/resolved/project") as resolve, \
             patch.object(department, "remote_isdir", return_value=False) as isdir, \
             patch.object(department, "ssh") as ssh, \
             patch.object(department, "zigzag_spawn") as spawn:
            with self.assertRaises(SystemExit) as exit_:
                department.main(argv)
        self.assertIn("project_dir does not exist", str(exit_.exception))
        isdir.assert_called_once_with(expected_dir)
        ssh.assert_not_called()
        spawn.assert_not_called()
        if resolved:
            resolve.assert_called_once_with("session-id")
        else:
            resolve.assert_not_called()

    def test_resume_omitted_project_resolves_session_cwd_before_dispatch(self):
        self.assert_missing_directory_stops_before_dispatch(
            ["resume", "session-id", self.prompt.name], "/resolved/project")

    def test_resume_explicit_project_preserves_cli_shape_before_dispatch(self):
        self.assert_missing_directory_stops_before_dispatch(
            ["resume", "/explicit/project", "session-id", self.prompt.name],
            "/explicit/project", resolved=False)

    def test_resume_forwards_read_only_for_relay_and_ssh(self):
        for transport in ([], ["--ssh"]):
            with self.subTest(transport=transport), \
                 patch.object(department, "remote_isdir", return_value=True), \
                 patch.object(department, "dispatch_task") as dispatch:
                department.main(["resume", "/project", "session-id", self.prompt.name,
                                 "--no-sop", "--read-only", *transport])
            self.assertEqual(dispatch.call_args.args[:5],
                             ("/project", b"continue the task\n", bool(transport),
                              "session-id", None))
            self.assertTrue(dispatch.call_args.args[5])


class CommandDispatchTest(unittest.TestCase):
    def test_tunnel_proxy_is_empty_without_https_proxy(self):
        with patch.dict(department.os.environ, {}, clear=True):
            self.assertEqual(department.tunnel_proxy(), "")

    def test_start_accepts_model_override(self):
        args = department.dispatch_args(["/project", "/prompt", "--model", "gpt-6-luna"])
        self.assertEqual(args.model, "gpt-6-luna")

    def test_preallocated_task_id_is_validated_and_preserved(self):
        result = SimpleNamespace(returncode=0, stdout=b"42\n", stderr=b"")
        with patch.object(department, "setup_task_dir", return_value="/remote/t") as setup, \
             patch.object(department, "ssh", return_value=result), \
             patch.object(department, "ledger_append"):
            department.dispatch_task("/project", b"prompt", True,
                                     task_id="t-fedcba")
        self.assertEqual(setup.call_args.args[0], "t-fedcba")
        with patch.object(department, "setup_task_dir") as setup, \
             self.assertRaises(SystemExit):
            department.dispatch_task("/project", b"prompt", True,
                                     task_id="../../bad")
        setup.assert_not_called()

    def test_command_handler_key_error_is_not_reported_as_unknown_command(self):
        with patch.object(department, "cmd_start", side_effect=KeyError("connection")):
            with self.assertRaisesRegex(KeyError, "connection"):
                department.main(["start", "/project", "/prompt"])

    def test_model_reaches_ssh_setup_and_launch(self):
        result = SimpleNamespace(returncode=0, stdout=b"42\n", stderr=b"")
        with patch.object(department.uuid, "uuid4", return_value=SimpleNamespace(hex="abc123")), \
             patch.object(department, "ssh", return_value=result) as ssh, \
             patch.object(department, "ledger_append"):
            department.dispatch_task("/project", b"prompt", True, model="test-model")
        calls = "\n".join(str(c.args[0]) for c in ssh.call_args_list)
        self.assertIn("model.txt", calls)
        self.assertIn("-m", calls)
        self.assertIn("test-model", calls)
        self.assertIn("exit-code.txt", calls)
        self.assertIn("child-pid.txt", calls)
        self.assertIn("trap cleanup TERM INT", calls)
        self.assertIn('kill \"$child\"', calls)

    def test_read_only_reaches_ssh_launch_without_approval_bypass(self):
        result = SimpleNamespace(returncode=0, stdout=b"42\n", stderr=b"")
        with patch.object(department.uuid, "uuid4", return_value=SimpleNamespace(hex="abc123")), \
             patch.object(department, "ssh", return_value=result) as ssh, \
             patch.object(department, "ledger_append"):
            department.dispatch_task("/project", b"prompt", True, read_only=True)
        launch = str(ssh.call_args_list[-1].args[0])
        self.assertIn("--sandbox read-only", launch)
        self.assertIn("--ignore-user-config --ignore-rules", launch)
        self.assertNotIn("--approve-for-me", launch)

    def test_read_only_prompt_omits_relay_announcement(self):
        result = SimpleNamespace(returncode=0, stdout=b"42\n", stderr=b"")
        with patch.object(department.uuid, "uuid4",
                          return_value=SimpleNamespace(hex="abc123")), \
             patch.object(department, "setup_task_dir", return_value="/remote/t") as setup, \
             patch.object(department, "ssh", return_value=result), \
             patch.object(department, "ledger_append"):
            department.dispatch_task("/project", b"review prompt", True,
                                     read_only=True)
        self.assertEqual(setup.call_args.args[2], b"review prompt")

    def test_invalid_model_is_rejected_before_remote_setup(self):
        with patch.object(department, "ssh") as ssh, self.assertRaises(SystemExit):
            department.dispatch_task("/project", b"prompt", False,
                                     model="model --approve-for-me")
        ssh.assert_not_called()

    def test_model_reaches_relay_for_resume(self):
        result = SimpleNamespace(returncode=0, stdout=b"", stderr=b"")
        with patch.object(department.uuid, "uuid4", return_value=SimpleNamespace(hex="abc123")), \
             patch.object(department, "ssh", return_value=result) as ssh, \
             patch.object(department, "zigzag_spawn", return_value="proc") as spawn, \
             patch.object(department, "ledger_append"):
            department.dispatch_task("/project", b"prompt", False, "session", "test-model")
            setup = "\n".join(str(call.args[0]) for call in ssh.call_args_list)
        self.assertEqual(spawn.call_args.args[1][0], "resume")
        self.assertIn("model.txt", setup)

    def test_status_uses_one_relay_poll_for_liveness_and_exit_diagnostic(self):
        entry = {"id": "t-one", "via": "relay", "proc": "proc"}
        with patch.object(department, "ledger_read", return_value=[entry]), \
             patch.object(department, "zigzag_poll", return_value={"running": False, "exit_code": 7}) as poll, \
             patch("builtins.print") as output:
            department.cmd_status(["t-one"])
        poll.assert_called_once_with("proc")
        output.assert_called_once_with("t-one: DONE (exit 7)")

    def test_status_distinguishes_pruned_relay_from_unknown_exit(self):
        entry = {"id": "t-one", "via": "relay", "proc": "proc"}
        with patch.object(department, "ledger_read", return_value=[entry]), \
             patch.object(department, "zigzag_poll", return_value=None), \
             patch("builtins.print") as output:
            department.cmd_status(["t-one"])
        output.assert_called_once_with("t-one: DONE (pruned)")

    def test_status_reports_ssh_exit_code(self):
        entry = {"id": "t-one", "via": "ssh"}
        result = SimpleNamespace(returncode=0, stdout=b"DONE 7\n", stderr=b"")
        with patch.object(department, "ledger_read", return_value=[entry]), \
             patch.object(department, "ssh", return_value=result), \
             patch("builtins.print") as output:
            department.cmd_status(["t-one"])
        output.assert_called_once_with("t-one: DONE (exit 7)")

    def test_kill_stops_ssh_wrapper_and_tracked_child(self):
        entry = {"id": "t-one", "via": "ssh"}
        result = SimpleNamespace(returncode=0, stdout=b"killed\n", stderr=b"")
        with patch.object(department, "ledger_read", return_value=[entry]), \
             patch.object(department, "ssh", return_value=result) as ssh, \
             patch("builtins.print") as output:
            department.cmd_kill(["t-one"])
        command = ssh.call_args.args[0]
        self.assertIn("child-pid.txt", command)
        self.assertIn("child-pid.txt pid", command)
        output.assert_called_once_with("t-one: killed (via ssh)")

    def test_result_prints_relay_failure_and_task_stderr(self):
        entry = {"id": "t-one", "via": "relay", "proc": "proc"}
        result = SimpleNamespace(stdout=b"final message\n", stderr=b"")
        stderr = SimpleNamespace(stdout=b"launcher failure\n", stderr=b"")
        tokens = SimpleNamespace(stdout=b'"total_tokens":3\n', stderr=b"")
        with patch.object(department, "ledger_read", return_value=[entry]), \
             patch.object(department, "remote_status_detail", return_value=("DONE", {"exit_code": 7, "stderr": "relay failure"})), \
             patch.object(department, "ssh", side_effect=[result, stderr, tokens]), \
             patch("builtins.print") as output:
            department.cmd_result(["t-one"])
        rendered = "\n".join(str(c.args[0]) for c in output.call_args_list)
        self.assertIn("relay exit_code: 7", rendered)
        self.assertIn("relay failure", rendered)
        self.assertIn("launcher failure", rendered)

    def test_result_reports_pruned_relay_and_ssh_stderr(self):
        relay = {"id": "t-relay", "via": "relay", "proc": "proc"}
        ssh_entry = {"id": "t-ssh", "via": "ssh"}
        response = SimpleNamespace(stdout=b"", stderr=b"")
        with patch.object(department, "ledger_read", return_value=[relay]), \
             patch.object(department, "remote_status_detail", return_value=("DONE", None)), \
             patch.object(department, "ssh", side_effect=[response, response, response]), \
             patch("builtins.print") as output:
            department.cmd_result(["t-relay"])
        rendered = "\n".join(str(c.args[0]) for c in output.call_args_list)
        self.assertIn("stderr.log (tail):", rendered)
        with patch.object(department, "ledger_read", return_value=[ssh_entry]), \
             patch.object(department, "remote_status_detail", return_value=("DONE", None)), \
             patch.object(department, "ssh", side_effect=[response, response, response]), \
             patch("builtins.print") as output:
            department.cmd_result(["t-ssh"])
        self.assertIn("--- diagnostics ---", "\n".join(str(c.args[0]) for c in output.call_args_list))


if __name__ == "__main__":
    unittest.main()
