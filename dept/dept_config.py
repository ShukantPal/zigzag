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


def required(config, section, key):
    try:
        return config[section][key]
    except KeyError as e:
        raise RuntimeError(
            f"missing {section}.{key} in {config_path()}; copy config.example.json"
        ) from e
