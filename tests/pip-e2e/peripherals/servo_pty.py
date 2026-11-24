"""Virtual Feetech STS3215 3-axis servo bus PTY emulator for Linux ARM64 Pip E2E QA.

Matches apps/robot/src/motion/{sts3215,drive,calibration,mod,poses}.rs:
- Real Linux PTY symlinked at <run_dir>/dev/servo.
- Verifies Linux 1 Mbaud (B1000000) raw CLOCAL CREAD -HUPCL -CRTSCTS termios.
- Implements exact little-endian Feetech STS wire protocol (PING 0x01, READ 0x02,
  WRITE 0x03, SYNC WRITE 0x83 on broadcast ID 254).
- Supports realistic half-duplex bus echo (default enabled) so sts3215.rs echo
  filtering is exercised end-to-end.
- Enforces EEPROM lock (reg 55 == 0 required to write regs < 40 such as LIMITS
  reg 9..13 or MODE reg 33) and calibrated/EEPROM position limits on GOAL writes.
- Simulates smooth present-position tracking (reg 56..58) when TORQUE (reg 40) == 1.
- Records every register read/write, 3-axis sync-write goal frame, cadence stats,
  limit violations, EEPROM lock violations, and checksum errors.
"""

from __future__ import annotations

import errno
import json
import os
import pty
import select
import struct
import termios
import threading
import time
from pathlib import Path
from typing import Any

REG_LIMITS = 9
REG_MODE = 33
REG_TORQUE = 40
REG_ACCEL = 41
REG_GOAL = 42
REG_TIME = 44
REG_SPEED = 46
REG_LOCK = 55
REG_PRESENT = 56

INST_PING = 0x01
INST_READ = 0x02
INST_WRITE = 0x03
INST_SYNC_WRITE = 0x83
BROADCAST_ID = 254

DEG_PER_STEP = 360.0 / 4096.0
B1000000_VAL = getattr(termios, "B1000000", 0o010010)
CRTSCTS_VAL = getattr(termios, "CRTSCTS", 0o20000000000)

DEFAULT_CALIBRATION: dict[str, dict[str, Any]] = {
    "yaw": {
        "id": 1,
        "center": 2048,
        "min_steps": 1888,
        "max_steps": 2208,
        "invert": False,
        "max_speed_deg_s": 45.0,
        "accel": 20,
    },
    "pitch": {
        "id": 2,
        "center": 2048,
        "min_steps": 1888,
        "max_steps": 2208,
        "invert": True,
        "max_speed_deg_s": 45.0,
        "accel": 20,
    },
    "roll": {
        "id": 3,
        "center": 2048,
        "min_steps": 1888,
        "max_steps": 2208,
        "invert": False,
        "max_speed_deg_s": 45.0,
        "accel": 20,
    },
}


def _now_ms() -> float:
    return round(time.time() * 1000.0, 3)


def sts_checksum(body: bytes | bytearray) -> int:
    return (~sum(body)) & 0xFF


def sts_packet(servo_id: int, inst_or_err: int, params: bytes = b"") -> bytes:
    length = len(params) + 2
    body = bytes([servo_id, length, inst_or_err]) + params
    return b"\xff\xff" + body + bytes([sts_checksum(body)])


def inspect_servo_termios(fd: int) -> dict[str, Any]:
    attrs = termios.tcgetattr(fd)
    _iflag, oflag, cflag, lflag, ispeed, ospeed, _cc = attrs
    baud_is_1m = ispeed == B1000000_VAL and ospeed == B1000000_VAL
    echo = bool(lflag & termios.ECHO)
    icanon = bool(lflag & termios.ICANON)
    opost = bool(oflag & termios.OPOST)
    clocal = bool(cflag & termios.CLOCAL)
    cread = bool(cflag & termios.CREAD)
    hupcl = bool(cflag & termios.HUPCL)
    crtscts = bool(cflag & CRTSCTS_VAL)
    raw_1mbaud_ok = (
        baud_is_1m
        and not echo
        and not icanon
        and not opost
        and clocal
        and cread
        and not hupcl
        and not crtscts
    )
    return {
        "ispeed_raw": int(ispeed),
        "ospeed_raw": int(ospeed),
        "baud_1000000": baud_is_1m,
        "echo": echo,
        "icanon": icanon,
        "opost": opost,
        "clocal": clocal,
        "cread": cread,
        "hupcl": hupcl,
        "crtscts": crtscts,
        "raw_1mbaud_ok": raw_1mbaud_ok,
    }


def _u16_le(mem: bytearray, reg: int) -> int:
    return int(struct.unpack_from("<H", mem, reg)[0])


def _set_u16_le(mem: bytearray, reg: int, val: int) -> None:
    struct.pack_into("<H", mem, reg, val & 0xFFFF)


class ServoPty:
    """Simulated 3-axis Feetech STS3215 serial bus over a Linux PTY."""

    def __init__(
        self,
        link_path: Path,
        calibration_path: Path,
        calibration: dict[str, dict[str, Any]] | None = None,
        unprovisioned: bool = False,
    ) -> None:
        self.link_path = Path(link_path)
        self.calibration_path = Path(calibration_path)
        self.link_path.parent.mkdir(parents=True, exist_ok=True)
        self.calibration_path.parent.mkdir(parents=True, exist_ok=True)

        if calibration is not None:
            self.calibration = json.loads(json.dumps(calibration))
            self.calibration_path.write_text(
                json.dumps(self.calibration, indent=2) + "\n", encoding="utf-8"
            )
        elif self.calibration_path.is_file():
            self.calibration = json.loads(self.calibration_path.read_text(encoding="utf-8"))
        else:
            self.calibration = json.loads(json.dumps(DEFAULT_CALIBRATION))
            self.calibration_path.write_text(
                json.dumps(self.calibration, indent=2) + "\n", encoding="utf-8"
            )

        self._axes_order = ("yaw", "pitch", "roll")
        self._joints_by_id: dict[int, dict[str, Any]] = {}
        self._axis_index_by_id: dict[int, int] = {}
        self._mem: dict[int, bytearray] = {}
        self._present_float: dict[int, float] = {}

        for idx, axis in enumerate(self._axes_order):
            j = self.calibration[axis]
            sid = int(j["id"])
            self._joints_by_id[sid] = j
            self._axis_index_by_id[sid] = idx
            mem = bytearray(64)
            if unprovisioned:
                _set_u16_le(mem, REG_LIMITS, 0)
                _set_u16_le(mem, REG_LIMITS + 2, 4095)
            else:
                _set_u16_le(mem, REG_LIMITS, int(j["min_steps"]))
                _set_u16_le(mem, REG_LIMITS + 2, int(j["max_steps"]))
            mem[REG_MODE] = 0
            mem[REG_TORQUE] = 0
            mem[REG_ACCEL] = int(j["accel"])
            _set_u16_le(mem, REG_GOAL, int(j["center"]))
            mem[REG_LOCK] = 1
            _set_u16_le(mem, REG_PRESENT, int(j["center"]))
            self._mem[sid] = mem
            self._present_float[sid] = float(int(j["center"]))

        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._stop = threading.Event()

        master_fd, slave_fd = pty.openpty()
        os.set_blocking(master_fd, False)
        slave_name = os.ttyname(slave_fd)
        os.chmod(slave_name, 0o660)
        # Initialize with cooked defaults so Serial::open() must apply raw 1 Mbaud.
        attrs = termios.tcgetattr(slave_fd)
        attrs[3] |= termios.ECHO | termios.ICANON
        attrs[4] = termios.B9600
        attrs[5] = termios.B9600
        termios.tcsetattr(slave_fd, termios.TCSANOW, attrs)

        tmp_link = self.link_path.with_suffix(".tmp")
        if tmp_link.exists() or tmp_link.is_symlink():
            tmp_link.unlink()
        os.symlink(slave_name, tmp_link)
        os.replace(tmp_link, self.link_path)

        self._master_fd = master_fd
        self._slave_fd = slave_fd
        self._slave_name = slave_name

        # Telemetry
        self._seq: int = 0
        self._events: list[dict[str, Any]] = []
        self._goals: list[dict[str, Any]] = []
        self._max_events: int = 8000
        self._max_goals: int = 4000
        self._goal_frames_count: int = 0
        self._limit_violations: list[dict[str, Any]] = []
        self._eeprom_lock_violations: list[dict[str, Any]] = []
        self._checksum_errors: int = 0
        self._goal_timestamps_mono: list[float] = []

        # Faults / bus options
        self._echo_enabled: bool = True
        self._drop_replies_for: set[int] = set()
        self._corrupt_checksum: bool = False
        self._status_error_byte: int = 0
        self._response_delay_s: float = 0.0
        self._forget_write: tuple[int, int] | None = None

        self._reader_thread = threading.Thread(
            target=self._read_loop, name="servo-pty-reader", daemon=True
        )
        self._physics_thread = threading.Thread(
            target=self._physics_loop, name="servo-pty-physics", daemon=True
        )
        self._reader_thread.start()
        self._physics_thread.start()

    def close(self) -> None:
        self._stop.set()
        with self._cond:
            self._cond.notify_all()
        self._reader_thread.join(timeout=1.5)
        self._physics_thread.join(timeout=1.5)
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
            return
        offset = 0
        deadline = time.monotonic() + 0.5
        while offset < len(data):
            try:
                written = os.write(fd, data[offset:])
                offset += written
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    return
                time.sleep(0.0005)
            except OSError:
                return

    def _physics_loop(self) -> None:
        dt = 0.01
        while not self._stop.is_set():
            time.sleep(dt)
            with self._lock:
                for sid, mem in self._mem.items():
                    if mem[REG_TORQUE] != 1:
                        continue
                    goal = float(_u16_le(mem, REG_GOAL))
                    cur = self._present_float[sid]
                    diff = goal - cur
                    if abs(diff) < 0.25:
                        cur = goal
                    else:
                        speed_steps = _u16_le(mem, REG_SPEED)
                        # Real STS3215 speed==0 means maximum speed; keep centering fast and smooth.
                        max_rate = float(speed_steps) if speed_steps > 0 else 2000.0
                        max_rate = max(max_rate, 600.0)
                        step = max_rate * dt
                        if diff > 0:
                            cur = min(goal, cur + step)
                        else:
                            cur = max(goal, cur - step)
                    min_lim = _u16_le(mem, REG_LIMITS)
                    max_lim = _u16_le(mem, REG_LIMITS + 2)
                    if min_lim < max_lim:
                        cur = max(float(min_lim), min(float(max_lim), cur))
                    self._present_float[sid] = cur
                    _set_u16_le(mem, REG_PRESENT, int(round(cur)))

    def _read_loop(self) -> None:
        buf = bytearray()
        while not self._stop.is_set():
            fd = self._master_fd
            if fd < 0:
                break
            try:
                rlist, _, _ = select.select([fd], [], [], 0.02)
            except (OSError, ValueError):
                continue
            if not rlist:
                continue
            try:
                chunk = os.read(fd, 4096)
            except BlockingIOError:
                continue
            except OSError as exc:
                if exc.errno == errno.EIO:
                    time.sleep(0.01)
                    continue
                if self._stop.is_set():
                    break
                continue
            if not chunk:
                time.sleep(0.01)
                continue
            buf.extend(chunk)
            self._drain_frames(buf)

    def _drain_frames(self, buf: bytearray) -> None:
        while True:
            idx = buf.find(b"\xff\xff")
            if idx < 0:
                if len(buf) > 1:
                    del buf[:-1]
                return
            if idx > 0:
                del buf[:idx]
            if len(buf) < 4:
                return
            length = buf[3]
            if length < 2:
                del buf[:2]
                continue
            frame_len = 4 + length
            if len(buf) < frame_len:
                return
            frame = bytes(buf[:frame_len])
            del buf[:frame_len]
            self._handle_frame(frame)

    def _append_event(self, ev: dict[str, Any]) -> None:
        self._seq += 1
        ev["seq"] = self._seq
        ev["ts_ms"] = _now_ms()
        self._events.append(ev)
        if len(self._events) > self._max_events:
            del self._events[: len(self._events) - self._max_events]

    def _handle_frame(self, frame: bytes) -> None:
        expected_cksum = sts_checksum(frame[2:-1])
        if expected_cksum != frame[-1]:
            with self._cond:
                self._checksum_errors += 1
                self._append_event(
                    {"kind": "checksum_error", "raw_hex": frame.hex()}
                )
                self._cond.notify_all()
            return

        servo_id = frame[2]
        inst = frame[4]
        params = frame[5:-1]

        reply: bytes | None = None
        delay_s = 0.0
        echo = False

        with self._cond:
            echo = self._echo_enabled
            delay_s = self._response_delay_s
            err_byte = self._status_error_byte
            corrupt = self._corrupt_checksum
            dropped = servo_id in self._drop_replies_for

            if inst == INST_PING:
                self._append_event({"kind": "ping", "id": servo_id})
                if servo_id in self._mem and not dropped:
                    reply = sts_packet(servo_id, err_byte)

            elif inst == INST_READ and len(params) == 2:
                reg, count = params[0], params[1]
                if servo_id in self._mem and reg + count <= 64:
                    data = bytes(self._mem[servo_id][reg : reg + count])
                    self._append_event(
                        {
                            "kind": "read",
                            "id": servo_id,
                            "reg": reg,
                            "len": count,
                            "data": list(data),
                        }
                    )
                    if not dropped:
                        reply = sts_packet(servo_id, err_byte, data)

            elif inst == INST_WRITE and len(params) >= 2:
                reg = params[0]
                data = params[1:]
                if servo_id in self._mem:
                    self._apply_register_write(servo_id, reg, data)
                    if not dropped:
                        reply = sts_packet(servo_id, err_byte)

            elif inst == INST_SYNC_WRITE and servo_id == BROADCAST_ID and len(params) >= 2:
                reg = params[0]
                item_len = params[1]
                stride = 1 + item_len
                rest = params[2:]
                if item_len > 0 and len(rest) % stride == 0:
                    if reg == REG_ACCEL and item_len == 7:
                        self._apply_goal_sync_write(rest, stride)
                    else:
                        for off in range(0, len(rest), stride):
                            sid = rest[off]
                            data = rest[off + 1 : off + stride]
                            if sid in self._mem:
                                self._apply_register_write(sid, reg, data)
            self._cond.notify_all()

        if delay_s > 0:
            time.sleep(delay_s)

        out = bytearray()
        if echo:
            out.extend(frame)
        if reply is not None:
            if corrupt:
                bad = bytearray(reply)
                bad[-1] ^= 0x01
                out.extend(bad)
            else:
                out.extend(reply)
        if out:
            self._write_master(bytes(out))

    def _apply_register_write(self, sid: int, reg: int, data: bytes) -> None:
        mem = self._mem[sid]
        if reg + len(data) > 64:
            return
        eeprom_violation = reg < 40 and mem[REG_LOCK] != 0
        if eeprom_violation:
            v = {
                "ts_ms": _now_ms(),
                "id": sid,
                "reg": reg,
                "data": list(data),
                "reason": "eeprom_locked",
            }
            self._eeprom_lock_violations.append(v)
            self._append_event(
                {
                    "kind": "write_rejected",
                    "id": sid,
                    "reg": reg,
                    "data": list(data),
                    "reason": "eeprom_locked",
                }
            )
            return

        limit_violation = False
        if reg == REG_GOAL and len(data) == 2:
            goal = struct.unpack_from("<H", data, 0)[0]
            limit_violation = self._check_goal_limit(sid, goal)

        if self._forget_write != (sid, reg):
            mem[reg : reg + len(data)] = data
            if reg == REG_PRESENT and len(data) == 2:
                self._present_float[sid] = float(struct.unpack_from("<H", data, 0)[0])

        self._append_event(
            {
                "kind": "write",
                "id": sid,
                "reg": reg,
                "data": list(data),
                "limit_violation": limit_violation,
            }
        )

    def _check_goal_limit(self, sid: int, goal: int) -> bool:
        joint = self._joints_by_id[sid]
        cal_min, cal_max = int(joint["min_steps"]), int(joint["max_steps"])
        mem = self._mem[sid]
        ee_min = _u16_le(mem, REG_LIMITS)
        ee_max = _u16_le(mem, REG_LIMITS + 2)
        violated = not (cal_min <= goal <= cal_max) or not (ee_min <= goal <= ee_max)
        if violated:
            self._limit_violations.append(
                {
                    "ts_ms": _now_ms(),
                    "id": sid,
                    "goal": goal,
                    "cal_min": cal_min,
                    "cal_max": cal_max,
                    "eeprom_min": ee_min,
                    "eeprom_max": ee_max,
                }
            )
        return violated

    def _apply_goal_sync_write(self, rest: bytes, stride: int) -> None:
        now_mono = time.monotonic()
        steps_arr: list[int | None] = [None, None, None]
        deg_arr: list[float | None] = [None, None, None]
        goals_by_id: dict[str, int] = {}
        speeds_by_id: dict[str, int] = {}
        accels_by_id: dict[str, int] = {}
        any_violation = False

        for off in range(0, len(rest), stride):
            sid = rest[off]
            payload = rest[off + 1 : off + stride]
            if sid not in self._mem:
                continue
            accel = payload[0]
            goal = struct.unpack_from("<H", payload, 1)[0]
            move_time = struct.unpack_from("<H", payload, 3)[0]
            speed = struct.unpack_from("<H", payload, 5)[0]

            violated = self._check_goal_limit(sid, goal)
            any_violation = any_violation or violated

            mem = self._mem[sid]
            if self._forget_write != (sid, REG_GOAL):
                mem[REG_ACCEL] = accel
                _set_u16_le(mem, REG_GOAL, goal)
                _set_u16_le(mem, REG_TIME, move_time)
                _set_u16_le(mem, REG_SPEED, speed)

            goals_by_id[str(sid)] = goal
            speeds_by_id[str(sid)] = speed
            accels_by_id[str(sid)] = accel

            idx = self._axis_index_by_id[sid]
            j = self._joints_by_id[sid]
            sign = -1.0 if j["invert"] else 1.0
            deg = round((goal - int(j["center"])) * DEG_PER_STEP * sign, 3)
            steps_arr[idx] = goal
            deg_arr[idx] = deg

        self._goal_frames_count += 1
        self._goal_timestamps_mono.append(now_mono)
        if len(self._goal_timestamps_mono) > 200:
            del self._goal_timestamps_mono[: len(self._goal_timestamps_mono) - 200]

        goal_entry = {
            "kind": "goals",
            "goals": goals_by_id,
            "steps": steps_arr,
            "degrees": deg_arr,
            "speeds": speeds_by_id,
            "accels": accels_by_id,
            "limit_violation": any_violation,
        }
        self._append_event(goal_entry)
        self._goals.append(goal_entry)
        if len(self._goals) > self._max_goals:
            del self._goals[: len(self._goals) - self._max_goals]

    def _cadence_stats_locked(self) -> dict[str, Any]:
        ts = self._goal_timestamps_mono
        if len(ts) < 2:
            return {"hz": 0.0, "samples": len(ts), "p95_ms": 0.0, "max_ms": 0.0}
        intervals_ms = sorted((ts[i] - ts[i - 1]) * 1000.0 for i in range(1, len(ts)))
        # Keep only recent active burst (ignore gaps > 1s across restarts)
        active = [x for x in intervals_ms if x < 1000.0]
        if not active:
            return {"hz": 0.0, "samples": 0, "p95_ms": 0.0, "max_ms": 0.0}
        avg_ms = sum(active) / len(active)
        p95_idx = max(0, min(len(active) - 1, int(len(active) * 0.95)))
        return {
            "hz": round(1000.0 / avg_ms, 2) if avg_ms > 0 else 0.0,
            "samples": len(active),
            "p95_ms": round(active[p95_idx], 2),
            "max_ms": round(active[-1], 2),
        }

    # ---- Public control API ----

    def status(self) -> dict[str, Any]:
        with self._lock:
            joints_out: dict[str, Any] = {}
            for axis in self._axes_order:
                j = self.calibration[axis]
                sid = int(j["id"])
                mem = self._mem[sid]
                present = _u16_le(mem, REG_PRESENT)
                goal = _u16_le(mem, REG_GOAL)
                sign = -1.0 if j["invert"] else 1.0
                joints_out[str(sid)] = {
                    "axis": axis,
                    "id": sid,
                    "present": present,
                    "present_deg": round((present - int(j["center"])) * DEG_PER_STEP * sign, 3),
                    "goal": goal,
                    "goal_deg": round((goal - int(j["center"])) * DEG_PER_STEP * sign, 3),
                    "torque": int(mem[REG_TORQUE]),
                    "lock": int(mem[REG_LOCK]),
                    "mode": int(mem[REG_MODE]),
                    "accel": int(mem[REG_ACCEL]),
                    "speed": _u16_le(mem, REG_SPEED),
                    "eeprom_min_steps": _u16_le(mem, REG_LIMITS),
                    "eeprom_max_steps": _u16_le(mem, REG_LIMITS + 2),
                    "cal_min_steps": int(j["min_steps"]),
                    "cal_max_steps": int(j["max_steps"]),
                    "center": int(j["center"]),
                }
            cadence = self._cadence_stats_locked()
            return {
                "pty_path": str(self.link_path),
                "slave_path": self._slave_name,
                "calibration_path": str(self.calibration_path),
                "termios": inspect_servo_termios(self._slave_fd) if self._slave_fd >= 0 else None,
                "echo_enabled": self._echo_enabled,
                "joints": joints_out,
                "goal_frames_count": self._goal_frames_count,
                "limit_violations": len(self._limit_violations),
                "limit_violations_count": len(self._limit_violations),
                "limit_violation_events": list(self._limit_violations[-20:]),
                "eeprom_lock_violations": len(self._eeprom_lock_violations),
                "eeprom_lock_violations_count": len(self._eeprom_lock_violations),
                "eeprom_lock_violation_events": list(self._eeprom_lock_violations[-20:]),
                "checksum_errors": self._checksum_errors,
                "cadence_hz": cadence["hz"],
                "cadence": cadence,
                "latest_seq": self._seq,
            }

    def history(self, kind: str = "all", since_seq: int = 0, limit: int = 500) -> dict[str, Any]:
        with self._lock:
            if kind == "goals":
                items = [e for e in self._goals if e["seq"] > since_seq]
            elif kind == "writes":
                items = [
                    e
                    for e in self._events
                    if e["seq"] > since_seq and e["kind"] in ("write", "write_rejected")
                ]
            elif kind == "reads":
                items = [
                    e for e in self._events if e["seq"] > since_seq and e["kind"] == "read"
                ]
            else:
                items = [e for e in self._events if e["seq"] > since_seq]
            if limit > 0:
                items = items[-limit:]
            return {
                "kind": kind,
                "events": items,
                "latest_seq": self._seq,
                "goal_frames_count": self._goal_frames_count,
                "limit_violations_count": len(self._limit_violations),
                "eeprom_lock_violations_count": len(self._eeprom_lock_violations),
            }

    def clear_history(self) -> dict[str, Any]:
        with self._lock:
            self._events.clear()
            self._goals.clear()
            self._limit_violations.clear()
            self._eeprom_lock_violations.clear()
            self._checksum_errors = 0
            self._goal_timestamps_mono.clear()
            return {"cleared": True, "seq": self._seq}

    def set_register(self, servo_id: int, reg: int, data: list[int]) -> dict[str, Any]:
        sid = int(servo_id)
        reg = int(reg)
        raw = bytes(int(b) & 0xFF for b in data)
        with self._cond:
            if sid not in self._mem:
                raise ValueError(f"unknown servo id {sid}")
            if reg < 0 or reg + len(raw) > 64:
                raise ValueError(f"register range out of bounds: {reg}..{reg + len(raw)}")
            self._mem[sid][reg : reg + len(raw)] = raw
            if reg == REG_PRESENT and len(raw) == 2:
                self._present_float[sid] = float(struct.unpack_from("<H", raw, 0)[0])
            self._cond.notify_all()
            return {"id": sid, "reg": reg, "bytes": list(raw)}

    def set_fault(
        self,
        drop_replies_for: list[int] | None = None,
        corrupt_checksum: bool | None = None,
        status_error_byte: int | None = None,
        echo_enabled: bool | None = None,
        response_delay_ms: float | None = None,
        forget_write: list[int] | None = None,
        clear_forget_write: bool = False,
    ) -> dict[str, Any]:
        with self._lock:
            if drop_replies_for is not None:
                self._drop_replies_for = {int(x) for x in drop_replies_for}
            if corrupt_checksum is not None:
                self._corrupt_checksum = bool(corrupt_checksum)
            if status_error_byte is not None:
                self._status_error_byte = int(status_error_byte) & 0xFF
            if echo_enabled is not None:
                self._echo_enabled = bool(echo_enabled)
            if response_delay_ms is not None:
                self._response_delay_s = max(0.0, float(response_delay_ms) / 1000.0)
            if clear_forget_write:
                self._forget_write = None
            elif forget_write is not None:
                if len(forget_write) != 2:
                    raise ValueError("forget_write must be [servo_id, reg]")
                self._forget_write = (int(forget_write[0]), int(forget_write[1]))
            return {
                "drop_replies_for": sorted(self._drop_replies_for),
                "corrupt_checksum": self._corrupt_checksum,
                "status_error_byte": self._status_error_byte,
                "echo_enabled": self._echo_enabled,
                "response_delay_ms": round(self._response_delay_s * 1000.0, 2),
                "forget_write": list(self._forget_write) if self._forget_write else None,
            }

    def wait_for_goal(
        self,
        axis: int | None = None,
        min_delta_steps: int | None = None,
        baseline_steps: int | None = None,
        target_steps: int | None = None,
        tolerance: int = 8,
        torque: int | None = None,
        after_seq: int = 0,
        timeout_ms: int = 5000,
    ) -> dict[str, Any]:
        deadline = time.monotonic() + (timeout_ms / 1000.0)
        with self._cond:
            if baseline_steps is None and axis is not None:
                ax_name = self._axes_order[int(axis)]
                baseline_steps = int(self.calibration[ax_name]["center"])
            while True:
                if torque is not None:
                    torques = [int(self._mem[int(self.calibration[a]["id"])][REG_TORQUE]) for a in self._axes_order]
                    if all(t == int(torque) for t in torques):
                        return {"matched": True, "torques": torques, "seq": self._seq}
                else:
                    for ev in self._goals:
                        if ev["seq"] <= after_seq:
                            continue
                        if axis is None:
                            return {"matched": True, "event": ev}
                        val = ev["steps"][int(axis)]
                        if val is None:
                            continue
                        if target_steps is not None and abs(val - int(target_steps)) <= int(tolerance):
                            return {"matched": True, "event": ev}
                        if (
                            min_delta_steps is not None
                            and baseline_steps is not None
                            and abs(val - int(baseline_steps)) >= int(min_delta_steps)
                        ):
                            return {"matched": True, "event": ev}
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"servo.wait_for_goal timed out after {timeout_ms}ms "
                        f"(axis={axis}, target={target_steps}, min_delta={min_delta_steps}, torque={torque})"
                    )
                self._cond.wait(timeout=min(remaining, 0.05))

    def assert_bounds(self, since_seq: int = 0) -> dict[str, Any]:
        with self._lock:
            if self._limit_violations:
                raise AssertionError(f"servo limit violations detected: {self._limit_violations}")
            if self._eeprom_lock_violations:
                raise AssertionError(
                    f"servo EEPROM lock violations detected: {self._eeprom_lock_violations}"
                )
            if self._checksum_errors > 0:
                raise AssertionError(f"servo checksum errors detected: {self._checksum_errors}")
            goals = [g for g in self._goals if g["seq"] > since_seq]
            per_axis: dict[str, dict[str, Any]] = {}
            for idx, axis in enumerate(self._axes_order):
                vals = [g["steps"][idx] for g in goals if g["steps"][idx] is not None]
                j = self.calibration[axis]
                per_axis[axis] = {
                    "min_observed": min(vals) if vals else None,
                    "max_observed": max(vals) if vals else None,
                    "cal_min": int(j["min_steps"]),
                    "cal_max": int(j["max_steps"]),
                    "samples": len(vals),
                }
            return {
                "ok": True,
                "goal_frames_checked": len(goals),
                "limit_violations": 0,
                "eeprom_lock_violations": 0,
                "checksum_errors": 0,
                "axes": per_axis,
            }
