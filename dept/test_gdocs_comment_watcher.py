import importlib
import pathlib
import sys
import unittest


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
watcher = importlib.import_module("gdocs_comment_watcher")


class GdocsFeedbackTest(unittest.TestCase):
    def test_reply_feedback_is_flattened_with_its_thread_parent(self):
        thread = {
            "id": "parent", "content": "original", "author": {"displayName": "Someone"},
            "replies": [{"id": "reply", "content": "follow up",
                         "author": {"displayName": "Shukant Pal"}}],
        }
        items = list(watcher.feedback_items([thread]))
        self.assertEqual([item["id"] for item in items], ["parent", "reply"])
        self.assertEqual(items[1]["_parent_id"], "parent")
        self.assertIs(items[1]["_thread"], thread)

    def test_existing_ack_is_reused_for_dispatch_retry(self):
        thread = {"id": "parent", "replies": [
            {"id": "ack", "content": watcher.ACK_TEXT},
        ]}
        self.assertEqual(watcher.existing_ack({"_thread": thread}), "ack")


if __name__ == "__main__":
    unittest.main()
