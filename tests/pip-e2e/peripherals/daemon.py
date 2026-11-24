#!/usr/bin/env python3
"""Entrypoint alias for supervisor.py (ARM64 virtual Pip peripheral daemon)."""

from __future__ import annotations

import sys
from pathlib import Path

THIS_DIR = Path(__file__).resolve().parent
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

from supervisor import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
