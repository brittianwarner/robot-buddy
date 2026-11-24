"""Simulated rpicam-vid I420 camera bridge for Linux ARM64 Pip E2E QA.

Matches apps/robot/src/vision/mod.rs & native.rs:
- Emits exact packed I420 frames: 384 x 288 x 3 / 2 = 165,888 bytes.
- Pure Python stdlib (zero pip dependencies): supports blank gray frames, raw
  packed .i420 files, binary P6 .ppm files (BT.601 RGB -> I420), controllable
  (offset_x, offset_y) shifting for real YuNet face tracking, two-face side-by-side
  composition for enrollment multi-face rejection, 180-degree rotation, stall
  faults, truncated-frame faults, and abrupt disconnect faults.
- Communicates with <run_dir>/bin/rpicam-vid over <run_dir>/ipc/camera.sock so the
  shim works cleanly under vision/mod.rs's cmd.env_clear().env("PATH", "/usr/bin:/bin").
"""

from __future__ import annotations

import json
import os
import socket
import struct
import threading
import time
from pathlib import Path
from typing import Any

WIDTH = 384
HEIGHT = 288
Y_SIZE = WIDTH * HEIGHT
UV_W = WIDTH // 2
UV_H = HEIGHT // 2
UV_SIZE = UV_W * UV_H
FRAME_BYTES = Y_SIZE + 2 * UV_SIZE  # 165,888 bytes

OP_FRAME = 1
OP_TRUNCATED_EXIT = 2
OP_EXIT = 3


def _now_ms() -> float:
    return round(time.time() * 1000.0, 3)


def make_blank_i420(y: int = 128, u: int = 128, v: int = 128) -> bytes:
    return bytes([y & 0xFF]) * Y_SIZE + bytes([u & 0xFF]) * UV_SIZE + bytes([v & 0xFF]) * UV_SIZE


def rotate_180_i420(frame: bytes) -> bytes:
    if len(frame) != FRAME_BYTES:
        raise ValueError(f"expected {FRAME_BYTES} bytes, got {len(frame)}")
    y_plane = frame[:Y_SIZE][::-1]
    u_plane = frame[Y_SIZE : Y_SIZE + UV_SIZE][::-1]
    v_plane = frame[Y_SIZE + UV_SIZE :][::-1]
    return y_plane + u_plane + v_plane


def shift_i420(frame: bytes, offset_x: int = 0, offset_y: int = 0) -> bytes:
    """Shift a 384x288 packed I420 frame by even pixel offsets onto a neutral gray canvas."""
    if len(frame) != FRAME_BYTES:
        raise ValueError(f"expected {FRAME_BYTES} bytes, got {len(frame)}")
    ox = int(offset_x)
    oy = int(offset_y)
    # Align to even pixels to preserve 4:2:0 chroma subsampling.
    ox = ox - (ox % 2)
    oy = oy - (oy % 2)
    if ox == 0 and oy == 0:
        return frame

    y_out = bytearray([128] * Y_SIZE)
    u_out = bytearray([128] * UV_SIZE)
    v_out = bytearray([128] * UV_SIZE)

    y_src = memoryview(frame)[:Y_SIZE]
    u_src = memoryview(frame)[Y_SIZE : Y_SIZE + UV_SIZE]
    v_src = memoryview(frame)[Y_SIZE + UV_SIZE :]

    # Copy Y rows
    for dst_y in range(HEIGHT):
        src_y = dst_y - oy
        if 0 <= src_y < HEIGHT:
            src_x0 = max(0, -ox)
            dst_x0 = max(0, ox)
            span = WIDTH - abs(ox)
            if span > 0:
                s0 = src_y * WIDTH + src_x0
                d0 = dst_y * WIDTH + dst_x0
                y_out[d0 : d0 + span] = y_src[s0 : s0 + span]

    # Copy U and V rows
    uox = ox // 2
    uoy = oy // 2
    for dst_y in range(UV_H):
        src_y = dst_y - uoy
        if 0 <= src_y < UV_H:
            src_x0 = max(0, -uox)
            dst_x0 = max(0, uox)
            span = UV_W - abs(uox)
            if span > 0:
                s0 = src_y * UV_W + src_x0
                d0 = dst_y * UV_W + dst_x0
                u_out[d0 : d0 + span] = u_src[s0 : s0 + span]
                v_out[d0 : d0 + span] = v_src[s0 : s0 + span]

    return bytes(y_out) + bytes(u_out) + bytes(v_out)


def _downsample_half_i420(frame: bytes) -> tuple[bytes, bytes, bytes]:
    """Downsample 384x288 I420 by 2x -> 192x144 (Y: 192x144, U/V: 96x72)."""
    y_half = bytearray(192 * 144)
    u_half = bytearray(96 * 72)
    v_half = bytearray(96 * 72)
    for row in range(144):
        src_row = (row * 2) * WIDTH
        dst_row = row * 192
        y_half[dst_row : dst_row + 192] = frame[src_row : src_row + WIDTH : 2]
    u_base = Y_SIZE
    v_base = Y_SIZE + UV_SIZE
    for row in range(72):
        src_row = (row * 2) * UV_W
        dst_row = row * 96
        u_half[dst_row : dst_row + 96] = frame[u_base + src_row : u_base + src_row + UV_W : 2]
        v_half[dst_row : dst_row + 96] = frame[v_base + src_row : v_base + src_row + UV_W : 2]
    return bytes(y_half), bytes(u_half), bytes(v_half)


def compose_two_faces_i420(frame_a: bytes, frame_b: bytes) -> bytes:
    """Place two 384x288 I420 face frames side-by-side (each 192x144 centered vertically)."""
    ya, ua, va = _downsample_half_i420(frame_a)
    yb, ub, vb = _downsample_half_i420(frame_b)
    y_out = bytearray([128] * Y_SIZE)
    u_out = bytearray([128] * UV_SIZE)
    v_out = bytearray([128] * UV_SIZE)

    y_top = (HEIGHT - 144) // 2  # 72 (even)
    for row in range(144):
        dst = (y_top + row) * WIDTH
        src = row * 192
        y_out[dst : dst + 192] = ya[src : src + 192]
        y_out[dst + 192 : dst + 384] = yb[src : src + 192]

    uv_top = y_top // 2  # 36
    for row in range(72):
        dst = (uv_top + row) * UV_W
        src = row * 96
        u_out[dst : dst + 96] = ua[src : src + 96]
        u_out[dst + 96 : dst + 192] = ub[src : src + 96]
        v_out[dst : dst + 96] = va[src : src + 96]
        v_out[dst + 96 : dst + 192] = vb[src : src + 96]

    return bytes(y_out) + bytes(u_out) + bytes(v_out)


def load_ppm_as_i420(path: Path) -> bytes:
    """Load a binary P6 PPM image in pure Python stdlib and convert to 384x288 BT.601 I420."""
    data = path.read_bytes()
    pos = 0
    tokens: list[bytes] = []
    n = len(data)
    while len(tokens) < 4 and pos < n:
        while pos < n and data[pos] in b" \t\r\n":
            pos += 1
        if pos < n and data[pos] == ord("#"):
            while pos < n and data[pos] != ord("\n"):
                pos += 1
            continue
        start = pos
        while pos < n and data[pos] not in b" \t\r\n":
            pos += 1
        tokens.append(data[start:pos])
    if len(tokens) < 4 or tokens[0] != b"P6":
        raise ValueError(f"unsupported PPM format in {path}")
    src_w = int(tokens[1])
    src_h = int(tokens[2])
    maxval = int(tokens[3])
    if maxval != 255:
        raise ValueError(f"unsupported PPM maxval={maxval}")
    pos += 1  # single whitespace byte after maxval
    rgb = memoryview(data)[pos:]
    if len(rgb) < src_w * src_h * 3:
        raise ValueError(f"truncated PPM pixel buffer in {path}")

    # Resize/letterbox to 384x288 with nearest neighbor
    y_plane = bytearray(Y_SIZE)
    u_plane = bytearray(UV_SIZE)
    v_plane = bytearray(UV_SIZE)

    # Precompute Y and store RGB for 2x2 UV averaging
    r_buf = bytearray(Y_SIZE)
    g_buf = bytearray(Y_SIZE)
    b_buf = bytearray(Y_SIZE)

    for dy in range(HEIGHT):
        sy = (dy * src_h) // HEIGHT
        row_off = sy * src_w * 3
        dst_off = dy * WIDTH
        for dx in range(WIDTH):
            sx = (dx * src_w) // WIDTH
            p = row_off + sx * 3
            r = rgb[p]
            g = rgb[p + 1]
            b = rgb[p + 2]
            idx = dst_off + dx
            r_buf[idx] = r
            g_buf[idx] = g
            b_buf[idx] = b
            # BT.601 full/standard match for OpenCV COLOR_YUV2BGR_I420
            yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16
            y_plane[idx] = 0 if yv < 0 else (255 if yv > 255 else yv)

    for uy in range(UV_H):
        y0 = (uy * 2) * WIDTH
        y1 = y0 + WIDTH
        u_row = uy * UV_W
        for ux in range(UV_W):
            x0 = ux * 2
            x1 = x0 + 1
            r = (r_buf[y0 + x0] + r_buf[y0 + x1] + r_buf[y1 + x0] + r_buf[y1 + x1]) >> 2
            g = (g_buf[y0 + x0] + g_buf[y0 + x1] + g_buf[y1 + x0] + g_buf[y1 + x1]) >> 2
            b = (b_buf[y0 + x0] + b_buf[y0 + x1] + b_buf[y1 + x0] + b_buf[y1 + x1]) >> 2
            uv = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128
            vv = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128
            u_plane[u_row + ux] = 0 if uv < 0 else (255 if uv > 255 else uv)
            v_plane[u_row + ux] = 0 if vv < 0 else (255 if vv > 255 else vv)

    return bytes(y_plane) + bytes(u_plane) + bytes(v_plane)


class CameraBridge:
    """IPC server feeding simulated I420 frames to <run_dir>/bin/rpicam-vid."""

    def __init__(self, sock_path: Path) -> None:
        self.sock_path = Path(sock_path)
        self.sock_path.parent.mkdir(parents=True, exist_ok=True)
        if self.sock_path.exists():
            self.sock_path.unlink()

        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._frame_cache: dict[str, bytes] = {}

        self._scene: dict[str, Any] = {
            "kind": "blank",
            "path": None,
            "path_b": None,
            "offset_x": 0,
            "offset_y": 0,
            "fps": 15.0,
            "truncate_after_frames": None,
        }
        self._current_frame: bytes = make_blank_i420()
        self._scene_generation: int = 1

        self._active_clients: int = 0
        self._spawns: int = 0
        self._frames_sent: int = 0
        self._last_argv: list[str] | None = None
        self._last_rotation: int = 0
        self._last_spawn_ms: float | None = None

        self._srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._srv.bind(str(self.sock_path))
        os.chmod(self.sock_path, 0o660)
        self._srv.listen(8)
        self._srv.settimeout(0.2)

        self._accept_thread = threading.Thread(
            target=self._accept_loop, name="camera-bridge-accept", daemon=True
        )
        self._accept_thread.start()

    def close(self) -> None:
        self._stop.set()
        try:
            self._srv.close()
        except OSError:
            pass
        self._accept_thread.join(timeout=1.5)
        try:
            if self.sock_path.exists():
                self.sock_path.unlink()
        except OSError:
            pass

    def _load_i420_cached(self, path_str: str, kind: str = "i420") -> bytes:
        p = Path(path_str)
        key = f"{kind}:{p.resolve()}:{p.stat().st_mtime_ns}"
        cached = self._frame_cache.get(key)
        if cached is not None:
            return cached
        if kind == "ppm" or p.suffix.lower() == ".ppm":
            frame = load_ppm_as_i420(p)
        else:
            raw = p.read_bytes()
            if len(raw) != FRAME_BYTES:
                raise ValueError(
                    f"I420 file {p} must be exactly {FRAME_BYTES} bytes (384x288 I420), got {len(raw)}"
                )
            frame = raw
        self._frame_cache[key] = frame
        return frame

    def set_scene(
        self,
        kind: str = "blank",
        path: str | None = None,
        path_b: str | None = None,
        offset_x: float = 0,
        offset_y: float = 0,
        fps: float | None = None,
        truncate_after_frames: int | None = None,
    ) -> dict[str, Any]:
        kind = str(kind)
        if kind not in ("blank", "i420", "ppm", "two_faces", "stall", "disconnect"):
            raise ValueError(f"unsupported camera scene kind: {kind}")

        ox_raw = float(offset_x)
        oy_raw = float(offset_y)
        px_x = int(round(ox_raw * (WIDTH / 2.0))) if 0.0 < abs(ox_raw) < 1.0 else int(round(ox_raw))
        px_y = int(round(oy_raw * (HEIGHT / 2.0))) if 0.0 < abs(oy_raw) < 1.0 else int(round(oy_raw))

        if kind == "blank":
            frame = make_blank_i420()
        elif kind in ("i420", "ppm"):
            if not path:
                raise ValueError(f"scene kind={kind} requires path")
            base = self._load_i420_cached(path, kind=kind)
            frame = shift_i420(base, offset_x=px_x, offset_y=px_y)
        elif kind == "two_faces":
            if not path:
                raise ValueError("scene kind=two_faces requires path (and optional path_b)")
            fa = self._load_i420_cached(path)
            fb = self._load_i420_cached(path_b or path)
            frame = compose_two_faces_i420(fa, fb)
        else:
            frame = self._current_frame

        with self._lock:
            self._scene = {
                "kind": kind,
                "path": path,
                "path_b": path_b,
                "offset_x": px_x,
                "offset_y": px_y,
                "fps": float(fps) if fps is not None else self._scene["fps"],
                "truncate_after_frames": (
                    int(truncate_after_frames) if truncate_after_frames is not None else None
                ),
            }
            self._current_frame = frame
            self._scene_generation += 1
            return {
                "scene": dict(self._scene),
                "generation": self._scene_generation,
                "frame_bytes": len(self._current_frame),
            }

    def status(self) -> dict[str, Any]:
        with self._lock:
            return {
                "sock_path": str(self.sock_path),
                "active_clients": self._active_clients,
                "spawns": self._spawns,
                "frames_sent": self._frames_sent,
                "scene": dict(self._scene),
                "scene_generation": self._scene_generation,
                "last_argv": self._last_argv,
                "last_rotation": self._last_rotation,
                "last_spawn_ms": self._last_spawn_ms,
                "width": WIDTH,
                "height": HEIGHT,
                "frame_bytes": FRAME_BYTES,
            }

    def _accept_loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            t = threading.Thread(
                target=self._serve_client,
                args=(conn,),
                name="camera-bridge-client",
                daemon=True,
            )
            t.start()

    def _serve_client(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(5.0)
            hello_raw = bytearray()
            while b"\n" not in hello_raw and len(hello_raw) < 8192:
                chunk = conn.recv(1024)
                if not chunk:
                    return
                hello_raw.extend(chunk)
            line = bytes(hello_raw.split(b"\n", 1)[0]).decode("utf-8", errors="replace")
            meta = json.loads(line)
            rotation = int(meta.get("rotation", 0))
            req_fps = float(meta.get("framerate", 15.0))
            max_frames = int(meta.get("frames", 0))
            argv = meta.get("argv", [])

            with self._lock:
                self._active_clients += 1
                self._spawns += 1
                self._last_argv = argv
                self._last_rotation = rotation
                self._last_spawn_ms = _now_ms()

            conn.settimeout(2.0)
            sent_for_client = 0
            next_tick = time.monotonic()
            while not self._stop.is_set():
                with self._lock:
                    scene = dict(self._scene)
                    frame = self._current_frame

                kind = scene["kind"]
                if kind == "disconnect":
                    conn.sendall(struct.pack("!BI", OP_EXIT, 1) + b"\x01")
                    return
                if kind == "stall":
                    time.sleep(0.05)
                    next_tick = time.monotonic()
                    continue

                trunc_after = scene.get("truncate_after_frames")
                if trunc_after is not None and sent_for_client >= trunc_after:
                    tail = b"tail"
                    conn.sendall(struct.pack("!BI", OP_TRUNCATED_EXIT, len(tail)) + tail)
                    return

                out_frame = rotate_180_i420(frame) if rotation == 180 else frame
                conn.sendall(struct.pack("!BI", OP_FRAME, len(out_frame)) + out_frame)
                sent_for_client += 1
                with self._lock:
                    self._frames_sent += 1

                if max_frames > 0 and sent_for_client >= max_frames:
                    conn.sendall(struct.pack("!BI", OP_EXIT, 1) + b"\x00")
                    return

                fps = max(1.0, float(scene.get("fps") or req_fps or 15.0))
                period = 1.0 / fps
                next_tick += period
                sleep_s = next_tick - time.monotonic()
                if sleep_s > 0:
                    time.sleep(sleep_s)
                else:
                    next_tick = time.monotonic()
        except (OSError, ValueError, json.JSONDecodeError):
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass
            with self._lock:
                if self._active_clients > 0:
                    self._active_clients -= 1
