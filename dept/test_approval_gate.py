import importlib
import json
import pathlib
import sys
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
gate = importlib.import_module("approval_gate")


class ApprovalGateChecksTest(unittest.TestCase):
    def test_fetch_gate_delegates_the_complete_decision_to_the_daemon(self):
        expected = {"pass": True, "head": "a" * 40, "approvals": {}}
        with patch.object(gate, "mac", return_value=json.dumps(expected)) as run:
            self.assertEqual(gate.fetch_gate("owner/repo", "17"), expected)
        run.assert_called_once_with("zigzag review-gate owner/repo 17")


if __name__ == "__main__":
    unittest.main()
