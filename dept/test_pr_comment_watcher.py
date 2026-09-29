import importlib
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
watcher = importlib.import_module("pr_comment_watcher")


class WatermarkTest(unittest.TestCase):
    def test_save_unions_stale_seen_and_pr_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "watermark.json"
            path.write_text(json.dumps({"seen": ["ic:old"], "pr_state": {"1": "OPEN"}}))
            with patch.object(watcher, "WATERMARK", str(path)), \
                 patch.object(watcher, "STATE_DIR", tmp):
                watcher.save_watermark({"seen": ["rc:new"], "pr_state": {"2": "CLOSED"}})
            data = json.loads(path.read_text())
        self.assertEqual(set(data["seen"]), {"ic:old", "rc:new"})
        self.assertEqual(data["pr_state"], {"1": "OPEN", "2": "CLOSED"})


if __name__ == "__main__":
    unittest.main()
