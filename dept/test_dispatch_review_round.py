import importlib
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
dispatcher = importlib.import_module("dispatch_review_round")


class DispatchReviewRoundTest(unittest.TestCase):
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

    def test_prompt_uses_checked_out_diff_not_untrusted_pr_metadata(self):
        prompt = dispatcher.FULL_TEMPLATE.format(pr=12, repo="owner/repo", project_dir="/work",
                                                 lens="tests", head="a" * 40)
        self.assertIn("git diff origin/main...HEAD", prompt)
        self.assertIn("Do not read the PR body", prompt)
        self.assertIn("Do not invoke `gh`", prompt)

    def test_seed_persists_each_reviewer_and_marks_failure_attention(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "a" * 40}
            launches = 0

            def dispatch(_project, _prompt, planned):
                nonlocal launches
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
