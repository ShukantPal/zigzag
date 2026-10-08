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
         "createdAt": "2026-01-01T00:00:00Z", "id": lens, "author": "reviewer"}
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
                f"VERDICT: APPROVE\\nHEAD: {HEAD}")
        verdicts = gate.latest_verdicts([{"body": body, "createdAt": "now", "id": 1,
                                          "author": "reviewer"}])
        self.assertEqual(verdicts["tests"][:2], ("APPROVE", HEAD))


if __name__ == "__main__":
    unittest.main()
