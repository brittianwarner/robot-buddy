#!/usr/bin/env sh
# Fetch pinned public OpenCV Zoo vision models (YuNet + SFace) and official OpenCV
# Apache-2.0 test face portraits into a gitignored cache directory, verify every
# SHA-256 checksum, bake 384x288 packed I420 frames, and write manifest.json.
#
# Usage:
#   tests/pip-e2e/peripherals/fetch-vision-models.sh [cache_dir]
#   PIP_E2E_VISION_CACHE=/opt/pip-e2e/cache/vision tests/pip-e2e/peripherals/fetch-vision-models.sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)"
CACHE_DIR="${1:-${PIP_E2E_VISION_CACHE:-$REPO_ROOT/tests/pip-e2e/.cache/vision}}"

mkdir -p "$CACHE_DIR/models" "$CACHE_DIR/raw" "$CACHE_DIR/frames"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

fetch_verified() {
  url="$1"
  dest="$2"
  expected="$3"

  if [ -f "$dest" ]; then
    actual="$(sha256_of "$dest")"
    if [ "$actual" = "$expected" ]; then
      return 0
    fi
    rm -f "$dest"
  fi

  tmp="${dest}.tmp.$$"
  curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$tmp"
  actual="$(sha256_of "$tmp")"
  if [ "$actual" != "$expected" ]; then
    rm -f "$tmp"
    echo "SHA-256 mismatch for $dest: got $actual expected $expected" >&2
    exit 1
  fi
  mv "$tmp" "$dest"
}

ZOO_COMMIT="48b10fa505aeb51fbd127fc60a5c660040e823b7"
ZOO_BASE="https://media.githubusercontent.com/media/opencv/opencv_zoo/${ZOO_COMMIT}/models"

YUNET_SHA="8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4"
SFACE_SHA="0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79"

fetch_verified \
  "${ZOO_BASE}/face_detection_yunet/face_detection_yunet_2023mar.onnx" \
  "$CACHE_DIR/models/face_detection_yunet_2023mar.onnx" \
  "$YUNET_SHA"

fetch_verified \
  "${ZOO_BASE}/face_recognition_sface/face_recognition_sface_2021dec.onnx" \
  "$CACHE_DIR/models/face_recognition_sface_2021dec.onnx" \
  "$SFACE_SHA"

EXTRA_COMMIT="0fd5515efaa81195465b04fa6e791922de3068e2"
EXTRA_BASE="https://raw.githubusercontent.com/opencv/opencv_extra/${EXTRA_COMMIT}/testdata/cv/face"

fetch_verified \
  "${EXTRA_BASE}/david1.jpg" \
  "$CACHE_DIR/raw/david1.jpg" \
  "378293e79d6802032c9f3596d2a70ab41ea3f52fc6c8b2f8ef5971541f9b32b4"

fetch_verified \
  "${EXTRA_BASE}/david2.jpg" \
  "$CACHE_DIR/raw/david2.jpg" \
  "b39930fb7d57f4a14edc55d94ba1a002e9bffe323315b0423c223f7321a4e3cc"

fetch_verified \
  "${EXTRA_BASE}/100032540_1.jpg" \
  "$CACHE_DIR/raw/100032540_1.jpg" \
  "bfe7d1d9bef715bca3d99b43733fc4c3812f4c43b71b8de4157482590e274037"

fetch_verified \
  "${EXTRA_BASE}/100040721_1.jpg" \
  "$CACHE_DIR/raw/100040721_1.jpg" \
  "2400a116a262a719659fb770e74d22949b5a085c597d28917da9e92fa1ed7995"

python3 "$SCRIPT_DIR/bake_i420.py" "$CACHE_DIR"

# Expose top-level symlinks so callers checking $CACHE_DIR/<file> directly find them cleanly.
ln -sf "models/face_detection_yunet_2023mar.onnx" "$CACHE_DIR/face_detection_yunet_2023mar.onnx"
ln -sf "models/face_recognition_sface_2021dec.onnx" "$CACHE_DIR/face_recognition_sface_2021dec.onnx"
for f in face_a_1.i420 face_a_2.i420 face_b_1.i420 face_c_1.i420 two_faces.i420; do
  ln -sf "frames/$f" "$CACHE_DIR/$f"
done

python3 - "$CACHE_DIR" "$ZOO_COMMIT" "$EXTRA_COMMIT" <<'PY'
import hashlib, json, sys
from pathlib import Path

cache = Path(sys.argv[1]).resolve()
zoo_commit, extra_commit = sys.argv[2], sys.argv[3]

def sha256(p: Path) -> str:
    h = hashlib.sha256()
    h.update(p.read_bytes())
    return h.hexdigest()

yunet = cache / "models" / "face_detection_yunet_2023mar.onnx"
sface = cache / "models" / "face_recognition_sface_2021dec.onnx"
frames = {
    "face_a_1": cache / "frames" / "face_a_1.i420",
    "face_a_2": cache / "frames" / "face_a_2.i420",
    "face_b_1": cache / "frames" / "face_b_1.i420",
    "face_c_1": cache / "frames" / "face_c_1.i420",
    "two_faces": cache / "frames" / "two_faces.i420",
}

manifest = {
    "cache_dir": str(cache),
    "models": {
        "yunet": {
            "path": str(yunet),
            "sha256": sha256(yunet),
            "source": f"https://github.com/opencv/opencv_zoo/tree/{zoo_commit}/models/face_detection_yunet",
            "license": "MIT",
        },
        "sface": {
            "path": str(sface),
            "sha256": sha256(sface),
            "source": f"https://github.com/opencv/opencv_zoo/tree/{zoo_commit}/models/face_recognition_sface",
            "license": "Apache-2.0",
        },
    },
    "frames": {
        k: {
            "path": str(p),
            "bytes": p.stat().st_size,
            "sha256": sha256(p),
            "format": "i420_384x288",
            "source": f"https://github.com/opencv/opencv_extra/tree/{extra_commit}/testdata/cv/face",
            "license": "Apache-2.0",
        }
        for k, p in frames.items()
    },
}

(cache / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
print(json.dumps(manifest, indent=2))
PY
