import importlib
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch


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

    def test_folder_discovery_filters_to_google_docs_and_includes_extra_share(self):
        client = watcher.DriveClient("token")
        with patch.object(client, "request", side_effect=[
            {"files": [
                {"id": "doc", "name": "Folder doc", "mimeType": watcher.GOOGLE_DOC_MIME},
                {"id": "pdf", "name": "Ignore", "mimeType": "application/pdf"},
            ]},
            {"id": "extra", "name": "Individual doc", "mimeType": watcher.GOOGLE_DOC_MIME},
        ]) as request:
            docs = client.list_documents("folder", ["extra"])
        self.assertEqual([d["id"] for d in docs], ["doc", "extra"])
        self.assertIn("'folder' in parents", request.call_args_list[0].kwargs["params"]["q"])

    def test_watermark_is_namespaced_by_document(self):
        with tempfile.TemporaryDirectory() as tmp:
            db = watcher.connect_database(str(pathlib.Path(tmp) / "dept.db"))
            watcher.mark_seen(db, "one", "same-comment")
            db.commit()
            self.assertTrue(watcher.comment_seen(db, "one", "same-comment"))
            self.assertFalse(watcher.comment_seen(db, "two", "same-comment"))
            db.close()

    def test_quiet_hours_follow_pacific_time(self):
        tz = watcher.ZoneInfo("America/Los_Angeles")
        self.assertTrue(watcher.quiet_hours(watcher.datetime.datetime(2026, 9, 28, 22, 0, tzinfo=tz)))
        self.assertTrue(watcher.quiet_hours(watcher.datetime.datetime(2026, 9, 29, 6, 59, tzinfo=tz)))
        self.assertFalse(watcher.quiet_hours(watcher.datetime.datetime(2026, 9, 28, 14, 0, tzinfo=tz)))


if __name__ == "__main__":
    unittest.main()
