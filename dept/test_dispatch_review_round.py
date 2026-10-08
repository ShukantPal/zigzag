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
        result = '{"headRefOid": "a", "headRefName": "branch", "body": "why"}'
        with patch.object(dispatcher, "mac", return_value=result) as mac:
            dispatcher.pr_info(12, "owner/repo")
        self.assertEqual(mac.call_args.args[0],
                         "gh pr view 12 --repo owner/repo --json headRefOid,headRefName,body")

    def test_superseded_rounds_do_not_count_or_consume_numbers(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            key = dispatcher.repo_key("owner/repo")
            (path / f"{key}-pr12-round1.json").write_text(json.dumps({"repo": "owner/repo", "round": 1, "status": "superseded"}))
            (path / f"{key}-pr12-round2.json").write_text(json.dumps({"repo": "owner/repo", "round": 2, "status": "collecting"}))
            with patch.object(dispatcher, "ROUNDS_DIR", path):
                self.assertEqual(len(dispatcher.round_files("owner/repo", 12)), 1)
                self.assertEqual(dispatcher.next_round_number("owner/repo", 12), 3)

    def test_prompt_uses_checked_out_diff_not_untrusted_pr_metadata(self):
        prompt = dispatcher.FULL_TEMPLATE.format(pr=12, repo="owner/repo", project_dir="/work",
                                                 lens="tests", head="a" * 40)
        self.assertIn("git diff origin/main...HEAD", prompt)
        self.assertIn("Do not read the PR body", prompt)
        self.assertIn("Do not invoke `gh`", prompt)

    def test_seed_persists_each_reviewer_and_marks_failure_attention(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "a" * 40, "headRefName": "branch", "body": "why"}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer", side_effect=["t-a", RuntimeError("nope")]):
                with self.assertRaisesRegex(RuntimeError, "nope"):
                    dispatcher.seed(12, "owner/repo", "/work", "session")
            data = json.loads(next((root / "rounds").glob("*.json")).read_text())
        self.assertEqual(data["status"], "attention")
        self.assertEqual(data["reviewers"], {"t-a": "correctness"})

    def test_seed_records_all_reviewers_and_repo_scoped_prompts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "b" * 40, "headRefName": "fetched-branch", "body": "intent"}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer", side_effect=["t-c", "t-s", "t-t"]):
                path, reviewers = dispatcher.seed(12, "owner/repo", "/work", "session")
            data = json.loads(path.read_text())
            prompts = [prompt.read_text() for prompt in (root / "prompts").glob("*.md")]
        self.assertEqual(data["status"], "collecting")
        self.assertEqual(reviewers, {"t-c": "correctness", "t-s": "simplicity", "t-t": "tests"})
        self.assertEqual(data["head"], "b" * 40)
        self.assertEqual(data["branch"], "fetched-branch")
        self.assertEqual(len(prompts), 3)
        self.assertTrue(all("Do not invoke `gh`" in prompt for prompt in prompts))

    def test_same_number_different_repositories_use_distinct_round_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            info = {"headRefOid": "c" * 40, "headRefName": "branch"}
            with patch.object(dispatcher, "ROUNDS_DIR", root / "rounds"), \
                 patch.object(dispatcher, "PROMPT_DIR", root / "prompts"), \
                 patch.object(dispatcher, "pr_info", return_value=info), \
                 patch.object(dispatcher, "dispatch_reviewer", side_effect=["a", "b", "c", "d", "e", "f"]):
                one, _ = dispatcher.seed(12, "owner/one", "/work", "session")
                two, _ = dispatcher.seed(12, "owner/two", "/work", "session")
        self.assertNotEqual(one.name, two.name)

    def test_reviewer_dispatch_is_read_only_and_non_gui(self):
        result = type("R", (), {"stdout": "started t-review", "stderr": ""})()
        with patch.object(dispatcher, "run", return_value=result) as run:
            self.assertEqual(dispatcher.dispatch_reviewer("/work", pathlib.Path("/prompt")), "t-review")
        self.assertEqual(run.call_args.args[-2:], ("--read-only", "--ssh"))


if __name__ == "__main__":
    unittest.main()
