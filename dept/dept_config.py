"""Small, dependency-free configuration helpers for department scripts.

Deployment-specific values belong in ``config.json`` (or the path named by
``CODEX_DEPT_CONFIG``), which is intentionally ignored by git.  The checked-in
``config.example.json`` documents the supported shape.
"""
import json
import os


ROOT = os.path.dirname(os.path.abspath(__file__))


def config_path():
    return os.path.expanduser(os.environ.get(
        "CODEX_DEPT_CONFIG", os.path.join(ROOT, "config.json")))


def load_config():
    """Return deployment configuration, or an empty mapping when unconfigured."""
    try:
        with open(config_path()) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}
    except json.JSONDecodeError as e:
        raise RuntimeError(f"invalid department config {config_path()}: {e}") from e


def state_dir(config):
    """State root; defaults to a gitignored directory beside this toolset."""
    value = config.get("state_dir") or os.environ.get("CODEX_DEPT_STATE_DIR")
    return os.path.expanduser(value) if value else os.path.join(ROOT, "runtime")


def ssh_base(connection):
    """Build the shared non-interactive SSH transport from deployment config."""
    key = os.path.expanduser(connection.get("ssh_key", ""))
    proxy = os.path.expanduser(connection.get("proxy_helper", ""))
    known_hosts = os.path.expanduser(connection.get("known_hosts", ""))
    return [
        "ssh", "-i", key,
        "-o", "BatchMode=yes", "-o", "PasswordAuthentication=no",
        "-o", "StrictHostKeyChecking=accept-new",
        "-o", f"UserKnownHostsFile={known_hosts}",
        "-o", f"ProxyCommand=python3 {proxy} %h %p",
        connection.get("mac", ""),
    ]


def ssh_env():
    """Return the proxy environment expected by the Tailscale helper."""
    env = dict(os.environ)
    proxy = env.get("HTTPS_PROXY", "")
    env["TUNNEL_PROXY"] = proxy.rsplit(":", 1)[0] + ":3130" if ":" in proxy else ""
    return env
