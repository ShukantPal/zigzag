#!/usr/bin/env python3
"""Codex department manager: dispatch and track background Codex agents on Shukant's Mac.

Usage:
  dept.py start <project-dir> <prompt-file>
  dept.py resume [project-dir] <session-id> <prompt-file>
      (project-dir optional: resolved from the session file's recorded cwd)
  dept.py status <task-id>
  dept.py list
  dept.py result <task-id>
  dept.py tokens <task-id>

All long-running work happens detached on the Mac (nohup), so it survives this VM.
"""
import json, os, subprocess, sys, time, uuid, datetime

try:  # `python3 dept/dept.py` and `python3 -m dept.dept` are both supported.
    from .dept_config import ROOT, load_config, state_dir
    from .status import main as status_main
except ImportError:  # pragma: no cover - direct script execution path
    from dept_config import ROOT, load_config, state_dir
    from status import main as status_main

CONFIG = load_config()
CONNECTION = CONFIG.get("connection", {})
STATE_DIR = state_dir(CONFIG)
MAC = os.environ.get("CODEX_DEPT_MAC", CONNECTION.get("mac", ""))
SSH_KEY = os.path.expanduser(CONNECTION.get("ssh_key", ""))
PROXY_HELPER = os.path.expanduser(CONNECTION.get("proxy_helper", ""))
# Absolute on the Mac: the relay spawn API has no shell, so ~ never expands.
REMOTE_DEPT = CONNECTION.get("remote_dept", "")
LEDGER = os.path.join(STATE_DIR, "ledger.jsonl")
ZIGZAG_URL = os.environ.get("ZIGZAG_URL", CONNECTION.get("zigzag_url", ""))
ZIGZAG_TOKEN_FILE = os.path.expanduser(CONNECTION.get("zigzag_token_file", ""))
# Launcher the relay is allowlisted to spawn: runs in the GUI login session
# (keychain reachable) and execs codex with the department's standard flags.
RELAY_LAUNCHER = CONNECTION.get("relay_launcher", "codex-launch")


def asset_path(name):
    return os.path.join(ROOT, name)


def tunnel_proxy():
    hp = os.environ["HTTPS_PROXY"]
    return hp.rsplit(":", 1)[0] + ":3130"


def ssh(*remote_cmd, stdin_data=None, timeout=60):
    if not (MAC and SSH_KEY and PROXY_HELPER and REMOTE_DEPT):
        sys.exit("department connection is not configured; copy dept/config.example.json to dept/config.json")
    env = dict(os.environ)
    env["TUNNEL_PROXY"] = tunnel_proxy()
    cmd = ["ssh", "-i", SSH_KEY, "-o", "BatchMode=yes",
           "-o", "PasswordAuthentication=no", "-o", "ConnectTimeout=30",
           # accept-new: verify known hosts strictly, but self-heal if the
           # known_hosts entry ever goes missing (2026-09-13: the file got
           # wiped mid-run and every SSH call started failing).
           "-o", "StrictHostKeyChecking=accept-new",
           "-o", f"ProxyCommand=python3 {PROXY_HELPER} %h %p", MAC,
           *remote_cmd]
    return subprocess.run(cmd, input=stdin_data, capture_output=True, text=False, timeout=timeout, env=env)


# --- Zigzag relay client (spawn/poll/kill in the GUI login session) ---

def _zigzag_opener():
    import urllib.request
    proxy = tunnel_proxy()
    return urllib.request.build_opener(
        urllib.request.ProxyHandler({"http": proxy, "https": proxy}))


def _zigzag_call(method, path, body=None, timeout=30):
    import urllib.request, urllib.error
    try:
        with open(ZIGZAG_TOKEN_FILE) as f:
            token = f.read().strip()
    except OSError as e:
        sys.exit(f"could not read zigzag token: {e}")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        ZIGZAG_URL + path, data=data, method=method,
        headers={"Authorization": f"Bearer {token}",
                 "Content-Type": "application/json"})
    try:
        with _zigzag_opener().open(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {"error": "http_error"}
    except Exception as e:
        sys.exit(f"zigzag request failed: {e}")


def zigzag_spawn(bin_name, args, ident):
    status, payload = _zigzag_call("POST", "/v1/spawn",
                                   {"id": ident, "bin": bin_name, "args": args})
    if status != 200 or "proc" not in payload:
        sys.exit(f"zigzag spawn failed (http {status}): {payload}")
    return payload["proc"]


def zigzag_poll(handle):
    status, payload = _zigzag_call("GET", f"/v1/proc/{handle}")
    if status == 404:
        return None  # pruned from the relay's table: treat as done
    if status != 200:
        sys.exit(f"zigzag poll failed (http {status}): {payload}")
    return payload


def zigzag_kill(handle):
    status, payload = _zigzag_call("POST", f"/v1/proc/{handle}/kill")
    if status == 404:
        return False
    if status != 200:
        sys.exit(f"zigzag kill failed (http {status}): {payload}")
    return payload.get("killed", False)


def ledger_append(entry):
    os.makedirs(os.path.dirname(LEDGER), exist_ok=True)
    with open(LEDGER, "a") as f:
        f.write(json.dumps(entry) + "\n")


def ledger_read():
    if not os.path.exists(LEDGER):
        return []
    out = []
    for line in open(LEDGER):
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def dispatch_args(args, resume=False):
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("project_dir", nargs="?" if resume else None, default=None)
    ap.add_argument("prompt_file")
    if resume:
        # argparse assigns positional values left-to-right; declaring the
        # optional project before the required session/prompt supports both
        # `resume SESSION PROMPT` and `resume PROJECT SESSION PROMPT`.
        ap = argparse.ArgumentParser()
        ap.add_argument("project_dir", nargs="?", default=None)
        ap.add_argument("session_id")
        ap.add_argument("prompt_file")
    ap.add_argument("--no-sop", action="store_true",
                    help="skip prepending the standard PR/review/CI operating procedure")
    ap.add_argument("--writing", action="store_true",
                    help="prepend the writing standard (strategic/hierarchical/simple) instead of the code SOP")
    ap.add_argument("--ssh", action="store_true",
                    help="launch over SSH+nohup instead of the relay (no keychain access)")
    return ap.parse_args(args)


def decorated_prompt(ns):
    prompt = open(ns.prompt_file, "rb").read()
    if not prompt.strip():
        sys.exit("empty prompt")
    if ns.writing:
        writing_path = asset_path("writing.md")
        if os.path.exists(writing_path):
            writing = open(writing_path, "rb").read()
            prompt = writing + b"\n\n---\n\nTASK:\n" + prompt
    elif not ns.no_sop:
        sop_path = asset_path("sop.md")
        if os.path.exists(sop_path):
            sop = open(sop_path, "rb").read()
            prompt = sop + b"\n\n---\n\nTASK:\n" + prompt
    return prompt


def setup_task_dir(tid, project_dir, prompt, session_id=None):
    """Store a fully-decorated task payload before either launch transport."""
    rdir = f"{REMOTE_DEPT}/{tid}"
    r = ssh(f"mkdir -p {rdir} && cat > {rdir}/prompt.txt",
            stdin_data=prompt, timeout=60)
    if r.returncode != 0:
        sys.exit(f"ssh setup failed: {r.stderr.decode()[-500:]}")
    files = f"printf %s {shq(project_dir)} > {rdir}/dir.txt"
    if session_id:
        files += f" && printf %s {shq(session_id)} > {rdir}/resume.txt"
    r = ssh(files, timeout=60)
    if r.returncode != 0:
        sys.exit(f"ssh dir write failed: {r.stderr.decode()[-500:]}")
    return rdir


def dispatch_task(project_dir, prompt, use_ssh, session_id=None):
    """Launch start/resume through one preparation, transport, and ledger path."""
    tid = "t-" + uuid.uuid4().hex[:6]
    relay_path = asset_path("relay-announce.md")
    if os.path.exists(relay_path):
        relay = open(relay_path, "rb").read().replace(b"{{TASK_ID}}", tid.encode())
        prompt = prompt + b"\n\n---\n\n" + relay
    rdir = setup_task_dir(tid, project_dir, prompt, session_id)
    action = "resumed" if session_id else "started"
    if use_ssh:
        command = (f'resume "$(cat {rdir}/resume.txt)" "$(cat {rdir}/prompt.txt)" '
                   f'-o {rdir}/last-message.txt'
                   if session_id else
                   f'-C "$d" -o {rdir}/last-message.txt "$(cat {rdir}/prompt.txt)"')
        launch = (f'd=$(cat {rdir}/dir.txt); [ -d "$d" ] || exit 3; cd "$d" && '
                  f'nohup codex exec --json --approve-for-me --skip-git-repo-check '
                  f'{command} '
                  f'< /dev/null > {rdir}/events.jsonl 2> {rdir}/stderr.log & '
                  f'echo $! > {rdir}/pid && cat {rdir}/pid')
        r = ssh(launch, timeout=60)
        if r.returncode != 0:
            sys.exit(f"launch failed (rc={r.returncode}): {r.stderr.decode()[-500:]}")
        pid = r.stdout.decode().strip()
        transport = {"via": "ssh", "pid": pid}
        detail = f"pid={pid}"
    else:
        proc = zigzag_spawn(RELAY_LAUNCHER,
                            ["resume" if session_id else "run", rdir], f"codex-{tid}")
        transport = {"via": "relay", "proc": proc}
        detail = f"proc={proc[:12]}..."
    prefix = f"[resume {session_id[:8]}] " if session_id else ""
    ledger_append({"id": tid, "project": project_dir, **transport, "status": "running",
                   "prompt_head": prefix + prompt.decode(errors="replace")[:200],
                   "started_at": datetime.datetime.now().isoformat(timespec="seconds")})
    session = f" session={session_id[:8]}" if session_id else ""
    print(f"{action} {tid} {detail}{session} project={project_dir} (via {transport['via']})")


def cmd_start(args):
    ns = dispatch_args(args)
    dispatch_task(ns.project_dir, decorated_prompt(ns), ns.ssh)


def shq(s):
    return "'" + s.replace("'", "'\\''") + "'"


def session_meta_cwd(lines):
    """Return the first valid ``session_meta.payload.cwd`` in JSONL lines."""
    for line in lines:
        try:
            record = json.loads(line)
        except (TypeError, json.JSONDecodeError):
            continue
        if record.get("type") == "session_meta":
            cwd = (record.get("payload") or {}).get("cwd")
            if isinstance(cwd, str) and cwd:
                return cwd
    return None


def single_cwd(candidates):
    """Return the sole non-empty string candidate, refusing ambiguity."""
    cwds = {cwd for cwd in candidates if isinstance(cwd, str) and cwd}
    return next(iter(cwds)) if len(cwds) == 1 else None


def resolve_session_cwd(session_id):
    """Return the cwd recorded in the Codex session file on the Mac, or None.

    Every Codex session file records the cwd it started in (session_meta).
    This is the source of truth for where a session lives, so resume doesn't
    need the caller to re-supply (and possibly mistype) the project dir.
    """
    if not session_id or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for c in session_id):
        return None
    script = (
        "import glob, json\n"
        f"sid = {session_id!r}\n"
        "cwds = set()\n"
        "pattern = '/Users/shukant/.codex/sessions/**/rollout-*-' + glob.escape(sid) + '.jsonl'\n"
        "for f in sorted(glob.glob(pattern, recursive=True)):\n"
        "    try:\n"
        "        lines = open(f, errors='replace')\n"
        "        for line in lines:\n"
        "            try:\n"
        "                d = json.loads(line)\n"
        "            except Exception:\n"
        "                continue\n"
        "            if d.get('type') == 'session_meta':\n"
        "                cwd = (d.get('payload') or {}).get('cwd')\n"
        "                if isinstance(cwd, str) and cwd:\n"
        "                    cwds.add(cwd)\n"
        "                break\n"
        "    except OSError:\n"
        "        continue\n"
        "print(json.dumps(sorted(cwds)))\n"
    ).encode()
    r = ssh("python3", "-", stdin_data=script, timeout=60)
    if r.returncode != 0:
        return None
    try:
        return single_cwd(json.loads(r.stdout.decode()))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None


def remote_isdir(path):
    r = ssh("test", "-d", path, timeout=30)
    return r.returncode == 0


def remote_status(entry):
    """RUNNING / DONE / MISSING for a ledger entry, via SSH pid or relay proc."""
    tid = entry["id"]
    if entry.get("via") == "relay" and entry.get("proc"):
        payload = zigzag_poll(entry["proc"])
        if payload is None:
            return "DONE"  # pruned from the relay table: finished long ago
        return "RUNNING" if payload.get("running") else "DONE"
    r = ssh(f"rdir={REMOTE_DEPT}/{tid}; "
            f"if [ ! -f $rdir/pid ]; then echo MISSING; exit 0; fi; "
            f"if kill -0 $(cat $rdir/pid) 2>/dev/null; then echo RUNNING; else echo DONE; fi",
            timeout=60)
    return r.stdout.decode().strip()


def cmd_kill(args):
    tid = args[0]
    matches = [e for e in ledger_read() if e["id"] == tid]
    if not matches:
        sys.exit(f"unknown task {tid}")
    entry = matches[-1]
    if entry.get("via") == "relay" and entry.get("proc"):
        killed = zigzag_kill(entry["proc"])
        print(f"{tid}: kill requested via relay (killed={killed})")
    else:
        r = ssh(f"kill $(cat {REMOTE_DEPT}/{tid}/pid) 2>/dev/null && echo killed || echo 'not running'",
                timeout=60)
        print(f"{tid}: {r.stdout.decode().strip()} (via ssh)")


def cmd_status(args):
    tid = args[0]
    entry = next((e for e in ledger_read() if e["id"] == tid), {"id": tid})
    print(f"{tid}: {remote_status(entry)}")


def cmd_list(args):
    for e in ledger_read():
        live = remote_status(e) if e.get("status") == "running" else e.get("status")
        via = e.get("via", "ssh")
        print(f"{e['id']} [{live}] ({via}) {e['project']} :: {e['prompt_head'][:80]}")


def cmd_result(args):
    tid = args[0]
    entry = next((e for e in ledger_read() if e["id"] == tid), {"id": tid})
    rdir = f"{REMOTE_DEPT}/{tid}"
    print(f"--- status: {remote_status(entry)} ---")
    r = ssh(f"cat {rdir}/last-message.txt 2>/dev/null || echo '(no final message yet)'", timeout=60)
    print(r.stdout.decode(errors="replace"))
    print("--- token usage ---")
    print(token_summary(tid))


def token_summary(tid):
    rdir = f"{REMOTE_DEPT}/{tid}"
    r = ssh(f"grep -o '\"total_tokens\":[0-9]*' {rdir}/events.jsonl 2>/dev/null | tail -1; "
            f"grep -o '\"input_tokens\":[0-9]*' {rdir}/events.jsonl 2>/dev/null | tail -1; "
            f"wc -l < {rdir}/events.jsonl 2>/dev/null", timeout=60)
    return r.stdout.decode().strip() or "(no events yet)"


def cmd_tokens(args):
    print(token_summary(args[0]))


def reported_path():
    return os.path.join(STATE_DIR, "reported.json")


def load_reported():
    p = reported_path()
    if os.path.exists(p):
        try:
            return json.load(open(p))
        except Exception:
            return {}
    return {}


def save_reported(r):
    json.dump(r, open(reported_path(), "w"), indent=1)


def cmd_resume(args):
    ns = dispatch_args(args, resume=True)
    project_dir = ns.project_dir
    if not project_dir:
        project_dir = resolve_session_cwd(ns.session_id)
        if not project_dir:
            sys.exit(f"could not find a Codex session file for {ns.session_id} on the Mac; "
                     f"pass project_dir explicitly")
        print(f"resolved project_dir={project_dir} from session {ns.session_id[:8]}",
              file=sys.stderr)
    if not remote_isdir(project_dir):
        sys.exit(f"project_dir does not exist on the Mac: {project_dir} "
                 f"(refusing to dispatch a dead task)")
    dispatch_task(project_dir, decorated_prompt(ns), ns.ssh, ns.session_id)


def cmd_check(args):
    """Print newly-completed, not-yet-reported tasks and mark them reported.
    Meant for a polling cron: prints NOTHING_NEW when there is nothing."""
    from datetime import datetime, timezone
    reported = load_reported()
    now = datetime.now(timezone.utc).isoformat(timespec="seconds")
    found = False
    for e in ledger_read():
        tid = e["id"]
        if tid in reported:
            continue
        if remote_status(e) == "RUNNING":
            continue
        found = True
        r = ssh(f"tail -c 1500 {REMOTE_DEPT}/{tid}/last-message.txt 2>/dev/null "
                f"|| echo '(no final message yet)'", timeout=60)
        summary = r.stdout.decode(errors="replace").strip()
        print(f"COMPLETED {tid} project={e['project']} started={e.get('started_at','?')}")
        print(f"task: {e.get('prompt_head','')[:200]}")
        print(f"summary: {summary}")
        print(f"tokens: {token_summary(tid)}")
        print("---")
        reported[tid] = now
    save_reported(reported)
    if not found:
        print("NOTHING_NEW")


def management_main(argv):
    """Run department manager commands, including ``status TASK_ID``."""
    if not argv:
        sys.exit("usage: dept.py <start|status|list|result|tokens|check|resume|kill> ...")
    cmd, rest = argv[0], argv[1:]
    commands = {"start": cmd_start, "status": cmd_status, "list": cmd_list,
     "result": cmd_result, "tokens": cmd_tokens, "check": cmd_check,
     "resume": cmd_resume, "kill": cmd_kill}
    handler = commands.get(cmd)
    if handler is None:
        sys.exit(f"unknown command: {cmd}")
    return handler(rest)


def main(argv=None):
    """Dispatch the read-only status view alongside the department manager."""
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == "status" and (len(argv) == 1 or argv[1].startswith("-")):
        return status_main(argv[1:])
    return management_main(argv)


if __name__ == "__main__":
    raise SystemExit(main())
