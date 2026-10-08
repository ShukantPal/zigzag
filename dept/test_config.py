import importlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
CONFIG_SOURCE = DEPT_DIR / "config.py"
ARTIFACT_PATH = DEPT_DIR / "config.materialized.json"
HOOK_SOURCE = DEPT_DIR.parent / "scripts" / "githooks" / "pre-commit"
INSTALLER_SOURCE = DEPT_DIR.parent / "scripts" / "install-hooks.sh"
sys.path.insert(0, str(DEPT_DIR))
config = importlib.import_module("config")


class ConfigValidationTest(unittest.TestCase):
    def setUp(self):
        self.loop_ownership = list(config.LOOP_OWNERSHIP)
        self.doc_routes = list(config.DOC_ROUTES)

    def tearDown(self):
        config.LOOP_OWNERSHIP[:] = self.loop_ownership
        config.DOC_ROUTES[:] = self.doc_routes

    def test_duplicate_loop_ownership_is_rejected(self):
        config.LOOP_OWNERSHIP.append(config.LoopOwnership("doc-router", "vm"))

        self.assertIn("duplicate loop ownership entries", config.validate())

    def test_doc_route_without_session_is_rejected(self):
        config.DOC_ROUTES.append(config.DocRoute("another-doc", ""))

        self.assertIn("doc another-doc: no assigned session", config.validate())

    def test_materialize_enforces_validation(self):
        config.DOC_ROUTES.append(config.DocRoute("invalid-doc", ""))

        with self.assertRaisesRegex(SystemExit, "no assigned session"):
            config.materialize()


class ConfigMaterializationTest(unittest.TestCase):
    def test_committed_artifact_has_the_daemon_contract(self):
        payload = json.loads(ARTIFACT_PATH.read_text())

        self.assertEqual(
            set(payload),
            {
                "design_docs_folder_id", "doc_router_watch_list", "doc_routes",
                "generated_by", "loop_ownership", "quiet_hours", "repos",
                "service_account", "watchers",
            },
        )
        self.assertEqual(payload["repos"], {
            "zigzag": "ShukantPal/zigzag", "leveled": "leveled-inc/leveled",
        })
        self.assertEqual(payload["service_account"], "zigzag@shukant.iam.gserviceaccount.com")
        self.assertEqual(payload["design_docs_folder_id"], "1W_iTcpdYGVXj_NTmkfcgOm_GGREk1Nj3")
        self.assertNotIn("review_policy", payload)
        self.assertNotIn(
            "review-rounds", [entry["loop"] for entry in payload["loop_ownership"]]
        )
        self.assertNotIn(
            "merge-killer", [entry["name"] for entry in payload["watchers"]]
        )
        self.assertEqual(
            payload["doc_router_watch_list"],
            [route["doc_id"] for route in payload["doc_routes"]],
        )

    def test_materialization_is_deterministic(self):
        self.assertEqual(config.materialize(), config.materialize())
        self.assertEqual(config.materialize(), ARTIFACT_PATH.read_text())


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


class HookTest(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.repo = pathlib.Path(self.temporary_directory.name) / "repo"
        self.repo.mkdir()
        self.git(["init", "-q"])
        self.git(["config", "user.email", "codex@example.test"])
        self.git(["config", "user.name", "Codex Test"])
        (self.repo / "dept").mkdir()
        (self.repo / "scripts" / "githooks").mkdir(parents=True)
        shutil.copy2(CONFIG_SOURCE, self.repo / "dept" / "config.py")
        shutil.copy2(HOOK_SOURCE, self.repo / "scripts" / "githooks" / "pre-commit")
        shutil.copy2(INSTALLER_SOURCE, self.repo / "scripts" / "install-hooks.sh")
        rendered = self.command([sys.executable, "dept/config.py", "--materialize"]).stdout
        (self.repo / "dept" / "config.materialized.json").write_text(rendered)
        self.git(["add", "dept/config.py", "dept/config.materialized.json", "scripts"])
        self.git(["commit", "-qm", "initial config"])

    def tearDown(self):
        self.temporary_directory.cleanup()

    def command(self, args):
        return subprocess.run(
            args, cwd=self.repo, text=True, capture_output=True, check=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull},
        )

    def git(self, args):
        return self.command(["git", *args])

    def staged_names(self):
        return self.git(["diff", "--cached", "--name-only"]).stdout.splitlines()

    def test_hook_materializes_the_staged_source_not_working_tree(self):
        config_path = self.repo / "dept" / "config.py"
        staged_source = config_path.read_text().replace('"start": "22:00"', '"start": "21:00"')
        config_path.write_text(staged_source)
        self.git(["add", "dept/config.py"])
        config_path.write_text(staged_source.replace('"start": "21:00"', '"start": "20:00"'))

        self.command([str(self.repo / "scripts" / "githooks" / "pre-commit")])

        artifact = json.loads(self.git(["show", ":dept/config.materialized.json"]).stdout)
        self.assertEqual(artifact["quiet_hours"]["start"], "21:00")
        self.assertEqual(self.staged_names(), ["dept/config.materialized.json", "dept/config.py"])

    def test_hook_skips_unrelated_staged_changes(self):
        (self.repo / "README.md").write_text("unrelated\n")
        self.git(["add", "README.md"])

        self.command([str(self.repo / "scripts" / "githooks" / "pre-commit")])

        self.assertEqual(self.staged_names(), ["README.md"])

    def test_hook_fails_open_when_staged_config_is_invalid(self):
        config_path = self.repo / "dept" / "config.py"
        config_path.write_text(config_path.read_text().replace(
            'LoopOwnership("dependabot", "mac"),',
            'LoopOwnership("doc-router", "mac"),',
        ))
        self.git(["add", "dept/config.py"])

        result = self.command([str(self.repo / "scripts" / "githooks" / "pre-commit")])

        self.assertIn("warning: unable to materialize", result.stderr)
        self.assertEqual(self.staged_names(), ["dept/config.py"])

    def test_installer_places_the_hook_at_git_resolved_path(self):
        self.git(["config", "core.hooksPath", "custom-hooks"])
        self.command(["sh", "scripts/install-hooks.sh"])

        hook_path = pathlib.Path(self.git(["rev-parse", "--git-path", "hooks/pre-commit"]).stdout.strip())
        if not hook_path.is_absolute():
            hook_path = self.repo / hook_path
        self.assertTrue(hook_path.is_file())
        self.assertTrue(os.access(hook_path, os.X_OK))
        self.assertEqual(hook_path.read_text(), HOOK_SOURCE.read_text())


if __name__ == "__main__":
    unittest.main()
