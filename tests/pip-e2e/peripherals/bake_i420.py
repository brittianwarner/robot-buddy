#!/usr/bin/env python3
"""Convert verified JPEG face portraits into 384x288 packed I420 frames (165,888 bytes).

Tries available local decoders in order:
1. OpenCV Python (cv2)
2. ffmpeg
3. Pillow (PIL) + pure-Python BT.601 RGB->I420
4. g++/clang++ + pkg-config opencv4 (exact OpenCV COLOR_BGR2YUV_I420, used in pi-build-tools)
5. macOS `sips` -> 24-bit BMP + pure-Python BT.601 RGB->I420

Also composes `two_faces.i420` side-by-side for multi-face enrollment rejection tests.
"""

from __future__ import annotations

import os
import platform
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

THIS_DIR = Path(__file__).resolve().parent
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

from camera_bridge import (  # noqa: E402
    FRAME_BYTES,
    HEIGHT,
    UV_H,
    UV_SIZE,
    UV_W,
    WIDTH,
    Y_SIZE,
    compose_two_faces_i420,
)


def _rgb24_to_i420_letterbox(rgb: bytes, src_w: int, src_h: int) -> bytes:
    """Aspect-preserving resize + center letterbox on neutral gray, then BT.601 I420."""
    scale = min(WIDTH / src_w, HEIGHT / src_h)
    dst_w = max(2, int(round(src_w * scale)) & ~1)
    dst_h = max(2, int(round(src_h * scale)) & ~1)
    pad_x = ((WIDTH - dst_w) // 2) & ~1
    pad_y = ((HEIGHT - dst_h) // 2) & ~1

    r_buf = bytearray([128] * Y_SIZE)
    g_buf = bytearray([128] * Y_SIZE)
    b_buf = bytearray([128] * Y_SIZE)

    for dy in range(dst_h):
        sy = min(src_h - 1, int(dy * src_h / dst_h))
        src_row = sy * src_w * 3
        dst_row = (pad_y + dy) * WIDTH + pad_x
        for dx in range(dst_w):
            sx = min(src_w - 1, int(dx * src_w / dst_w))
            p = src_row + sx * 3
            idx = dst_row + dx
            r_buf[idx] = rgb[p]
            g_buf[idx] = rgb[p + 1]
            b_buf[idx] = rgb[p + 2]

    y_plane = bytearray(Y_SIZE)
    u_plane = bytearray(UV_SIZE)
    v_plane = bytearray(UV_SIZE)

    for idx in range(Y_SIZE):
        r, g, b = r_buf[idx], g_buf[idx], b_buf[idx]
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

    out = bytes(y_plane) + bytes(u_plane) + bytes(v_plane)
    assert len(out) == FRAME_BYTES
    return out


def _try_cv2(src: Path, dst: Path) -> bool:
    try:
        import cv2  # type: ignore
        import numpy as np  # type: ignore
    except ImportError:
        return False
    img = cv2.imread(str(src), cv2.IMREAD_COLOR)
    if img is None:
        return False
    h, w = img.shape[:2]
    scale = min(WIDTH / w, HEIGHT / h)
    nw = max(2, int(round(w * scale)) & ~1)
    nh = max(2, int(round(h * scale)) & ~1)
    resized = cv2.resize(img, (nw, nh), interpolation=cv2.INTER_AREA)
    canvas = np.full((HEIGHT, WIDTH, 3), 128, dtype=np.uint8)
    px = ((WIDTH - nw) // 2) & ~1
    py = ((HEIGHT - nh) // 2) & ~1
    canvas[py : py + nh, px : px + nw] = resized
    i420 = cv2.cvtColor(canvas, cv2.COLOR_BGR2YUV_I420)
    raw = i420.tobytes()
    assert len(raw) == FRAME_BYTES
    dst.write_bytes(raw)
    return True


def _try_ffmpeg(src: Path, dst: Path) -> bool:
    ff = shutil.which("ffmpeg")
    if not ff:
        return False
    vf = (
        f"scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,"
        f"pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2:color=0x808080,"
        "format=yuv420p"
    )
    res = subprocess.run(
        [ff, "-v", "error", "-y", "-i", str(src), "-vf", vf, "-frames:v", "1", "-f", "rawvideo", str(dst)],
        check=False,
    )
    return res.returncode == 0 and dst.is_file() and dst.stat().st_size == FRAME_BYTES


def _try_pillow(src: Path, dst: Path) -> bool:
    try:
        from PIL import Image  # type: ignore
    except ImportError:
        return False
    with Image.open(src) as im:
        rgb_im = im.convert("RGB")
        raw = _rgb24_to_i420_letterbox(rgb_im.tobytes(), rgb_im.width, rgb_im.height)
    dst.write_bytes(raw)
    return True


def _try_opencv_cpp(pairs: list[tuple[Path, Path]]) -> bool:
    cxx = shutil.which("g++") or shutil.which("clang++")
    pkg = shutil.which("pkg-config")
    if not cxx or not pkg:
        return False
    check = subprocess.run([pkg, "--exists", "opencv4"], check=False)
    if check.returncode != 0:
        return False
    cflags = subprocess.check_output([pkg, "--cflags", "opencv4"], text=True).split()
    libs = subprocess.check_output([pkg, "--libs", "opencv4"], text=True).split()
    cpp_src = r"""
#include <opencv2/imgcodecs.hpp>
#include <opencv2/imgproc.hpp>
#include <fstream>
#include <iostream>
int main(int argc, char** argv) {
    if (argc != 3) return 2;
    cv::Mat img = cv::imread(argv[1], cv::IMREAD_COLOR);
    if (img.empty()) return 3;
    const int W = 384, H = 288;
    double scale = std::min((double)W / img.cols, (double)H / img.rows);
    int nw = std::max(2, ((int)std::lround(img.cols * scale)) & ~1);
    int nh = std::max(2, ((int)std::lround(img.rows * scale)) & ~1);
    cv::Mat resized;
    cv::resize(img, resized, cv::Size(nw, nh), 0, 0, cv::INTER_AREA);
    cv::Mat canvas(H, W, CV_8UC3, cv::Scalar(128, 128, 128));
    int px = ((W - nw) / 2) & ~1;
    int py = ((H - nh) / 2) & ~1;
    resized.copyTo(canvas(cv::Rect(px, py, nw, nh)));
    cv::Mat i420;
    cv::cvtColor(canvas, i420, cv::COLOR_BGR2YUV_I420);
    std::ofstream out(argv[2], std::ios::binary);
    out.write(reinterpret_cast<const char*>(i420.data), i420.total() * i420.elemSize());
    return out.good() ? 0 : 4;
}
"""
    with tempfile.TemporaryDirectory(prefix="bake-i420-") as td:
        src_cpp = Path(td) / "conv.cpp"
        bin_path = Path(td) / "conv"
        src_cpp.write_text(cpp_src, encoding="utf-8")
        build = subprocess.run([cxx, "-O2", str(src_cpp), "-o", str(bin_path), *cflags, *libs], check=False)
        if build.returncode != 0:
            return False
        for src, dst in pairs:
            r = subprocess.run([str(bin_path), str(src), str(dst)], check=False)
            if r.returncode != 0 or not dst.is_file() or dst.stat().st_size != FRAME_BYTES:
                return False
    return True


def _try_macos_sips(src: Path, dst: Path) -> bool:
    if platform.system() != "Darwin" or not shutil.which("sips"):
        return False
    with tempfile.TemporaryDirectory(prefix="bake-sips-") as td:
        bmp = Path(td) / "img.bmp"
        r = subprocess.run(
            ["sips", "-s", "format", "bmp", str(src), "--out", str(bmp)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if r.returncode != 0 or not bmp.is_file():
            return False
        data = bmp.read_bytes()
        if data[:2] != b"BM":
            return False
        pixel_off = struct.unpack_from("<I", data, 10)[0]
        w = struct.unpack_from("<i", data, 18)[0]
        h = struct.unpack_from("<i", data, 22)[0]
        bpp = struct.unpack_from("<H", data, 28)[0]
        if bpp not in (24, 32) or w <= 0 or h == 0:
            return False
        top_down = h < 0
        ah = abs(h)
        step = bpp // 8
        row_stride = ((w * step + 3) // 4) * 4
        rgb = bytearray(w * ah * 3)
        for y in range(ah):
            src_y = y if top_down else (ah - 1 - y)
            base = pixel_off + src_y * row_stride
            dst_base = y * w * 3
            for x in range(w):
                p = base + x * step
                # BMP stores B, G, R (, A)
                rgb[dst_base + x * 3] = data[p + 2]
                rgb[dst_base + x * 3 + 1] = data[p + 1]
                rgb[dst_base + x * 3 + 2] = data[p]
        raw = _rgb24_to_i420_letterbox(bytes(rgb), w, ah)
        dst.write_bytes(raw)
        return True


def convert_all(cache_dir: Path) -> None:
    pairs = [
        (cache_dir / "raw" / "david1.jpg", cache_dir / "frames" / "face_a_1.i420"),
        (cache_dir / "raw" / "david2.jpg", cache_dir / "frames" / "face_a_2.i420"),
        (cache_dir / "raw" / "100032540_1.jpg", cache_dir / "frames" / "face_b_1.i420"),
        (cache_dir / "raw" / "100040721_1.jpg", cache_dir / "frames" / "face_c_1.i420"),
    ]
    (cache_dir / "frames").mkdir(parents=True, exist_ok=True)
    missing = [(s, d) for s, d in pairs if not (d.is_file() and d.stat().st_size == FRAME_BYTES)]
    if missing:
        ok = True
        for src, dst in missing:
            if not (_try_cv2(src, dst) or _try_ffmpeg(src, dst) or _try_pillow(src, dst) or _try_macos_sips(src, dst)):
                ok = False
                break
        if not ok:
            if not _try_opencv_cpp(missing):
                raise RuntimeError(
                    "Unable to convert JPEG faces to 384x288 I420: install ffmpeg, python3-opencv, "
                    "python3-pil, or g++ + libopencv-dev (or run on macOS with sips)."
                )

    two = cache_dir / "frames" / "two_faces.i420"
    if not (two.is_file() and two.stat().st_size == FRAME_BYTES):
        fa = (cache_dir / "frames" / "face_a_1.i420").read_bytes()
        fb = (cache_dir / "frames" / "face_b_1.i420").read_bytes()
        two.write_bytes(compose_two_faces_i420(fa, fb))


def main() -> int:
    if len(sys.argv) != 2:
        sys.stderr.write("usage: bake_i420.py <vision_cache_dir>\n")
        return 2
    convert_all(Path(sys.argv[1]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
