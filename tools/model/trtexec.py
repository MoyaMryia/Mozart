"""Resolve the trtexec executable without assuming a fixed install path."""

import os
import shutil
from pathlib import Path


def resolve(explicit: Path | None = None) -> str:
    candidate = str(explicit) if explicit else os.environ.get("TRTEXEC")
    if candidate:
        path = shutil.which(candidate)
        if path is None:
            raise SystemExit(f"trtexec not found: {candidate}")
        return path
    path = shutil.which("trtexec")
    if path is None:
        raise SystemExit("trtexec not found; set TRTEXEC or pass --trtexec")
    return path
