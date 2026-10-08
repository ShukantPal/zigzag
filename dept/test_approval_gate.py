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
        {"body": f"🤖 Codex (AI assistant) — [{lens}] review verdict\nVERDICT: APPROVE\nHEAD: {HEAD}\nROUND: 1\nLENSES: correctness,simplicity,tests",
         "createdAt": "2026-01-01T00:00:00Z", "id": lens, "author": "ShukantPal"}
        for lens in ("correctness", "simplicity", "tests")
    ]


class ApprovalGateChecksTest(unittest.TestCase):
    def setUp(self):
        self.real_latest_seeded_round = gate.latest_seeded_round
        actors = patch.object(gate, "human_review_actors",
                              return_value=frozenset({"HumanReviewer"}))
        actors.start()
        self.addCleanup(actors.stop)
        seeded = patch.object(gate, "latest_seeded_round", return_value=None)
        seeded.start()
        self.addCleanup(seeded.stop)

    def result_for(self, checks):
        return {
            "head": HEAD,
            "comments": approvals(),
            "reviews": [{"author": "HumanReviewer", "state": "APPROVED",
                         "commit": HEAD, "submittedAt": "2026-01-01T01:00:00Z",
                         "id": 1}],
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
                f"VERDICT: APPROVE\\nHEAD: {HEAD}\\nROUND: 1\\n"
                "LENSES: correctness,simplicity,tests")
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
                                "id": 2})
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])

    def test_later_dismissal_revokes_human_approval(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["reviews"].append({"author": "HumanReviewer", "state": "DISMISSED",
                                "commit": HEAD, "submittedAt": "2026-01-01T02:00:00Z",
                                "id": 2})
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("no current-head APPROVED review from an allowlisted human",
                      result["reasons"])

    def test_same_timestamp_uses_numeric_review_id_order(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["reviews"].append({"author": "HumanReviewer",
                                "state": "CHANGES_REQUESTED", "commit": HEAD,
                                "submittedAt": "2026-01-01T01:00:00Z", "id": 2})
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])

    def test_gate_never_mixes_approvals_across_rounds(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["comments"][2]["body"] = data["comments"][2]["body"].replace(
            "ROUND: 1", "ROUND: 2")
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1", ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("[correctness] verdict is from round 1; latest round is 2",
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
                "ROUND: 1\nLENSES: correctness,simplicity,tests")
        self.assertNotIn("tests", gate.latest_verdicts([
            {"body": body, "createdAt": "now", "id": 1, "author": "ShukantPal"}
        ]))

    def test_higher_round_wins_even_when_older_round_posts_later(self):
        newer_round = {
            "body": (f"🤖 Codex (AI assistant) — [tests] review verdict\n"
                     f"VERDICT: CHANGES REQUESTED\nHEAD: {HEAD}\nROUND: 2\n"
                     "LENSES: correctness,simplicity,tests"),
            "createdAt": "2026-01-01T00:00:00Z", "id": "round-2",
            "author": "ShukantPal",
        }
        late_old_round = {
            "body": (f"🤖 Codex (AI assistant) — [tests] review verdict\n"
                     f"VERDICT: APPROVE\nHEAD: {HEAD}\nROUND: 1\n"
                     "LENSES: correctness,simplicity,tests"),
            "createdAt": "2026-01-01T01:00:00Z", "id": "round-1-late",
            "author": "ShukantPal",
        }
        verdict = gate.latest_verdicts([newer_round, late_old_round])["tests"]
        self.assertEqual(verdict, ("CHANGES REQUESTED", HEAD, "ShukantPal", 2,
                                   ("correctness", "simplicity", "tests")))

    def test_seeded_security_lens_cannot_be_omitted_from_default_gate(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        for comment in data["comments"]:
            comment["body"] = comment["body"].replace(
                "LENSES: correctness,simplicity,tests",
                "LENSES: correctness,security,simplicity,tests")
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1",
                                ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("no verdict from [security] reviewer", result["reasons"])

    def test_conflicting_round_manifests_fail_closed(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        data["comments"][0]["body"] = data["comments"][0]["body"].replace(
            "LENSES: correctness,simplicity,tests",
            "LENSES: correctness,security,simplicity,tests")
        with patch.object(gate, "fetch_pr", return_value=data):
            result = gate.check("owner/repo", "1",
                                ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertIn("latest review round has no single consistent lens manifest",
                      result["reasons"])

    def test_new_seeded_round_blocks_prior_same_head_approvals(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        seeded = {"round": 2, "head": HEAD,
                  "lenses": ("correctness", "security", "simplicity", "tests"),
                  "status": "collecting"}
        with patch.object(gate, "fetch_pr", return_value=data), \
             patch.object(gate, "latest_seeded_round", return_value=seeded):
            result = gate.check("owner/repo", "1",
                                ["correctness", "simplicity", "tests"])
        self.assertFalse(result["pass"])
        self.assertEqual(result["review_round"], 2)
        self.assertEqual(result["seeded_round"], seeded)
        self.assertIn("latest seeded review round is collecting", result["reasons"])
        self.assertIn("no verdict from [security] reviewer", result["reasons"])

    def test_published_seeded_round_with_matching_approvals_passes(self):
        data = self.result_for([
            {"name": "semgrep", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "BuildBuddy", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ])
        seeded = {"round": 1, "head": HEAD,
                  "lenses": ("correctness", "simplicity", "tests"),
                  "status": "published"}
        with patch.object(gate, "fetch_pr", return_value=data), \
             patch.object(gate, "latest_seeded_round", return_value=seeded):
            result = gate.check("owner/repo", "1",
                                ["correctness", "simplicity", "tests"])
        self.assertTrue(result["pass"])

    def test_latest_seeded_round_reads_all_statuses(self):
        rounds = [
            (None, {"round": 1, "head": "a" * 40, "status": "published",
                    "reviewers": {"t-a": "tests"}}),
            (None, {"round": 2, "head": "b" * 40, "status": "attention",
                    "reviewers": {"t-b": "security", "t-c": "correctness"}}),
        ]
        with patch.object(gate, "stored_rounds", return_value=rounds):
            latest = self.real_latest_seeded_round("owner/repo", "1")
        self.assertEqual(latest, {
            "round": 2, "head": "b" * 40,
            "lenses": ("correctness", "security"), "status": "attention",
        })


class HumanActorConfigTest(unittest.TestCase):
    def test_shared_automation_actor_is_excluded(self):
        config = {"approval_gate": {
            "human_review_actors": ["ShukantPal", "HumanReviewer"],
        }}
        with patch.object(gate, "CONFIG", config):
            self.assertEqual(gate.human_review_actors(), frozenset({"HumanReviewer"}))


if __name__ == "__main__":
    unittest.main()
