import importlib
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
dispatcher = importlib.import_module("dispatch_review_round")
round_state = importlib.import_module("round_state")


class DispatchReviewRoundTest(unittest.TestCase):
    def setUp(self):
        self.real_create_review_snapshot = dispatcher.create_review_snapshot
        def snapshot(_project, head, repo, pr, number):
            return (f"/snapshot/{dispatcher.repo_key(repo)}-pr{pr}-"
                    f"round{number}-{head[:12]}")

        snapshot_patch = patch.object(dispatcher, "create_review_snapshot",
                                      side_effect=snapshot)
        root = patch.object(dispatcher, "SNAPSHOT_ROOT", "/snapshot")
        snapshot_patch.start()
        root.start()
        self.addCleanup(snapshot_patch.stop)
        self.addCleanup(root.stop)

    def test_pr_info_scopes_gh_to_repo(self):
        result = '{"headRefOid": "a"}'
        with patch.object(dispatcher, "mac", return_value=result) as mac:
            dispatcher.pr_info(12, "owner/repo")
        self.assertEqual(mac.call_args.args[0],
                         "gh pr view 12 --repo owner/repo --json headRefOid")

    def test_superseded_rounds_do_not_count_or_consume_numbers(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            key = dispatcher.repo_key("owner/repo")
            (path / f"{key}-pr12-round1.json").write_text(json.dumps({"repo": "owner/repo", "round": 1, "status": "superseded"}))
            (path / f"{key}-pr12-round2.json").write_text(json.dumps({"repo": "owner/repo", "round": 2, "status": "collecting"}))
            with patch.object(dispatcher, "ROUNDS_DIR", path):
                self.assertEqual(len(dispatcher.round_files("owner/repo", 12)), 1)
                self.assertEqual(dispatcher.next_round_number("owner/repo", 12), 3)

    def test_seed_never_overwrites_a_superseded_round_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            rounds = root / "rounds"; rounds.mkdir()
            key = dispatcher.repo_key("owner/repo")
            first = rounds / f"{key}-pr12-round1.json"
            first.write_text(json.dumps({"repo": "owner/repo", "round": 1,
                                         "status": "superseded"}))
            with patch.object(dispatcher, "ROUNDS_DIR", rounds), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info",
                              return_value={"headRefOid": "f" * 40}), \
                 patch.object(dispatcher, "dispatch_reviewer",
                              side_effect=lambda _p, _f, planned: planned):
                second, _ = dispatcher.seed(12, "owner/repo", "/work")
            first_status = json.loads(first.read_text())["status"]
        self.assertEqual(second.name, f"{key}-pr12-round2.json")
        self.assertEqual(first_status, "superseded")

    def test_round_state_writes_replace_atomically(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "round.json"
            path.write_text('{"old": true}')
            with patch.object(round_state.os, "replace",
                              wraps=round_state.os.replace) as replace:
                dispatcher.write_round(path, {"new": True})
            source, destination = replace.call_args.args
            self.assertNotEqual(pathlib.Path(source), path)
            self.assertEqual(pathlib.Path(destination), path)
            self.assertEqual(json.loads(path.read_text()), {"new": True})
            self.assertEqual(list(path.parent.glob(f".{path.name}.*")), [])

    def test_matching_corrupt_round_state_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            rounds = pathlib.Path(tmp)
            key = dispatcher.repo_key("owner/repo")
            (rounds / f"{key}-pr12-round2.json").write_text("{")
            with patch.object(dispatcher, "ROUNDS_DIR", rounds), \
                 self.assertRaisesRegex(RuntimeError,
                                       "cannot read review round state"):
                dispatcher.stored_rounds("owner/repo", 12)

    def test_prompt_uses_checked_out_diff_not_untrusted_pr_metadata(self):
        prompt = dispatcher.FULL_TEMPLATE.format(pr=12, repo="owner/repo", project_dir="/work",
                                                 lens="tests", head="a" * 40)
        self.assertIn("verify `.review-head`", prompt)
        self.assertIn("inspect `.review.diff`", prompt)
        self.assertIn("Do not read the PR body", prompt)
        self.assertIn("Do not invoke `gh`", prompt)

    def test_seed_persists_each_reviewer_and_marks_failure_attention(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "a" * 40}
            launches = 0

            def dispatch(_project, _prompt, planned):
                nonlocal launches
                state = json.loads(next((root / "rounds").glob("*.json")).read_text())
                self.assertEqual(state["status"], "dispatching")
                self.assertEqual(set(state["reviewers"].values()),
                                 {"correctness", "simplicity", "tests"})
                launches += 1
                if launches == 2:
                    raise RuntimeError("nope")
                return planned

            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer", side_effect=dispatch):
                with self.assertRaisesRegex(RuntimeError, "nope"):
                    dispatcher.seed(12, "owner/repo", "/work")
            data = json.loads(next((root / "rounds").glob("*.json")).read_text())
        self.assertEqual(data["status"], "attention")
        self.assertEqual(set(data["reviewers"].values()),
                         {"correctness", "simplicity", "tests"})

    def test_snapshot_is_pinned_to_head_without_using_the_live_checkout(self):
        with patch.object(dispatcher, "SNAPSHOT_ROOT", "/snapshots"), \
             patch.object(dispatcher, "mac", return_value="") as mac:
            path = self.real_create_review_snapshot(
                "/live checkout", "a" * 40, "owner/repo", 12, 3)
        self.assertEqual(
            path,
            f"/snapshots/{dispatcher.repo_key('owner/repo')}-pr12-round3-{'a' * 12}")
        command = mac.call_args.args[0]
        self.assertIn("source='/live checkout'", command)
        self.assertIn(f"head={'a' * 40}", command)
        self.assertIn('git -C "$source" archive', command)
        self.assertIn('git -C "$source" diff --binary origin/main...', command)
        self.assertIn(".review-head", command)

    def test_snapshot_materializes_exact_committed_source_and_diff(self):
        repo = DEPT_DIR.parent
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=repo, check=True,
            capture_output=True, text=True).stdout.strip()

        def local_mac(command):
            subprocess.run(["sh", "-c", command], check=True,
                           capture_output=True, text=True)
            return ""

        with tempfile.TemporaryDirectory() as tmp, \
             patch.object(dispatcher, "SNAPSHOT_ROOT", tmp), \
             patch.object(dispatcher, "mac", side_effect=local_mac):
            snapshot = pathlib.Path(self.real_create_review_snapshot(
                str(repo), head, "owner/repo", 12, 3))
            self.assertEqual((snapshot / ".review-head").read_text().strip(), head)
            self.assertTrue((snapshot / ".review.diff").is_file())
            self.assertEqual(
                (snapshot / "dept" / "dispatch_review_round.py").read_bytes(),
                subprocess.run(
                    ["git", "show", f"{head}:dept/dispatch_review_round.py"],
                    cwd=repo, check=True, capture_output=True).stdout)

    def test_snapshot_rejects_non_commit_identifier(self):
        with patch.object(dispatcher, "mac") as mac, \
             self.assertRaisesRegex(RuntimeError, "invalid review head"):
            self.real_create_review_snapshot(
                "/work", "main; touch /tmp/oops", "owner/repo", 12, 3)
        mac.assert_not_called()

    def test_seed_records_all_reviewers_and_repo_scoped_prompts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "b" * 40}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer",
                              side_effect=lambda _p, _f, planned: planned):
                path, reviewers = dispatcher.seed(12, "owner/repo", "/work")
            data = json.loads(path.read_text())
            prompts = [prompt.read_text() for prompt in (root / "prompts").glob("*.md")]
        self.assertEqual(data["status"], "collecting")
        self.assertEqual(set(reviewers.values()), {"correctness", "simplicity", "tests"})
        self.assertEqual(data["head"], "b" * 40)
        self.assertEqual(data["source_project_dir"], "/work")
        self.assertEqual(data["project_dir"],
                         f"/snapshot/{dispatcher.repo_key('owner/repo')}-pr12-"
                         f"round1-{'b' * 12}")
        self.assertEqual(len(prompts), 3)
        self.assertTrue(all("Do not invoke `gh`" in prompt for prompt in prompts))

    def test_same_number_different_repositories_use_distinct_round_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "c" * 40}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer",
                              side_effect=lambda _p, _f, planned: planned):
                one, _ = dispatcher.seed(12, "owner/one", "/work")
                two, _ = dispatcher.seed(12, "owner/two", "/work")
        self.assertNotEqual(one.name, two.name)

    def test_round_seed_lock_rejects_a_competing_file_description(self):
        with tempfile.TemporaryDirectory() as tmp:
            rounds = pathlib.Path(tmp) / "rounds"
            with patch.object(dispatcher, "ROUNDS_DIR", rounds), \
                 dispatcher.round_seed_lock("owner/repo", 12):
                lock_path = rounds / (
                    f"{dispatcher.repo_key('owner/repo')}-pr12.seed.lock")
                with lock_path.open("w") as competitor:
                    with self.assertRaises(BlockingIOError):
                        dispatcher.fcntl.flock(
                            competitor,
                            dispatcher.fcntl.LOCK_EX | dispatcher.fcntl.LOCK_NB)

    def test_reviewer_dispatch_is_read_only_and_non_gui(self):
        result = type("R", (), {"stdout": "started t-abc123", "stderr": ""})()
        with patch.object(dispatcher, "run", return_value=result) as run:
            self.assertEqual(dispatcher.dispatch_reviewer(
                "/work", pathlib.Path("/prompt"), "t-abc123"), "t-abc123")
        self.assertEqual(run.call_args.args[-4:],
                         ("--read-only", "--ssh", "--task-id", "t-abc123"))

    def test_seed_can_include_security_lens(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "d" * 40}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer",
                              side_effect=lambda _p, _f, planned: planned):
                _, reviewers = dispatcher.seed(12, "owner/repo", "/work",
                                               lenses=dispatcher.LENSES + ("security",))
        self.assertEqual(set(reviewers.values()), {"correctness", "simplicity", "tests", "security"})


if __name__ == "__main__":
    unittest.main()
