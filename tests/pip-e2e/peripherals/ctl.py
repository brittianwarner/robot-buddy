#!/usr/bin/env python3
"""CLI helper for talking to the ARM64 virtual Pip peripheral supervisor."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

THIS_DIR = Path(__file__).resolve().parent
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

from client import PeripheralsClient  # noqa: E402


def main() -> int:
    p = argparse.ArgumentParser(description="Control CLI for virtual Pip peripherals")
    p.add_argument("--sock", required=True, help="Path to <run_dir>/control.sock")
    p.add_argument("method", help="RPC method name (e.g. status, face.inject_hello)")
    p.add_argument("params", nargs="?", default="{}", help="JSON object params")
    args = p.parse_args()

    params = json.loads(args.params)
    with PeripheralsClient(args.sock) as client:
        res = client.call(args.method, params)
    print(json.dumps(res, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
