import importlib
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
config = importlib.import_module("config")


class ConfigValidationTest(unittest.TestCase):
    def setUp(self):
        self.review_policy = dict(config.REVIEW_POLICY)
        self.loop_ownership = list(config.LOOP_OWNERSHIP)
        self.doc_routes = list(config.DOC_ROUTES)

    def tearDown(self):
        config.REVIEW_POLICY.clear()
        config.REVIEW_POLICY.update(self.review_policy)
        config.LOOP_OWNERSHIP[:] = self.loop_ownership
        config.DOC_ROUTES[:] = self.doc_routes

    def test_unknown_lens_is_rejected(self):
        config.REVIEW_POLICY["leveled"] = config.ReviewPolicy(2, 2, ["unknown"], False)

        self.assertIn("leveled: unknown lenses ['unknown']", config.validate())

    def test_zigzag_requires_security_lens(self):
        config.REVIEW_POLICY["zigzag"] = config.ReviewPolicy(
            2, 2, ["correctness", "tests"], True
        )

        self.assertIn(
            "zigzag: security lens required but missing", config.validate()
        )

    def test_duplicate_loop_ownership_is_rejected(self):
        config.LOOP_OWNERSHIP.append(config.LoopOwnership("doc-router", "vm"))

        self.assertIn("duplicate loop ownership entries", config.validate())

    def test_doc_route_without_session_is_rejected(self):
        config.DOC_ROUTES.append(config.DocRoute("another-doc", ""))

        self.assertIn("doc another-doc: no assigned session", config.validate())


class ConfigCheckTest(unittest.TestCase):
    def test_check_passes_for_fresh_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            artifact = pathlib.Path(directory) / "config.materialized.json"
            artifact.write_text(config.materialize())
            with patch.object(config, "MATERIALIZED_PATH", artifact):
                self.assertEqual(config.main(["--check"]), 0)

    def test_check_fails_after_artifact_is_hand_edited(self):
        with tempfile.TemporaryDirectory() as directory:
            artifact = pathlib.Path(directory) / "config.materialized.json"
            artifact.write_text(config.materialize() + "hand edit\n")
            with patch.object(config, "MATERIALIZED_PATH", artifact):
                self.assertEqual(config.main(["--check"]), 1)


if __name__ == "__main__":
    unittest.main()
