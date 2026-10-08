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
        {"body": f"🤖 Codex (AI assistant) — [{lens}] review verdict\nVERDICT: APPROVE\nHEAD: {HEAD}\nATTESTATION: HUMAN",
         "createdAt": "2026-01-01T00:00:00Z", "id": lens, "author": "ShukantPal"}
        for lens in ("correctness", "simplicity", "tests")
    ]


class ApprovalGateChecksTest(unittest.TestCase):
    def result_for(self, checks):
        return {"head": HEAD, "comments": approvals(), "checks": checks}

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
                f"VERDICT: APPROVE\\nHEAD: {HEAD}\\nATTESTATION: HUMAN")
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

    def test_model_advisory_approve_requires_human_attestation(self):
        comments = approvals()
        comments[0]["body"] = comments[0]["body"].replace(
            "ATTESTATION: HUMAN", "ATTESTATION: MODEL_ADVISORY")
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["comments"] = comments
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("[correctness] APPROVE is advisory; human attestation required",
                      result["reasons"])

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
                f"VERDICT: APPROVE\nVERDICT: CHANGES REQUESTED\nHEAD: {HEAD}\n"
                "ATTESTATION: HUMAN")
        self.assertNotIn("tests", gate.latest_verdicts([
            {"body": body, "createdAt": "now", "id": 1, "author": "ShukantPal"}
        ]))


if __name__ == "__main__":
    unittest.main()
