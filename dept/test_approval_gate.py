import importlib
import pathlib
import sys
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
gate = importlib.import_module("approval_gate")


HEAD = "a" * 40


def approvals():
    return [
        {"body": f"🤖 Codex (AI assistant) — [{lens}] review verdict\nVERDICT: APPROVE\nHEAD: {HEAD}",
         "createdAt": "2026-01-01T00:00:00Z", "id": lens, "author": "ShukantPal"}
        for lens in ("correctness", "simplicity", "tests")
    ]


class ApprovalGateChecksTest(unittest.TestCase):
    def setUp(self):
        actors = patch.object(gate, "human_review_actors",
                              return_value=frozenset({"HumanReviewer"}))
        actors.start()
        self.addCleanup(actors.stop)

    def result_for(self, checks):
        return {
            "head": HEAD,
            "comments": approvals(),
            "reviews": [{"author": "HumanReviewer", "state": "APPROVED",
                         "commit": HEAD, "submittedAt": "2026-01-01T01:00:00Z",
                         "id": "human-approval"}],
            "checks": checks,
        }

    def test_missing_required_check_fails_closed(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("required check missing: BuildBuddy", result["reasons"])

    def test_in_progress_required_check_fails_closed(self):
        data = self.result_for([
            {"name": "semgrep", "status": "IN_PROGRESS", "conclusion": None},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertTrue(any("semgrep" in reason for reason in result["reasons"]))

    def test_completed_successful_required_checks_pass(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertTrue(result["pass"])

    def test_literal_newlines_in_verdict_are_accepted(self):
        body = (f"🤖 Codex (AI assistant) — [tests] review verdict\\n"
                f"VERDICT: APPROVE\\nHEAD: {HEAD}")
        verdicts = gate.latest_verdicts([{"body": body, "createdAt": "now", "id": 1,
                                          "author": "ShukantPal"}])
        self.assertEqual(verdicts["tests"][:2], ("APPROVE", HEAD))

    def test_untrusted_marker_comment_cannot_satisfy_gate(self):
        comments = approvals()
        comments[0]["author"] = "attacker"
        with patch.object(gate, "fetch_pr", return_value=self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ]) | {"comments": comments}):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("no verdict from [correctness] reviewer", result["reasons"])

    def test_no_configured_human_reviewer_fails_closed(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        with patch.object(gate, "fetch_pr", return_value=data), \
             patch.object(gate, "human_review_actors", return_value=frozenset()):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("no separate human review actors configured", result["reasons"])

    def test_stale_human_approval_fails_closed(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["reviews"][0]["commit"] = "b" * 40
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("no current-head APPROVED review from an allowlisted human",
                      result["reasons"])

    def test_later_changes_requested_supersedes_human_approval(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["reviews"].append({"author": "HumanReviewer", "state": "CHANGES_REQUESTED",
                                "commit": HEAD, "submittedAt": "2026-01-01T02:00:00Z",
                                "id": "later-review"})
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])

    def test_changes_requested_blocks_gate(self):
        comments = approvals()
        comments[2]["body"] = comments[2]["body"].replace(
            "VERDICT: APPROVE", "VERDICT: CHANGES REQUESTED")
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["comments"] = comments
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("[tests] latest verdict is CHANGES REQUESTED (not APPROVE)",
                      result["reasons"])

    def test_conflicting_structured_verdict_is_ignored(self):
        body = (f"🤖 Codex (AI assistant) — [tests] review verdict\n"
                f"VERDICT: APPROVE\nVERDICT: CHANGES REQUESTED\nHEAD: {HEAD}")
        self.assertNotIn("tests", gate.latest_verdicts([
            {"body": body, "createdAt": "now", "id": 1, "author": "ShukantPal"}
        ]))


class HumanActorConfigTest(unittest.TestCase):
    def test_shared_automation_actor_is_excluded(self):
        config = {"approval_gate": {
            "human_review_actors": ["ShukantPal", "HumanReviewer"],
        }}
        with patch.object(gate, "CONFIG", config):
            self.assertEqual(gate.human_review_actors(), frozenset({"HumanReviewer"}))


if __name__ == "__main__":
    unittest.main()
