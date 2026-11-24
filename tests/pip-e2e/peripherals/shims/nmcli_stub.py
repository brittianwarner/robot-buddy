#!/usr/bin/env python3
"""Optional controllable `nmcli` stub for non-simulate ARM64 container E2E runs.

Matches apps/robot/src/wifi.rs invocations:
- `nmcli -t -f STATE general` -> prints current uplink state (default `connected`)
- `nmcli connection add ...` / `up` / `delete` / `modify` / `show` -> stateful
  profile tracking in `<run_dir>/nmcli/` so Wi-Fi setup QR flows can also be
  exercised cleanly inside an isolated container without NetworkManager D-Bus.
"""

from __future__ import annotations

import json
import os
import sys
import uuid
from pathlib import Path


def _split_wrapper_args(raw_argv: list[str]) -> tuple[Path, list[str]]:
    if len(raw_argv) >= 3 and raw_argv[0] == "--state-dir":
        state_dir = Path(raw_argv[1])
        rest = raw_argv[2:]
        if rest and rest[0] == "--":
            rest = rest[1:]
        return state_dir, rest
    state_dir = Path(os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "nmcli")))
    return state_dir, raw_argv


def main() -> int:
    state_dir, args = _split_wrapper_args(sys.argv[1:])
    state_dir.mkdir(parents=True, exist_ok=True)
    profiles_dir = state_dir / "profiles"
    profiles_dir.mkdir(parents=True, exist_ok=True)

    # Record argv without logging any wifi-sec.psk password value.
    redacted: list[str] = []
    hide_next = False
    for a in args:
        if hide_next:
            redacted.append("<redacted>")
            hide_next = False
        else:
            redacted.append(a)
            if a == "wifi-sec.psk":
                hide_next = True
    with open(state_dir / "log.jsonl", "a", encoding="utf-8") as fp:
        fp.write(json.dumps({"argv": redacted}) + "\n")

    if "general" in args:
        state_file = state_dir / "state"
        val = state_file.read_text(encoding="utf-8").strip() if state_file.is_file() else "connected"
        sys.stdout.write(val + "\n")
        return 0

    def _find_by_name(con_name: str) -> list[Path]:
        found = []
        for p in sorted(profiles_dir.glob("*.json")):
            try:
                d = json.loads(p.read_text(encoding="utf-8"))
                if d.get("name") == con_name:
                    found.append(p)
            except Exception:
                pass
        return found

    joined = " " + " ".join(args) + " "
    if " connection add " in joined:
        con_name = "unnamed"
        autoconnect = "yes"
        for i, a in enumerate(args):
            if a == "con-name" and i + 1 < len(args):
                con_name = args[i + 1]
            elif a == "autoconnect" and i + 1 < len(args):
                autoconnect = args[i + 1]
        u = str(uuid.uuid4())
        (profiles_dir / f"{u}.json").write_text(
            json.dumps({"uuid": u, "name": con_name, "autoconnect": autoconnect}),
            encoding="utf-8",
        )
        sys.stdout.write(f"Connection '{con_name}' ({u}) successfully added.\n")
        return 0

    if " connection delete " in joined:
        target_uuid = None
        target_id = None
        for i, a in enumerate(args):
            if a == "uuid" and i + 1 < len(args):
                target_uuid = args[i + 1]
            elif a == "id" and i + 1 < len(args):
                target_id = args[i + 1]
        if target_uuid:
            p = profiles_dir / f"{target_uuid}.json"
            if not p.is_file():
                sys.stderr.write(f"Error: unknown connection '{target_uuid}'.\n")
                return 10
            p.unlink()
            return 0
        if target_id:
            matches = _find_by_name(target_id)
            if not matches:
                sys.stderr.write(f"Error: unknown connection '{target_id}'.\n")
                return 10
            matches[0].unlink()
            return 0
        return 10

    if " connection up " in joined:
        return 0

    if " connection modify " in joined:
        target_uuid = None
        auto = None
        for i, a in enumerate(args):
            if a == "uuid" and i + 1 < len(args):
                target_uuid = args[i + 1]
            elif a == "connection.autoconnect" and i + 1 < len(args):
                auto = args[i + 1]
        if target_uuid:
            p = profiles_dir / f"{target_uuid}.json"
            if not p.is_file():
                return 10
            d = json.loads(p.read_text(encoding="utf-8"))
            if auto is not None:
                d["autoconnect"] = auto
            p.write_text(json.dumps(d), encoding="utf-8")
            return 0
        return 10

    if " connection show " in joined:
        for i, a in enumerate(args):
            if a == "id" and i + 1 < len(args):
                matches = _find_by_name(args[i + 1])
                if not matches:
                    return 10
                for p in matches:
                    sys.stdout.write(p.stem + "\n")
                return 0
        return 0

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
