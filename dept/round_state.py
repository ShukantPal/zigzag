"""Shared durable storage helpers for review-round state."""
import json
import os
from pathlib import Path
import tempfile


def write_round(path, data):
    """Atomically replace a round file so readers never observe partial JSON."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
                "w", dir=path.parent, prefix=f".{path.name}.", delete=False) as out:
            temporary = Path(out.name)
            json.dump(data, out, indent=2)
            out.flush()
            os.fsync(out.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
