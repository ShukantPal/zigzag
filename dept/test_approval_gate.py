import importlib
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
        with patch.object(gate, "relay_call", return_value=expected) as run:
            self.assertEqual(gate.fetch_gate("owner/repo", "17"), expected)
        run.assert_called_once_with(
            "/v1/review-gate?repository=owner%2Frepo&pull_request=17")


if __name__ == "__main__":
    unittest.main()
