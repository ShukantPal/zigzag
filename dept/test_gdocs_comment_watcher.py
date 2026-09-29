import importlib
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
import urllib.error
import urllib.parse
from types import SimpleNamespace
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
watcher = importlib.import_module("gdocs_comment_watcher")


class FakeResponse(io.StringIO):
    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class FakeDrive:
    def __init__(self, comments):
        self.comments = comments
        self.acks = []
        self.skipped_documents = []

    def list_documents(self, folder_id, additional):
        return [{"id": "doc", "name": "Watched doc"}]

    def list_comments(self, doc_id):
        return self.comments

    def post_eyes_reply(self, doc_id, comment_id):
        self.acks.append((doc_id, comment_id))
        return f"ack-{len(self.acks)}"


def human_comment(comment_id, text="feedback"):
    return {
        "id": comment_id,
        "content": text,
        "author": {"permissionId": "shukant-id", "displayName": "Not Trusted By Name"},
        "createdTime": "2026-09-28T12:00:00Z",
        "replies": [],
    }


class GdocsFeedbackTest(unittest.TestCase):
    def test_reply_feedback_is_flattened_with_its_thread_parent(self):
        thread = {
            "id": "parent", "content": "original", "author": {"permissionId": "other"},
            "replies": [{"id": "reply", "content": "follow up",
                         "author": {"permissionId": "shukant-id"}}],
        }
        items = list(watcher.feedback_items([thread]))
        self.assertEqual([item["id"] for item in items], ["parent", "reply"])
        self.assertEqual(items[1]["_parent_id"], "parent")
        self.assertIs(items[1]["_thread"], thread)

    def test_ack_is_persisted_for_the_exact_comment(self):
        with tempfile.TemporaryDirectory() as tmp:
            db = watcher.connect_database(str(pathlib.Path(tmp) / "dept.db"))
            watcher.save_ack(db, "doc", "one", "ack-one")
            self.assertEqual(watcher.stored_ack(db, "doc", "one"), "ack-one")
            self.assertIsNone(watcher.stored_ack(db, "doc", "two"))
            db.close()

    def test_folder_discovery_paginates_and_skips_stale_extra_share(self):
        client = watcher.DriveClient("token")
        with patch.object(client, "request", side_effect=[
            {"nextPageToken": "next", "files": [
                {"id": "doc1", "name": "One", "mimeType": watcher.GOOGLE_DOC_MIME},
            ]},
            {"files": [
                {"id": "pdf", "name": "Ignore", "mimeType": "application/pdf"},
                {"id": "doc2", "name": "Two", "mimeType": watcher.GOOGLE_DOC_MIME},
            ]},
            watcher.DriveError("Drive GET files/stale failed (HTTP 404)"),
        ]) as request:
            docs = client.list_documents("folder", ["stale"])
        self.assertEqual([d["id"] for d in docs], ["doc1", "doc2"])
        self.assertEqual(client.skipped_documents[0][0], "stale")
        self.assertEqual(request.call_args_list[1].kwargs["params"]["pageToken"], "next")

    def test_comment_listing_paginates_without_shared_drive_parameter(self):
        client = watcher.DriveClient("token")
        with patch.object(client, "request", side_effect=[
            {"nextPageToken": "next", "comments": [{"id": "one"}]},
            {"comments": [{"id": "two"}]},
        ]) as request:
            comments = client.list_comments("doc")
        self.assertEqual([c["id"] for c in comments], ["one", "two"])
        self.assertNotIn("supportsAllDrives", request.call_args_list[0].kwargs["params"])
        self.assertEqual(request.call_args_list[1].kwargs["params"]["pageToken"], "next")

    def test_reply_creation_uses_only_supported_parameters(self):
        client = watcher.DriveClient("token")
        with patch.object(client, "request", return_value={"id": "ack"}) as request:
            self.assertEqual(client.post_eyes_reply("doc", "comment"), "ack")
        self.assertEqual(request.call_args.kwargs["params"], {"fields": "id"})
        self.assertEqual(request.call_args.kwargs["body"], {"content": watcher.ACK_TEXT})

    def test_drive_http_error_is_redacted_to_status(self):
        error = urllib.error.HTTPError("https://example.invalid", 403, "forbidden", {}, None)
        with patch.object(watcher.urllib.request, "urlopen", side_effect=error):
            with self.assertRaisesRegex(watcher.DriveError, "HTTP 403"):
                watcher.DriveClient("token").request("GET", "files")

    def test_task_status_is_fail_closed_when_indeterminate(self):
        failed = SimpleNamespace(returncode=1, stdout="", stderr="relay unavailable")
        with patch.object(watcher.subprocess, "run", return_value=failed):
            self.assertIsNone(watcher.task_running("task"))
        done = SimpleNamespace(returncode=0, stdout="DONE", stderr="")
        with patch.object(watcher.subprocess, "run", return_value=done):
            self.assertFalse(watcher.task_running("task"))

    def test_keychain_validation_rejects_malformed_or_wrong_identity(self):
        bad = SimpleNamespace(returncode=0, stdout="not json")
        with patch.object(watcher.subprocess, "run", return_value=bad):
            with self.assertRaisesRegex(
                    watcher.DriveError, "zigzag-sa.*raw JSON.*hex-decoded JSON") as error:
                watcher.read_service_account_key()
        self.assertNotIn(bad.stdout, str(error.exception))
        wrong = SimpleNamespace(returncode=0, stdout=json.dumps({
            "client_email": "other@example.com", "private_key": "key"}))
        with patch.object(watcher.subprocess, "run", return_value=wrong):
            with self.assertRaisesRegex(watcher.DriveError, "wrong service account"):
                watcher.read_service_account_key()

    def test_keychain_raw_json_value_parses(self):
        expected = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "key"}
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(expected))):
            self.assertEqual(watcher.read_service_account_key(), expected)

    def test_keychain_hex_json_value_parses_to_identical_object(self):
        expected = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "key\nline"}
        encoded = json.dumps(expected, indent=2).encode().hex()
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=encoded)):
            self.assertEqual(watcher.read_service_account_key(), expected)

    def test_keychain_uppercase_hex_json_value_parses(self):
        expected = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "key"}
        encoded = json.dumps(expected).encode().hex().upper()
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=encoded)):
            self.assertEqual(watcher.read_service_account_key(), expected)

    def test_keychain_odd_length_hex_looking_value_errors_without_leaking_value(self):
        value = "abc"
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=value)):
            with self.assertRaisesRegex(
                    watcher.DriveError, "zigzag-sa.*raw JSON.*hex-decoded JSON") as error:
                watcher.read_service_account_key()
        self.assertNotIn(value, str(error.exception))

    def test_keychain_malformed_even_length_hex_errors_without_leaking_value(self):
        value = "6e6f74206a736f6e"
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=value)):
            with self.assertRaisesRegex(
                    watcher.DriveError, "zigzag-sa.*raw JSON.*hex-decoded JSON") as error:
                watcher.read_service_account_key()
        self.assertNotIn(value, str(error.exception))

    def test_keychain_hex_json_scalar_errors_cleanly(self):
        with patch.object(watcher.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout="6e756c6c")):
            with self.assertRaisesRegex(watcher.DriveError, "wrong service account"):
                watcher.read_service_account_key()

    def test_token_mint_uses_drive_scope_and_removes_temporary_key(self):
        key = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "private"}
        signed = SimpleNamespace(returncode=0, stdout=b"signature")
        with patch.object(watcher.subprocess, "run", return_value=signed) as run, \
             patch.object(watcher.urllib.request, "urlopen",
                          return_value=FakeResponse('{"access_token": "token"}')) as urlopen:
            token = watcher.service_account_token(key, now=10)
            key_path = run.call_args.args[0][-1]
        self.assertEqual(token, "token")
        request = urlopen.call_args.args[0]
        sent = request.data.decode()
        self.assertIn("assertion=", sent)
        self.assertNotIn("private", sent)
        assertion = urllib.parse.parse_qs(sent)["assertion"][0]
        claims = assertion.split(".")[1] + "=="
        self.assertEqual(
            json.loads(watcher.base64.urlsafe_b64decode(claims))["scope"],
            watcher.DRIVE_SCOPE)
        self.assertFalse(os.path.exists(key_path))

    def test_token_mint_rejects_missing_token_and_cleans_key(self):
        key = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "private"}
        signed = SimpleNamespace(returncode=0, stdout=b"signature")
        with patch.object(watcher.subprocess, "run", return_value=signed), \
             patch.object(watcher.urllib.request, "urlopen",
                          return_value=FakeResponse("{}")):
            with self.assertRaisesRegex(watcher.DriveError, "no access token"):
                watcher.service_account_token(key, now=10)

    def test_token_mint_handles_signing_and_http_failures(self):
        key = {"client_email": watcher.SERVICE_ACCOUNT, "private_key": "private"}
        failed_sign = SimpleNamespace(returncode=1, stdout=b"")
        with patch.object(watcher.subprocess, "run", return_value=failed_sign) as run:
            with self.assertRaisesRegex(watcher.DriveError, "could not sign"):
                watcher.service_account_token(key, now=10)
        self.assertFalse(os.path.exists(run.call_args.args[0][-1]))
        signed = SimpleNamespace(returncode=0, stdout=b"signature")
        with patch.object(watcher.subprocess, "run", return_value=signed), \
             patch.object(watcher.urllib.request, "urlopen",
                          side_effect=urllib.error.URLError("offline")):
            with self.assertRaisesRegex(watcher.DriveError, "could not mint"):
                watcher.service_account_token(key, now=10)

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


class WatcherStateMachineTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.old = {
            "WATCHER": watcher.WATCHER, "STATE_ROOT": watcher.STATE_ROOT,
            "DATABASE": watcher.DATABASE, "PROMPT_DIR": watcher.PROMPT_DIR,
            "LOCK_FILE": watcher.LOCK_FILE,
        }
        watcher.WATCHER = {
            "owners": {"doc": {"project": "/project", "session": "session"}},
            "trusted_author_permission_ids": ["shukant-id"],
        }
        watcher.STATE_ROOT = str(self.root)
        watcher.DATABASE = str(self.root / "dept.db")
        watcher.PROMPT_DIR = str(self.root / "prompts")
        watcher.LOCK_FILE = str(self.root / "watcher.lock")

    def tearDown(self):
        for name, value in self.old.items():
            setattr(watcher, name, value)
        self.tmp.cleanup()

    def run_main(self, drive, dispatch_result=("task-1", "")):
        with patch.object(watcher, "read_service_account_key", return_value={}), \
             patch.object(watcher, "service_account_token", return_value="token"), \
             patch.object(watcher, "DriveClient", return_value=drive), \
             patch.object(watcher, "dispatch", return_value=dispatch_result) as dispatch:
            rc = watcher.main(["--force"])
        return rc, dispatch

    def test_first_run_seeds_without_acknowledging_or_dispatching(self):
        drive = FakeDrive([human_comment("old")])
        rc, dispatch = self.run_main(drive)
        self.assertEqual(rc, 0)
        self.assertEqual(drive.acks, [])
        dispatch.assert_not_called()
        db = watcher.connect_database()
        self.assertTrue(watcher.comment_seen(db, "doc", "old"))
        db.close()

    def test_new_feedback_is_acked_then_dispatched_and_watermarked(self):
        initial = FakeDrive([])
        self.run_main(initial)
        drive = FakeDrive([human_comment("new")])
        rc, dispatch = self.run_main(drive)
        self.assertEqual(rc, 0)
        self.assertEqual(drive.acks, [("doc", "new")])
        dispatch.assert_called_once()
        prompt = dispatch.call_args.args[1]
        self.assertFalse(os.path.exists(prompt))
        db = watcher.connect_database()
        self.assertTrue(watcher.comment_seen(db, "doc", "new"))
        self.assertEqual(watcher.stored_ack(db, "doc", "new"), "ack-1")
        db.close()

    def test_failed_dispatch_reuses_persisted_ack_on_retry(self):
        self.run_main(FakeDrive([]))
        failed = FakeDrive([human_comment("new")])
        _, failed_dispatch = self.run_main(failed, dispatch_result=(None, "launch failed"))
        self.assertEqual(failed.acks, [("doc", "new")])
        failed_dispatch.assert_called_once()
        retry = FakeDrive([human_comment("new")])
        _, retry_dispatch = self.run_main(retry)
        self.assertEqual(retry.acks, [])
        retry_dispatch.assert_called_once()
        db = watcher.connect_database()
        self.assertTrue(watcher.comment_seen(db, "doc", "new"))
        self.assertEqual(
            db.execute("SELECT task_id FROM drive_comment_session_task").fetchone()[0],
            "task-1")
        db.close()

    def test_persisted_running_session_blocks_later_feedback_and_stale_one_clears(self):
        self.run_main(FakeDrive([]))
        first = FakeDrive([human_comment("first")])
        self.run_main(first)
        db = watcher.connect_database()
        self.assertEqual(
            db.execute("SELECT task_id FROM drive_comment_session_task WHERE session_id = 'session'").fetchone()[0],
            "task-1")
        db.close()
        blocked = FakeDrive([human_comment("first"), human_comment("second")])
        with patch.object(watcher, "task_running", return_value=True):
            _, dispatch = self.run_main(blocked)
        self.assertEqual(blocked.acks, [])
        dispatch.assert_not_called()
        db = watcher.connect_database()
        self.assertFalse(watcher.comment_seen(db, "doc", "second"))
        with patch.object(watcher, "task_running", return_value=None):
            self.assertTrue(watcher.session_busy(db, "session"))
        with patch.object(watcher, "task_running", return_value=False):
            self.assertFalse(watcher.session_busy(db, "session"))
        self.assertIsNone(
            db.execute("SELECT task_id FROM drive_comment_session_task WHERE session_id = 'session'").fetchone())
        db.close()

    def test_non_trusted_and_resolved_feedback_are_consumed(self):
        self.run_main(FakeDrive([]))
        untrusted = human_comment("untrusted")
        untrusted["author"] = {"permissionId": "someone-else"}
        resolved = human_comment("resolved")
        resolved["resolved"] = True
        drive = FakeDrive([untrusted, resolved])
        _, dispatch = self.run_main(drive)
        self.assertEqual(drive.acks, [])
        dispatch.assert_not_called()
        db = watcher.connect_database()
        for comment_id in ("untrusted", "resolved"):
            self.assertTrue(watcher.comment_seen(db, "doc", comment_id))
        db.close()

    def test_trusted_reply_is_acked_on_parent_and_persisted_by_reply_id(self):
        self.run_main(FakeDrive([]))
        thread = human_comment("parent", "original")
        thread["author"] = {"permissionId": "other"}
        thread["replies"] = [{
            "id": "trusted-reply", "content": "follow-up",
            "author": {"permissionId": "shukant-id"},
            "createdTime": "2026-09-28T12:01:00Z",
        }]
        drive = FakeDrive([thread])
        _, dispatch = self.run_main(drive)
        self.assertEqual(drive.acks, [("doc", "parent")])
        dispatch.assert_called_once()
        db = watcher.connect_database()
        self.assertTrue(watcher.comment_seen(db, "doc", "trusted-reply"))
        self.assertEqual(watcher.stored_ack(db, "doc", "trusted-reply"), "ack-1")
        self.assertIsNone(watcher.stored_ack(db, "doc", "parent"))
        db.close()
