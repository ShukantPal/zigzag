#!/usr/bin/env bash
# Exercise the real relay binary through zzapi and the dept status TUI model.
#
# The fake `codex` below intentionally stays alive.  That lets this test
# prove pause/resume signal a real process instead of merely accepting a 200.
set -euo pipefail

relay_bin=${1:?usage: $0 RELAY_BIN ZZAPI_BIN}
zzapi_bin=${2:?usage: $0 RELAY_BIN ZZAPI_BIN}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d)
relay_pid=""

cleanup() {
  status=$?
  if [[ "$status" -ne 0 && -f "$tmp_dir/relay.log" ]]; then
    echo "relay log after failed full-stack test:" >&2
    tail -n 100 "$tmp_dir/relay.log" >&2 || true
  fi
  if [[ -n "$relay_pid" ]] && kill -0 "$relay_pid" 2>/dev/null; then
    kill "$relay_pid" 2>/dev/null || true
    wait "$relay_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp_dir"
  return "$status"
}
trap cleanup EXIT

home_dir="$tmp_dir/home"
bin_dir="$tmp_dir/bin"
project_dir="$tmp_dir/project"
worktree_root="$tmp_dir/worktrees"
state_file="$tmp_dir/events.json"
token_file="$tmp_dir/relay.token"
mkdir -p "$home_dir" "$bin_dir" "$project_dir" "$worktree_root"
printf '%s' 'e2e-full-stack-secret-0123456789abcdef' > "$token_file"
chmod 600 "$token_file"

# The daemon binds this value in addition to loopback.  Ask the OS for its
# outbound source address without sending a packet; unlike 127.0.0.2, this
# also works on macOS, whose loopback aliases are not generally bindable.
tailnet_ip=$(python3 - <<'PY'
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.connect(("8.8.8.8", 80))
print(sock.getsockname()[0])
sock.close()
PY
)
printf '%s\n' '#!/bin/sh' "echo $tailnet_ip" > "$bin_dir/tailscale"
chmod 700 "$bin_dir/tailscale"

printf '%s\n' \
  '#!/bin/sh' \
  "printf '%s\\n' '{\"type\":\"item.completed\",\"text\":\"e2e-agent-output\"}'" \
  '# Keep the process alive so pause, resume, and delete have real work to do.' \
  'while :; do sleep 1; done' > "$bin_dir/codex"
chmod 700 "$bin_dir/codex"

git -C "$project_dir" init -b main -q
git -C "$project_dir" config user.email zigzag-e2e@example.invalid
git -C "$project_dir" config user.name 'Zigzag E2E'
git -C "$project_dir" commit --allow-empty -qm init

port=$(python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
)

HOME="$home_dir" PATH="$bin_dir:$PATH" ZIGZAG_UPDATE_POLICY=paused \
  ZIGZAG_WORKTREE_ROOTS="$worktree_root" \
  ZIGZAG_WORKTREE_REPO_ROOT="$tmp_dir" \
  "$relay_bin" --secret-file "$token_file" --state-file "$state_file" --port "$port" \
  >"$tmp_dir/relay.log" 2>&1 &
relay_pid=$!

api=("$zzapi_bin" --hostname "127.0.0.1:$port" --token-file "$token_file" --json)
for _ in $(seq 1 50); do
  if "${api[@]}" health >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
"${api[@]}" health >/dev/null

created=$("${api[@]}" agents create \
  --prompt 'full-stack e2e task' \
  --project-dir "$project_dir" \
  --branch codex/e2e-full-stack \
  --worktree "$worktree_root/agent")
agent_id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' <<<"$created")

# Creation must result in a registered running agent, not an accepted denial.
listed=$("${api[@]}" agents list)
python3 -c '
import json, sys
agent_id = sys.argv[1]
agents = json.load(sys.stdin)["agents"]
agent = next((agent for agent in agents if agent["id"] == agent_id), None)
assert agent is not None, "created agent is absent from zzapi agents list"
assert agent["state"] == "running", agent
' "$agent_id" <<<"$listed"

status=$("${api[@]}" agents get "$agent_id")
task_id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["task_id"])' <<<"$status")
leader_pid=$(python3 - "$agent_id" "$tmp_dir/events.agents.json" <<'PY'
import json, sys
agent_id, registry_path = sys.argv[1:]
agents = json.load(open(registry_path))["agents"]
agent = next(agent for agent in agents if agent["id"] == agent_id)
print(agent["leader_pid"])
PY
)
transcript="$home_dir/.zigzag/agents/codex/$agent_id.jsonl"
for _ in $(seq 1 50); do
  [[ -s "$transcript" ]] && grep -q 'e2e-agent-output' "$transcript" && break
  sleep 0.1
done
[[ -s "$transcript" ]]
grep -q 'e2e-agent-output' "$transcript"

# The TUI's non-interactive view must associate the relay events and agent
# table, yielding the actual task and a concrete agent state.
tui=$(python3 "$repo_root/dept/status.py" --url "http://127.0.0.1:$port" \
  --state-file "$state_file" --token-file "$token_file" --once)
python3 -c '
import sys
task_id = sys.argv[1]
rows = [line.split("\t") for line in sys.stdin.read().splitlines()[1:]]
row = next((row for row in rows if row and row[0] == task_id), None)
assert row is not None, f"TUI did not display task {task_id!r}"
assert row[4] != "not observed", f"TUI did not observe agent state: {row}"
' "$task_id" <<<"$tui"

# Exercise the TUI model's real TCP receive path after bootstrap.  Its HTTP
# accessor is deliberately disabled before polling: the injected event must
# arrive through the authenticated socket subscription, not a fallback poll.
PYTHONPATH="$repo_root" python3 - "$port" "$token_file" <<'PY'
import json
import sys
import urllib.request

from dept.status import EventStream

port, token_path = sys.argv[1:]
token = open(token_path).read().strip()
url = f"http://127.0.0.1:{port}"
stream = EventStream(url, token)
stream.bootstrap()
stream._get = lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("unexpected HTTP fallback"))
request = urllib.request.Request(
    url + "/v1/events",
    data=json.dumps({"id": "socket-full-stack", "kind": "socket_test"}).encode(),
    headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    method="POST",
)
with urllib.request.urlopen(request, timeout=5) as response:
    assert response.status == 201
for _ in range(20):
    stream.poll(1)
    if any(item.get("id") == "socket-full-stack" for item in stream.events):
        break
else:
    raise AssertionError("TUI model did not receive the pushed socket event")
PY

"${api[@]}" agents pause "$agent_id" >/dev/null
for _ in $(seq 1 50); do
  state=$(ps -o stat= -p "$leader_pid" 2>/dev/null | tr -d ' ' || true)
  [[ "$state" == *T* ]] && break
  sleep 0.1
done
[[ "${state:-}" == *T* ]]
paused=$("${api[@]}" agents get "$agent_id")
python3 -c 'import json,sys; assert json.load(sys.stdin)["paused_at"]' <<<"$paused"

"${api[@]}" agents resume "$agent_id" >/dev/null
for _ in $(seq 1 50); do
  state=$(ps -o stat= -p "$leader_pid" 2>/dev/null | tr -d ' ' || true)
  [[ -n "$state" && "$state" != *T* ]] && break
  sleep 0.1
done
[[ -n "${state:-}" && "$state" != *T* ]]
resumed=$("${api[@]}" agents get "$agent_id")
python3 -c 'import json,sys; assert json.load(sys.stdin)["paused_at"] is None' <<<"$resumed"

"${api[@]}" agents stop "$agent_id" >/dev/null
deleted=$("${api[@]}" agents get "$agent_id")
python3 -c 'import json,sys; assert json.load(sys.stdin)["state"] in ("stopped", "killed")' <<<"$deleted"
if kill -0 "$leader_pid" 2>/dev/null; then
  echo "agent leader is still alive after delete" >&2
  exit 1
fi

echo "full-stack relay + zzapi + TUI lifecycle passed"
