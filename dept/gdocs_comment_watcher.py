#!/usr/bin/env python3
"""Mac-side Drive watcher for Google Doc feedback.

Uses the zigzag service-account JSON held in the GUI-login keychain, discovers
documents from the shared Drive folder, acknowledges new Shukant comments, and
resumes their configured owner sessions. Durable watermarks live in SQLite at
the Mac department state root.
"""
import argparse
import base64
import datetime
import fcntl
import json
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from zoneinfo import ZoneInfo

from dept_config import ROOT, load_config

SERVICE_ACCOUNT = "zigzag@shukant.iam.gserviceaccount.com"
KEYCHAIN_SERVICE = "zigzag-sa"
FOLDER_ID = "1W_iTcpdYGVXj_NTmkfcgOm_GGREk1Nj3"
# Used only when deployment configuration has not named an individual share.
EXTRA_DOCUMENT_ID = "1PF8O_BoLwKetxmcuPmQRYXq4weQGXwd6vABSd62atV4"
DRIVE_SCOPE = "https://www.googleapis.com/auth/drive"
GOOGLE_DOC_MIME = "application/vnd.google-apps.document"
MARKER = "Muse (AI assistant)"
EYES = "\U0001F440"
ACK_TEXT = f"{EYES} {MARKER} — picked up, addressing it now."

CONFIG = load_config()
WATCHER = CONFIG.get("gdocs_comment_watcher", {})
STATE_ROOT = os.path.expanduser(WATCHER.get("state_root", "~/.zigzag/dept"))
DATABASE = os.path.join(STATE_ROOT, "dept.db")
PROMPT_DIR = os.path.join(STATE_ROOT, "prompts")
LOCK_FILE = os.path.join(STATE_ROOT, "drive-comment-watcher.lock")
DEPT = os.path.join(ROOT, "dept.py")


class DriveError(RuntimeError):
    pass


def b64url(value):
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode()


def read_service_account_key():
    """Read, but never print or persist, the keychain JSON."""
    result = subprocess.run(
        ["security", "find-generic-password", "-s", KEYCHAIN_SERVICE,
         "-a", SERVICE_ACCOUNT, "-w"],
        capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise DriveError("could not read the zigzag service-account key from the login keychain")
    try:
        key = json.loads(result.stdout)
    except json.JSONDecodeError:
        # JSON necessarily includes punctuation such as {}, ", or :, so a strict
        # full-string hex match cannot misidentify genuine raw JSON.
        if re.fullmatch(r"[0-9a-fA-F]+", result.stdout) and len(result.stdout) % 2 == 0:
            try:
                key = json.loads(bytes.fromhex(result.stdout))
            except (json.JSONDecodeError, UnicodeDecodeError) as error:
                raise DriveError(
                    "zigzag-sa keychain item could not be parsed as raw JSON or hex-decoded JSON"
                ) from error
        else:
            raise DriveError(
                "zigzag-sa keychain item could not be parsed as raw JSON or hex-decoded JSON"
            )
    if (not isinstance(key, dict) or key.get("client_email") != SERVICE_ACCOUNT
            or not key.get("private_key")):
        raise DriveError("zigzag service-account keychain item has the wrong service account")
    return key


def secure_directory(path):
    os.makedirs(path, mode=0o700, exist_ok=True)
    os.chmod(path, 0o700)


def secure_file(path):
    try:
        os.chmod(path, 0o600)
    except FileNotFoundError:
        pass


def service_account_token(key, now=None):
    """Mint a Drive-scoped service-account token with macOS OpenSSL."""
    now = int(time.time() if now is None else now)
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}, separators=(",", ":")).encode())
    claims = b64url(json.dumps({
        "iss": key["client_email"], "scope": DRIVE_SCOPE,
        "aud": "https://oauth2.googleapis.com/token", "iat": now, "exp": now + 3600,
    }, separators=(",", ":")).encode())
    signing_input = f"{header}.{claims}".encode()
    fd, key_path = tempfile.mkstemp(prefix="zigzag-drive-key-", text=True)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(key["private_key"])
        signed = subprocess.run(
            ["openssl", "dgst", "-sha256", "-sign", key_path],
            input=signing_input, capture_output=True, timeout=30)
        if signed.returncode:
            raise DriveError("could not sign the service-account OAuth assertion")
    finally:
        try:
            os.unlink(key_path)
        except FileNotFoundError:
            pass
    body = urllib.parse.urlencode({
        "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
        "assertion": f"{header}.{claims}.{b64url(signed.stdout)}",
    }).encode()
    try:
        request = urllib.request.Request(
            "https://oauth2.googleapis.com/token", data=body,
            headers={"Content-Type": "application/x-www-form-urlencoded"}, method="POST")
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.load(response)
    except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError) as error:
        raise DriveError("could not mint a Google Drive service-account token") from error
    if not payload.get("access_token"):
        raise DriveError("Google OAuth token response had no access token")
    return payload["access_token"]


class DriveClient:
    def __init__(self, token):
        self.token = token
        self.skipped_documents = []

    def request(self, method, path, *, params=None, body=None):
        url = "https://www.googleapis.com/drive/v3/" + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(url, data=data, method=method, headers={
            "Authorization": f"Bearer {self.token}", "Accept": "application/json",
            **({"Content-Type": "application/json"} if body is not None else {}),
        })
        try:
            with urllib.request.urlopen(request, timeout=45) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise DriveError(f"Drive {method} {path} failed (HTTP {error.code})") from error
        except (urllib.error.URLError, json.JSONDecodeError) as error:
            raise DriveError(f"Drive {method} {path} failed") from error

    def list_documents(self, folder_id, additional_ids):
        docs, page_token = {}, None
        while True:
            page = self.request("GET", "files", params={
                "q": f"'{folder_id}' in parents and trashed = false",
                "fields": "nextPageToken,files(id,name,mimeType)", "pageSize": 100,
                "pageToken": page_token or "", "supportsAllDrives": "true",
                "includeItemsFromAllDrives": "true",
            })
            for item in page.get("files", []):
                if item.get("mimeType") == GOOGLE_DOC_MIME:
                    docs[item["id"]] = item
            page_token = page.get("nextPageToken")
            if not page_token:
                break
        for doc_id in additional_ids:
            try:
                item = self.request("GET", f"files/{urllib.parse.quote(doc_id, safe='')}", params={
                    "fields": "id,name,mimeType", "supportsAllDrives": "true"})
            except DriveError as error:
                self.skipped_documents.append((doc_id, str(error)))
                continue
            if item.get("mimeType") == GOOGLE_DOC_MIME:
                docs[item["id"]] = item
        return list(docs.values())

    def list_comments(self, doc_id):
        comments, page_token = [], None
        while True:
            page = self.request("GET", f"files/{urllib.parse.quote(doc_id, safe='')}/comments", params={
                "fields": "nextPageToken,comments(id,content,quotedFileContent(value),author(displayName,emailAddress,permissionId),createdTime,resolved,replies(id,content,author(displayName,emailAddress,permissionId),createdTime))",
                "pageSize": 100, "pageToken": page_token or "", "includeDeleted": "false"})
            comments.extend(page.get("comments", []))
            page_token = page.get("nextPageToken")
            if not page_token:
                return comments

    def post_eyes_reply(self, doc_id, comment_id):
        reply = self.request("POST", f"files/{urllib.parse.quote(doc_id, safe='')}/comments/"
                             f"{urllib.parse.quote(comment_id, safe='')}/replies",
                             params={"fields": "id"},
                             body={"content": ACK_TEXT})
        if not reply.get("id"):
            raise DriveError("Drive created an acknowledgment reply without an id")
        return reply["id"]


def connect_database(path=None):
    path = path or DATABASE
    secure_directory(os.path.dirname(path))
    db = sqlite3.connect(path)
    db.execute("PRAGMA journal_mode=WAL")
    secure_file(path)
    secure_file(path + "-wal")
    secure_file(path + "-shm")
    db.executescript("""
        CREATE TABLE IF NOT EXISTS drive_comment_watermark (
            document_id TEXT NOT NULL, comment_id TEXT NOT NULL, seen_at TEXT NOT NULL,
            PRIMARY KEY (document_id, comment_id));
        CREATE TABLE IF NOT EXISTS drive_comment_ack (
            document_id TEXT NOT NULL, comment_id TEXT NOT NULL, ack_reply_id TEXT NOT NULL,
            created_at TEXT NOT NULL, PRIMARY KEY (document_id, comment_id));
        CREATE TABLE IF NOT EXISTS drive_comment_session_task (
            session_id TEXT PRIMARY KEY, task_id TEXT NOT NULL, updated_at TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS drive_comment_watcher_meta (
            key TEXT PRIMARY KEY, value TEXT NOT NULL);
    """)
    return db


def database_initialized(db):
    return db.execute("SELECT 1 FROM drive_comment_watcher_meta WHERE key = 'initialized'").fetchone() is not None


def mark_initialized(db):
    db.execute("INSERT OR REPLACE INTO drive_comment_watcher_meta(key, value) VALUES ('initialized', ?)",
               (datetime.datetime.now(datetime.timezone.utc).isoformat(),))


def comment_seen(db, doc_id, comment_id):
    return db.execute("SELECT 1 FROM drive_comment_watermark WHERE document_id = ? AND comment_id = ?",
                      (doc_id, comment_id)).fetchone() is not None


def mark_seen(db, doc_id, comment_id):
    db.execute("INSERT OR IGNORE INTO drive_comment_watermark VALUES (?, ?, ?)",
               (doc_id, comment_id, datetime.datetime.now(datetime.timezone.utc).isoformat()))


def feedback_items(comments):
    """Flatten top-level and reply feedback, retaining the parent thread."""
    for comment in comments:
        yield {**comment, "_parent_id": comment["id"]}
        for reply in comment.get("replies", []) or []:
            yield {**reply, "quotedFileContent": comment.get("quotedFileContent"),
                   "_parent_id": comment["id"], "_thread": comment}


def configured_values(name):
    values = WATCHER.get(name, [])
    return {str(value).casefold() for value in values if value}


def trusted_author(item):
    """Require a stable configured Drive identity, never a display name."""
    author = item.get("author") or {}
    return ((author.get("permissionId") or "").casefold() in configured_values("trusted_author_permission_ids")
            or (author.get("emailAddress") or "").casefold() in configured_values("trusted_author_emails"))


def stored_ack(db, doc_id, comment_id):
    row = db.execute(
        "SELECT ack_reply_id FROM drive_comment_ack WHERE document_id = ? AND comment_id = ?",
        (doc_id, comment_id)).fetchone()
    return row[0] if row else None


def save_ack(db, doc_id, comment_id, ack_reply_id):
    db.execute("INSERT OR IGNORE INTO drive_comment_ack VALUES (?, ?, ?, ?)",
               (doc_id, comment_id, ack_reply_id,
                datetime.datetime.now(datetime.timezone.utc).isoformat()))
    db.commit()


def owner_for(doc_id):
    """Owners are session metadata, never the watched-document registry."""
    owners = WATCHER.get("owners") or WATCHER.get("docs", {})
    owner = owners.get(doc_id)
    return owner if isinstance(owner, dict) and owner.get("project") and owner.get("session") else None


def task_running(task_id):
    """Return True/False for a known task state, None when status is unknown."""
    try:
        result = subprocess.run([sys.executable, DEPT, "status", task_id],
                                capture_output=True, text=True, timeout=90)
    except Exception:
        return None
    text = result.stdout + result.stderr
    if result.returncode != 0:
        return None
    if "RUNNING" in text:
        return True
    return False if "DONE" in text else None


def session_busy(db, session_id):
    row = db.execute("SELECT task_id FROM drive_comment_session_task WHERE session_id = ?",
                     (session_id,)).fetchone()
    if not row:
        return False
    if task_running(row[0]) is not False:
        return True
    db.execute("DELETE FROM drive_comment_session_task WHERE session_id = ?", (session_id,))
    db.commit()
    return False


PROMPT_TEMPLATE = """# Google Doc feedback — address Shukant's new comments

Shukant left {count} new comment(s) on {title} ({doc_id}). The Mac-side Drive
watcher has already posted marked 👀 acknowledgments. ADDRESS every comment:
update the document source/docx in the established workflow, make matching
scoped code changes where needed, and report exactly what changed.

## Comments
{comments}

Do not post another acknowledgment. Keep substantive Drive replies marked with
{marker} so the watcher never mistakes them for human feedback.
"""


def write_prompt(doc, acked):
    lines = []
    for item, reply_id in acked:
        anchor = ((item.get("quotedFileContent") or {}).get("value") or "")[:200].replace("\n", " ")
        lines.append(f"- comment {item['id']} (ack {reply_id}, {item.get('createdTime', 'unknown time')})\n"
                     f"  Anchor: {anchor}\n  Text: {item.get('content', '')}")
    secure_directory(PROMPT_DIR)
    with tempfile.NamedTemporaryFile(
            mode="w", prefix=f"drive-feedback-{doc['id'][:8]}-",
            suffix=".md", dir=PROMPT_DIR, delete=False) as handle:
        handle.write(PROMPT_TEMPLATE.format(count=len(acked), title=doc.get("name", doc["id"]),
                                             doc_id=doc["id"], comments="\n".join(lines), marker=MARKER))
    secure_file(handle.name)
    return handle.name


def dispatch(owner, prompt):
    result = subprocess.run([sys.executable, DEPT, "resume", owner["project"], owner["session"], prompt],
                            capture_output=True, text=True, timeout=180)
    if result.returncode:
        return None, (result.stdout + result.stderr).strip()
    task_id = next((line.split()[1] for line in result.stdout.splitlines()
                    if line.startswith("resumed ")), None)
    return (task_id, "") if task_id else (None, (result.stdout + result.stderr).strip())


def quiet_hours(now=None):
    hour = (now or datetime.datetime.now(ZoneInfo("America/Los_Angeles"))).hour
    return hour >= 22 or hour < 7


def watched_documents(client):
    folder_id = WATCHER.get("folder_id", FOLDER_ID)
    additional = WATCHER.get("additional_file_ids")
    if additional is None:
        additional = [EXTRA_DOCUMENT_ID]
    additional = list(dict.fromkeys(additional))
    return client.list_documents(folder_id, additional)


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", action="store_true", help="Record current comments without dispatching.")
    ap.add_argument("--force", action="store_true", help="Poll during the 22:00–07:00 PT quiet hours.")
    args = ap.parse_args(argv)
    if quiet_hours() and not args.force:
        return 0

    secure_directory(STATE_ROOT)
    lock = open(LOCK_FILE, "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        print("another Drive comment watcher holds the lock; exiting")
        return 0
    db = connect_database()
    try:
        client = DriveClient(service_account_token(read_service_account_key()))
        docs = watched_documents(client)
    except DriveError as error:
        print(f"Drive watcher setup failed: {error}", file=sys.stderr)
        db.close()
        lock.close()
        return 1
    for doc_id, error in client.skipped_documents:
        print(f"{doc_id}: additional document skipped: {error}", file=sys.stderr)

    seed_only = args.seed or not database_initialized(db)
    dispatched = 0
    for doc in docs:
        try:
            comments = client.list_comments(doc["id"])
        except DriveError as error:
            print(f"{doc.get('name', doc['id'])}: comment list failed: {error}", file=sys.stderr)
            continue
        fresh = []
        for item in feedback_items(comments):
            if comment_seen(db, doc["id"], item["id"]):
                continue
            if seed_only or item.get("_thread", item).get("resolved") or not trusted_author(item):
                mark_seen(db, doc["id"], item["id"])
            else:
                fresh.append(item)
        db.commit()
        if seed_only or not fresh:
            continue
        owner = owner_for(doc["id"])
        if not owner:
            print(f"{doc.get('name', doc['id'])}: {len(fresh)} new comment(s), no owning session configured", file=sys.stderr)
            continue
        if session_busy(db, owner["session"]):
            print(f"{doc.get('name', doc['id'])}: owning session is busy; will retry {len(fresh)} comment(s)")
            continue
        acked = []
        for item in fresh:
            try:
                reply_id = stored_ack(db, doc["id"], item["id"])
                if not reply_id:
                    reply_id = client.post_eyes_reply(doc["id"], item["_parent_id"])
                    save_ack(db, doc["id"], item["id"], reply_id)
                acked.append((item, reply_id))
            except DriveError as error:
                print(f"{doc['id']}:{item['id']}: acknowledgment failed: {error}", file=sys.stderr)
        if not acked:
            continue
        prompt = write_prompt(doc, acked)
        try:
            task_id, error = dispatch(owner, prompt)
        finally:
            try:
                os.unlink(prompt)
            except FileNotFoundError:
                pass
        if not task_id:
            print(f"{doc.get('name', doc['id'])}: dispatch failed; will retry: {error[:300]}", file=sys.stderr)
            continue
        now = datetime.datetime.now(datetime.timezone.utc).isoformat()
        for item, reply_id in acked:
            mark_seen(db, doc["id"], item["id"])
        db.execute("INSERT OR REPLACE INTO drive_comment_session_task VALUES (?, ?, ?)",
                   (owner["session"], task_id, now))
        db.commit()
        dispatched += 1
        print(f"{doc.get('name', doc['id'])}: dispatched {task_id} for {len(acked)} comment(s)")
    if seed_only:
        mark_initialized(db)
        db.commit()
        print(f"seeded Drive comment watermark for {len(docs)} document(s)")
    elif not dispatched:
        print("NOTHING_NEW")
    db.close()
    lock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
