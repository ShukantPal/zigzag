import importlib
import json
import pathlib
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
dispatcher = importlib.import_module("dispatch_review_round")


class DispatchReviewRoundTest(unittest.TestCase):
    def test_pr_info_scopes_gh_to_repo(self):
        result = SimpleNamespace(stdout='{"headRefOid": "a", "headRefName": "branch", "body": "why"}')
        with patch.object(dispatcher, "run", return_value=result) as run:
            dispatcher.pr_info(12, "owner/repo")
        self.assertEqual(run.call_args.args[:6], ("gh", "pr", "view", "12", "--repo", "owner/repo"))

    def test_superseded_rounds_do_not_count_or_consume_numbers(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp)
            (path / "12-round1.json").write_text(json.dumps({"round": 1, "status": "superseded"}))
            (path / "12-round2.json").write_text(json.dumps({"round": 2, "status": "collecting"}))
            with patch.object(dispatcher, "ROUNDS_DIR", path):
                self.assertEqual(len(dispatcher.round_files(12)), 1)
                self.assertEqual(dispatcher.next_round_number(12), 3)

    def test_prompt_keeps_repo_scope_and_design_rationale(self):
        prompt = dispatcher.FULL_TEMPLATE.format(pr=12, repo="owner/repo", project_dir="/work",
                                                 lens="tests", head="a" * 40, body="Intentional removal.")
        self.assertIn("gh pr view 12 --repo owner/repo", prompt)
        self.assertIn("flat diff only", prompt)
        self.assertIn("Intentional removal.", prompt)


if __name__ == "__main__":
    unittest.main()
