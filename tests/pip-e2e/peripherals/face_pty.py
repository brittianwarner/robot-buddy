"""Virtual ESP32-S3 face serial PTY emulator for Linux ARM64 Pip E2E QA.

Matches apps/robot/src/face.rs:
- Opens a real Linux PTY with cdc_acm kernel defaults (9600, cooked, ECHO, HUPCL,
  -CLOCAL) so robot-buddy-robot's open_serial() termios setup is genuinely verified.
- Keeps an unlocked slave fd open so PTY master reads do not hit EIO between opens
  while robot-buddy-robot's flock(LOCK_EX | LOCK_NB) still succeeds exclusively.
- Parses and validates every JSON line written by the robot: led, e/i, look, g,
  ping, qr.
- Supports soft-reset hello injection WITHOUT closing the PTY fd (optionally
  fragmented across chunks) and explicit QR readback assertions after hello.
- Supports controllable heartbeats, firmware error lines, write-stall fault
  injection, and termios inspection.
"""

from __future__ import annotations

import errno
import json
import os
import pty
import select
import termios
import threading
import time
from pathlib import Path
from typing import Any

VALID_LEDS = frozenset({"idle", "listening", "thinking", "speaking", "muted"})
VALID_EXPRESSIONS = frozenset(
    {
        "neutral",
        "happy",
        "curious",
        "thinking",
        "surprised",
        "sad",
        "angry",
        "sleepy",
        "love",
        "asleep",
    }
)
VALID_GESTURES = frozenset({"blink", "ear_wiggle"})

BAUD_NAMES: dict[int, int] = {}
for _name, _rate in (
    ("B9600", 9600),
    ("B19200", 19200),
    ("B38400", 38400),
    ("B57600", 57600),
    ("B115200", 115200),
    ("B230400", 230400),
    ("B460800", 460800),
    ("B921600", 921600),
    ("B1000000", 1000000),
):
    if hasattr(termios, _name):
        BAUD_NAMES[getattr(termios, _name)] = _rate


def _now_ms() -> float:
    return round(time.time() * 1000.0, 3)


def apply_cdc_kernel_defaults(fd: int) -> None:
    """Configure a PTY slave like a freshly enumerated Linux cdc_acm port."""
    attrs = termios.tcgetattr(fd)
    attrs[0] |= termios.ICRNL
    attrs[1] |= termios.OPOST | termios.ONLCR
    attrs[2] |= termios.HUPCL
    attrs[2] &= ~termios.CLOCAL
    attrs[3] |= termios.ECHO | termios.ICANON | termios.ISIG
    attrs[4] = termios.B9600
    attrs[5] = termios.B9600
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def inspect_face_termios(fd: int) -> dict[str, Any]:
    attrs = termios.tcgetattr(fd)
    iflag, oflag, cflag, lflag, ispeed, ospeed, _cc = attrs
    in_baud = BAUD_NAMES.get(ispeed, int(ispeed))
    out_baud = BAUD_NAMES.get(ospeed, int(ospeed))
    echo = bool(lflag & termios.ECHO)
    icanon = bool(lflag & termios.ICANON)
    opost = bool(oflag & termios.OPOST)
    clocal = bool(cflag & termios.CLOCAL)
    hupcl = bool(cflag & termios.HUPCL)
    cread = bool(cflag & termios.CREAD)
    raw_115200_ok = (
        out_baud == 115200
        and not echo
        and not icanon
        and not opost
        and clocal
        and cread
        and not hupcl
    )
    return {
        "ispeed": in_baud,
        "ospeed": out_baud,
        "echo": echo,
        "icanon": icanon,
        "opost": opost,
        "clocal": clocal,
        "cread": cread,
        "hupcl": hupcl,
        "raw_ok": raw_115200_ok,
        "raw_115200_ok": raw_115200_ok,
    }


def _validate_face_json(raw: str) -> tuple[str, dict[str, Any] | None, str | None]:
    if len(raw.encode("utf-8", errors="replace")) > 560:
        return "invalid", None, "line too long"
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return "invalid", None, "bad json"
    if not isinstance(parsed, dict):
        return "invalid", None, "bad json"

    if "led" in parsed:
        led = parsed["led"]
        if isinstance(led, str) and led in VALID_LEDS:
            return "led", parsed, None
        return "led", parsed, "unknown led mode"

    if "e" in parsed:
        expr = parsed["e"]
        intensity = parsed.get("i")
        if (
            isinstance(expr, str)
            and expr in VALID_EXPRESSIONS
            and isinstance(intensity, (int, float))
            and 0.0 <= float(intensity) <= 1.0
        ):
            return "expression", parsed, None
        return "expression", parsed, "unknown emotion"

    if "look" in parsed:
        look = parsed["look"]
        if (
            isinstance(look, list)
            and len(look) == 2
            and all(isinstance(v, (int, float)) and -1.0 <= float(v) <= 1.0 for v in look)
        ):
            return "look", parsed, None
        return "look", parsed, "bad look"

    if "g" in parsed:
        gesture = parsed["g"]
        if isinstance(gesture, str) and gesture in VALID_GESTURES:
            return "gesture", parsed, None
        return "gesture", parsed, "unknown gesture"

    if "ping" in parsed:
        if isinstance(parsed["ping"], int) and parsed["ping"] >= 0:
            return "ping", parsed, None
        return "ping", parsed, "unknown command"

    if "qr" in parsed:
        qr = parsed["qr"]
        if isinstance(qr, list) and len(qr) == 0:
            return "qr", parsed, None
        if (
            isinstance(qr, list)
            and len(qr) == 2
            and all(isinstance(s, str) and len(s) <= 255 for s in qr)
        ):
            width = parsed.get("w")
            if width is None or (isinstance(width, int) and 60 <= width <= 200):
                return "qr", parsed, None
        return "qr", parsed, "unknown command"

    return "unknown", parsed, "unknown command"


class FacePty:
    """Simulated ESP32-S3 face serial board over a real Linux PTY."""

    def __init__(self, link_path: Path) -> None:
        self.link_path = Path(link_path)
        self.link_path.parent.mkdir(parents=True, exist_ok=True)

        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._stop = threading.Event()

        self._master_fd: int = -1
        self._slave_fd: int = -1
        self._slave_name: str = ""
        self._fd_closed_count: int = 0
        self._open_pty()

        self._seq: int = 0
        self._hello_seq: int = 0
        self._hello_marks: dict[int, dict[str, Any]] = {}
        self._events: list[dict[str, Any]] = []
        self._max_events: int = 5000
        self._errors: list[dict[str, Any]] = []

        self._state: dict[str, Any] = {
            "led": None,
            "expression": None,
            "intensity": None,
            "look": None,
            "qr": None,
            "qr_width": None,
            "gestures_count": 0,
            "pings_count": 0,
            "last_event_ms": None,
        }

        # Fault / traffic controls
        self._stall_writes: bool = False
        self._auto_error_reply: bool = False
        self._hb_enabled: bool = True
        self._hb_interval_s: float = 2.0
        self._hb_fps: int = 30
        self._hb_link: int = 1
        self._hb_counter: int = 0
        self._client_seen: bool = False

        self._reader_thread = threading.Thread(
            target=self._read_loop, name="face-pty-reader", daemon=True
        )
        self._hb_thread = threading.Thread(
            target=self._heartbeat_loop, name="face-pty-hb", daemon=True
        )
        self._reader_thread.start()
        self._hb_thread.start()

    def _open_pty(self) -> None:
        master_fd, slave_fd = pty.openpty()
        os.set_blocking(master_fd, False)
        slave_name = os.ttyname(slave_fd)
        apply_cdc_kernel_defaults(slave_fd)
        os.chmod(slave_name, 0o660)

        tmp_link = self.link_path.with_suffix(".tmp")
        if tmp_link.exists() or tmp_link.is_symlink():
            tmp_link.unlink()
        os.symlink(slave_name, tmp_link)
        os.replace(tmp_link, self.link_path)

        self._master_fd = master_fd
        self._slave_fd = slave_fd
        self._slave_name = slave_name

    def close(self) -> None:
        self._stop.set()
        with self._cond:
            self._cond.notify_all()
        self._reader_thread.join(timeout=1.5)
        self._hb_thread.join(timeout=1.5)
        for fd_attr in ("_master_fd", "_slave_fd"):
            fd = getattr(self, fd_attr, -1)
            if fd >= 0:
                try:
                    os.close(fd)
                except OSError:
                    pass
                setattr(self, fd_attr, -1)
        try:
            if self.link_path.is_symlink() or self.link_path.exists():
                self.link_path.unlink()
        except OSError:
            pass

    def _write_master(self, data: bytes) -> None:
        fd = self._master_fd
        if fd < 0:
            raise RuntimeError("face PTY master closed")
        view = memoryview(data)
        offset = 0
        deadline = time.monotonic() + 2.0
        while offset < len(data):
            try:
                written = os.write(fd, view[offset:])
                offset += written
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("face PTY master write timed out")
                time.sleep(0.002)

    def _read_loop(self) -> None:
        buf = bytearray()
        while not self._stop.is_set():
            with self._lock:
                stalled = self._stall_writes
                fd = self._master_fd
            if stalled or fd < 0:
                time.sleep(0.02)
                continue
            try:
                rlist, _, _ = select.select([fd], [], [], 0.05)
            except (OSError, ValueError):
                time.sleep(0.02)
                continue
            if not rlist:
                continue
            try:
                chunk = os.read(fd, 4096)
            except BlockingIOError:
                continue
            except OSError as exc:
                if exc.errno == errno.EIO:
                    time.sleep(0.02)
                    continue
                if self._stop.is_set():
                    break
                time.sleep(0.02)
                continue
            if not chunk:
                time.sleep(0.02)
                continue
            buf.extend(chunk)
            while True:
                nl = buf.find(b"\n")
                if nl < 0:
                    if len(buf) > 4096:
                        self._record_line(buf.decode("utf-8", errors="replace"))
                        buf.clear()
                    break
                raw_bytes = bytes(buf[:nl])
                del buf[: nl + 1]
                if raw_bytes.endswith(b"\r"):
                    raw_bytes = raw_bytes[:-1]
                self._record_line(raw_bytes.decode("utf-8", errors="replace"))

    def _record_line(self, raw: str) -> None:
        kind, parsed, err = _validate_face_json(raw)
        now = _now_ms()
        reply_err: str | None = None
        with self._cond:
            self._client_seen = True
            self._seq += 1
            entry = {
                "seq": self._seq,
                "ts_ms": now,
                "kind": kind,
                "cmd": parsed,
                "parsed": parsed,
                "raw": raw,
                "after_hello_seq": self._hello_seq,
                "valid": err is None,
                "error": err,
            }
            self._events.append(entry)
            if len(self._events) > self._max_events:
                del self._events[: len(self._events) - self._max_events]

            self._state["last_event_ms"] = now
            if err is not None:
                self._errors.append({"seq": self._seq, "ts_ms": now, "error": err})
                if self._auto_error_reply:
                    reply_err = err
            elif parsed is not None:
                if kind == "led":
                    self._state["led"] = parsed["led"]
                elif kind == "expression":
                    self._state["expression"] = parsed["e"]
                    self._state["intensity"] = float(parsed["i"])
                elif kind == "look":
                    self._state["look"] = [float(parsed["look"][0]), float(parsed["look"][1])]
                elif kind == "gesture":
                    self._state["gestures_count"] += 1
                elif kind == "ping":
                    self._state["pings_count"] += 1
                elif kind == "qr":
                    self._state["qr"] = list(parsed["qr"])
                    self._state["qr_width"] = parsed.get("w")
            self._cond.notify_all()

        if reply_err is not None:
            try:
                self._write_master(f'{{"err":"{reply_err}"}}\n'.encode("utf-8"))
            except Exception:
                pass

    def _heartbeat_loop(self) -> None:
        next_at = time.monotonic() + self._hb_interval_s
        while not self._stop.is_set():
            time.sleep(0.05)
            now = time.monotonic()
            with self._lock:
                enabled = self._hb_enabled and self._client_seen and not self._stall_writes
                interval = self._hb_interval_s
                fps = self._hb_fps
                link = self._hb_link
            if not enabled:
                next_at = now + interval
                continue
            if now >= next_at:
                with self._lock:
                    self._hb_counter += 1
                    hb_id = self._hb_counter
                line = json.dumps({"hb": hb_id, "fps": fps, "link": link}, separators=(",", ":"))
                try:
                    self._write_master((line + "\n").encode("utf-8"))
                except Exception:
                    pass
                next_at = now + interval

    # ---- Public control API ----

    def status(self) -> dict[str, Any]:
        with self._lock:
            tinfo = inspect_face_termios(self._slave_fd) if self._slave_fd >= 0 else None
            return {
                "connected": self._client_seen,
                "pty_path": str(self.link_path),
                "slave_path": self._slave_name,
                "termios": tinfo,
                "state": dict(self._state),
                "hello_seq": self._hello_seq,
                "fd_closed_count": self._fd_closed_count,
                "total_lines": self._seq,
                "buffered_lines": len(self._events),
                "errors_count": len(self._errors),
                "errors": list(self._errors[-20:]),
                "heartbeat": {
                    "enabled": self._hb_enabled,
                    "interval_ms": int(self._hb_interval_s * 1000),
                    "fps": self._hb_fps,
                    "link": self._hb_link,
                    "sent": self._hb_counter,
                },
                "faults": {
                    "stall_writes": self._stall_writes,
                    "auto_error_reply": self._auto_error_reply,
                },
            }

    def history(self, since_seq: int = 0, limit: int = 500) -> dict[str, Any]:
        with self._lock:
            items = [e for e in self._events if e["seq"] > since_seq]
            if limit > 0:
                items = items[-limit:]
            return {
                "events": items,
                "lines": items,
                "state": dict(self._state),
                "hello_seq": self._hello_seq,
                "fd_closed_count": self._fd_closed_count,
                "termios": inspect_face_termios(self._slave_fd) if self._slave_fd >= 0 else None,
                "latest_seq": self._seq,
            }

    def clear_history(self) -> dict[str, Any]:
        with self._lock:
            self._events.clear()
            self._errors.clear()
            return {"cleared": True, "seq": self._seq}

    def inject_hello(
        self,
        version: int = 1,
        eyes: int = 1,
        fragmented: bool = True,
        noise_prefix: str = "",
    ) -> dict[str, Any]:
        """Send a board soft-reset hello line WITHOUT closing the PTY fd."""
        with self._lock:
            self._hello_seq += 1
            hello_seq = self._hello_seq
            seq_before = self._seq
            fd_closed_before = self._fd_closed_count
            self._hello_marks[hello_seq] = {
                "seq_before": seq_before,
                "fd_closed_before": fd_closed_before,
                "ts_ms": _now_ms(),
            }

        payload = json.dumps(
            {"hello": "pip-face", "v": int(version), "eyes": int(eyes)},
            separators=(",", ":"),
        ) + "\n"
        raw = (noise_prefix + payload).encode("utf-8")

        if fragmented and len(raw) > 12:
            split_at = len(noise_prefix.encode("utf-8")) + 14
            self._write_master(raw[:split_at])
            time.sleep(0.015)
            self._write_master(raw[split_at:])
        else:
            self._write_master(raw)

        return {
            "hello_seq": hello_seq,
            "seq_before": seq_before,
            "fd_closed": False,
            "fd_closed_count": fd_closed_before,
            "fragmented": bool(fragmented),
        }

    def assert_qr_after_hello(
        self,
        hello_seq: int | None = None,
        expected_qr: list[str] | None = None,
        expected_left: str | None = None,
        expected_right: str | None = None,
        require_non_empty: bool = False,
        expected_width: int | None = None,
        timeout_ms: int = 3000,
        forbid_gesture_replay: bool = True,
    ) -> dict[str, Any]:
        """Wait for and verify QR readback emitted after a soft-reset hello without fd close."""
        with self._cond:
            target_hello = hello_seq if hello_seq is not None else self._hello_seq
            mark = self._hello_marks.get(target_hello)
            if mark is None:
                raise ValueError(f"unknown hello_seq={target_hello}")
            seq_before = mark["seq_before"]
            fd_closed_before = mark["fd_closed_before"]

            deadline = time.monotonic() + (timeout_ms / 1000.0)
            matched_qr_event: dict[str, Any] | None = None
            while True:
                after = [
                    e
                    for e in self._events
                    if e["seq"] > seq_before and e["after_hello_seq"] >= target_hello
                ]
                for ev in after:
                    if ev["kind"] == "qr" and ev["valid"]:
                        matched_qr_event = ev
                        break
                if matched_qr_event is not None:
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"no QR frame received after hello_seq={target_hello} within {timeout_ms}ms "
                        f"(saw {len(after)} events: {[e['kind'] for e in after]})"
                    )
                self._cond.wait(timeout=min(remaining, 0.1))

            fd_closed = self._fd_closed_count != fd_closed_before
            if fd_closed:
                raise AssertionError(
                    f"PTY fd was closed during soft-reset hello_seq={target_hello}"
                )

            repainted = [
                e
                for e in self._events
                if seq_before < e["seq"] <= matched_qr_event["seq"]
                and e["after_hello_seq"] >= target_hello
            ]
            replayed_gestures = [e for e in repainted if e["kind"] == "gesture"]
            if forbid_gesture_replay and replayed_gestures:
                raise AssertionError(
                    f"gesture was unexpectedly replayed after hello_seq={target_hello}: {replayed_gestures}"
                )

            qr_val = list(matched_qr_event["parsed"]["qr"])
            qr_width = matched_qr_event["parsed"].get("w")

            if expected_qr is not None and qr_val != list(expected_qr):
                raise AssertionError(
                    f"QR readback mismatch after hello_seq={target_hello}: got {qr_val!r}, expected {expected_qr!r}"
                )
            if expected_left is not None:
                if len(qr_val) != 2 or qr_val[0] != expected_left:
                    raise AssertionError(
                        f"QR left mismatch after hello_seq={target_hello}: got {qr_val!r}, expected left={expected_left!r}"
                    )
            if expected_right is not None:
                if len(qr_val) != 2 or qr_val[1] != expected_right:
                    raise AssertionError(
                        f"QR right mismatch after hello_seq={target_hello}: got {qr_val!r}, expected right={expected_right!r}"
                    )
            if require_non_empty:
                if len(qr_val) != 2 or not qr_val[0] or not qr_val[1]:
                    raise AssertionError(
                        f"expected non-empty QR pair after hello_seq={target_hello}, got {qr_val!r}"
                    )
            if expected_width is not None and qr_width != expected_width:
                raise AssertionError(
                    f"QR width mismatch after hello_seq={target_hello}: got {qr_width!r}, expected {expected_width!r}"
                )

            return {
                "ok": True,
                "hello_seq": target_hello,
                "fd_closed": False,
                "qr": qr_val,
                "qr_width": qr_width,
                "qr_event_seq": matched_qr_event["seq"],
                "repainted_kinds": [e["kind"] for e in repainted],
                "gesture_replayed": bool(replayed_gestures),
            }

    def wait_for(self, predicate: dict[str, Any], timeout_ms: int = 5000) -> dict[str, Any]:
        after_seq = int(predicate.get("after_seq", 0))
        after_hello_seq = predicate.get("after_hello_seq")

        def matches(ev: dict[str, Any]) -> bool:
            if ev["seq"] <= after_seq:
                return False
            if after_hello_seq is not None and ev["after_hello_seq"] < int(after_hello_seq):
                return False
            parsed = ev.get("parsed")
            if not isinstance(parsed, dict) or not ev.get("valid"):
                return False
            if "kind" in predicate and ev["kind"] != predicate["kind"]:
                return False
            if "led" in predicate and parsed.get("led") != predicate["led"]:
                return False
            if "e" in predicate and parsed.get("e") != predicate["e"]:
                return False
            if "g" in predicate and parsed.get("g") != predicate["g"]:
                return False
            if predicate.get("has_look") and ev["kind"] != "look":
                return False
            if "look_nonzero" in predicate and predicate["look_nonzero"]:
                if ev["kind"] != "look":
                    return False
                lx, ly = parsed["look"]
                if abs(float(lx)) < 0.01 and abs(float(ly)) < 0.01:
                    return False
            if "qr" in predicate and parsed.get("qr") != predicate["qr"]:
                return False
            if predicate.get("qr_non_empty"):
                qr = parsed.get("qr")
                if not (isinstance(qr, list) and len(qr) == 2 and qr[0] and qr[1]):
                    return False
            if predicate.get("qr_cleared") and parsed.get("qr") != []:
                return False
            if predicate.get("has_ping") and ev["kind"] != "ping":
                return False
            return True

        deadline = time.monotonic() + (timeout_ms / 1000.0)
        with self._cond:
            while True:
                for ev in self._events:
                    if matches(ev):
                        return {"matched": True, "event": ev}
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"face.wait_for timed out after {timeout_ms}ms for predicate={predicate}"
                    )
                self._cond.wait(timeout=min(remaining, 0.1))

    def set_heartbeat(
        self,
        enabled: bool | None = None,
        interval_ms: int | None = None,
        fps: int | None = None,
        link: int | None = None,
    ) -> dict[str, Any]:
        with self._lock:
            if enabled is not None:
                self._hb_enabled = bool(enabled)
            if interval_ms is not None:
                self._hb_interval_s = max(0.05, float(interval_ms) / 1000.0)
            if fps is not None:
                self._hb_fps = int(fps)
            if link is not None:
                self._hb_link = int(link)
            return {
                "enabled": self._hb_enabled,
                "interval_ms": int(self._hb_interval_s * 1000),
                "fps": self._hb_fps,
                "link": self._hb_link,
            }

    def inject_error(self, err: str) -> dict[str, Any]:
        line = json.dumps({"err": str(err)}, separators=(",", ":")) + "\n"
        self._write_master(line.encode("utf-8"))
        return {"sent": line.strip()}

    def set_fault(
        self,
        stall_writes: bool | None = None,
        auto_error_reply: bool | None = None,
        reset_termios: bool = False,
        reopen_pty: bool = False,
    ) -> dict[str, Any]:
        with self._lock:
            if stall_writes is not None:
                self._stall_writes = bool(stall_writes)
            if auto_error_reply is not None:
                self._auto_error_reply = bool(auto_error_reply)
            if reset_termios and self._slave_fd >= 0:
                apply_cdc_kernel_defaults(self._slave_fd)
            if reopen_pty:
                old_master, old_slave = self._master_fd, self._slave_fd
                self._fd_closed_count += 1
                self._open_pty()
                for fd in (old_master, old_slave):
                    if fd >= 0:
                        try:
                            os.close(fd)
                        except OSError:
                            pass
            return {
                "stall_writes": self._stall_writes,
                "auto_error_reply": self._auto_error_reply,
                "fd_closed_count": self._fd_closed_count,
                "termios": inspect_face_termios(self._slave_fd) if self._slave_fd >= 0 else None,
            }
