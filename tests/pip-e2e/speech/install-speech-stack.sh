#!/usr/bin/env bash
# Idempotent, checksum-verified installer for the ARM64 Pip speech stack.
#
# Installs:
#   <prefix>/bin/whisper-cli                    (ggml-org/whisper.cpp v1.9.6, static Release)
#   <prefix>/bin/ort-wake-probe                 (native C openWakeWord probe using ORT_API_VERSION=20)
#   <prefix>/lib/libonnxruntime.so              (Microsoft ONNX Runtime 1.20.1 linux-arm64)
#   <prefix>/lib/libonnxruntime_providers_shared.so
#   <cache>/speech/models/ggml-tiny.en.bin      (Whisper tiny.en English ggml model)
#   <cache>/speech/models/wake/melspectrogram.onnx (openWakeWord v0.5.1)
#   <cache>/speech/models/wake/embedding_model.onnx (openWakeWord v0.5.1)
#   <cache>/speech/fixtures/manifest.json       (verified human + clearly-labeled synthetic fixtures)
#
# Private weights (hey_pip.onnx) are NEVER downloaded, copied, or committed here;
# mount them read-only at /opt/pip-e2e/private/hey_pip.onnx (or PIP_E2E_HEY_PIP_ONNX).

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LOCKFILE="${SCRIPT_DIR}/assets.lock.json"

PREFIX="/opt/pip-e2e/speech"
CACHE="/opt/pip-e2e/cache"
MODE="all"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix)
      PREFIX="$2"
      shift 2
      ;;
    --cache)
      CACHE="$2"
      shift 2
      ;;
    --build-only)
      MODE="build"
      shift
      ;;
    --fetch-only)
      MODE="fetch"
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [--prefix /opt/pip-e2e/speech] [--cache /opt/pip-e2e/cache] [--build-only | --fetch-only]"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

lock_get() {
  python3 - "$LOCKFILE" "$@" <<'PY'
import json, sys
cur = json.load(open(sys.argv[1], "r", encoding="utf-8"))
for part in sys.argv[2:]:
    cur = cur[part]
print(cur)
PY
}

fetch_verified() {
  local url="$1"
  local expected_sha="$2"
  local dest="$3"
  mkdir -p "$(dirname "$dest")"
  if [[ -f "$dest" ]]; then
    local got
    got="$(sha256_file "$dest")"
    if [[ "$got" == "$expected_sha" ]]; then
      echo "[speech-install] cached ok: $dest ($got)"
      return 0
    fi
    echo "[speech-install] checksum mismatch on cached $dest (got $got), re-fetching..." >&2
    rm -f "$dest"
  fi
  local tmp="${dest}.tmp.$$"
  echo "[speech-install] fetching $url -> $dest"
  curl -fsSL --retry 3 --retry-delay 2 -o "$tmp" "$url"
  local got
  got="$(sha256_file "$tmp")"
  if [[ "$got" != "$expected_sha" ]]; then
    rm -f "$tmp"
    echo "[speech-install] ERROR: SHA-256 mismatch for $url" >&2
    echo "  expected: $expected_sha" >&2
    echo "  got:      $got" >&2
    exit 1
  fi
  mv "$tmp" "$dest"
  echo "[speech-install] verified ok: $dest ($got)"
}

install_runtime_binaries() {
  local arch
  arch="$(uname -m)"
  if [[ "$arch" != "aarch64" && "$arch" != "arm64" ]]; then
    echo "[speech-install] ERROR: expected aarch64/arm64 runtime, got $arch" >&2
    exit 1
  fi
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "[speech-install] ERROR: --build-only / runtime build must run inside Linux ARM64" >&2
    exit 1
  fi

  mkdir -p "${PREFIX}/bin" "${PREFIX}/lib" "${PREFIX}/include" "${CACHE}/speech/src"

  # 1. Official Microsoft ONNX Runtime 1.20.1 (linux-arm64) matching ort =2.0.0-rc.9 (ORT_API_VERSION=20)
  local ort_url ort_sha ort_nupkg libort_sha libprov_sha
  ort_url="$(lock_get runtime onnxruntime_nupkg url)"
  ort_sha="$(lock_get runtime onnxruntime_nupkg sha256)"
  ort_nupkg="${CACHE}/$(lock_get runtime onnxruntime_nupkg cache_relpath)"
  libort_sha="$(lock_get runtime onnxruntime_nupkg extracted libonnxruntime.so sha256)"
  libprov_sha="$(lock_get runtime onnxruntime_nupkg extracted libonnxruntime_providers_shared.so sha256)"

  local need_ort=1
  if [[ -f "${PREFIX}/lib/libonnxruntime.so" && -f "${PREFIX}/lib/libonnxruntime_providers_shared.so" && -f "${PREFIX}/include/onnxruntime_c_api.h" ]]; then
    if [[ "$(sha256_file "${PREFIX}/lib/libonnxruntime.so")" == "$libort_sha" && "$(sha256_file "${PREFIX}/lib/libonnxruntime_providers_shared.so")" == "$libprov_sha" ]]; then
      echo "[speech-install] ONNX Runtime 1.20.1 already installed at ${PREFIX}/lib/libonnxruntime.so"
      need_ort=0
    fi
  fi

  if [[ "$need_ort" -eq 1 ]]; then
    fetch_verified "$ort_url" "$ort_sha" "$ort_nupkg"
    python3 - "$ort_nupkg" "${PREFIX}" <<'PY'
import os, sys, zipfile
nupkg, prefix = sys.argv[1], sys.argv[2]
mapping = {
    "runtimes/linux-arm64/native/libonnxruntime.so": os.path.join(prefix, "lib", "libonnxruntime.so"),
    "runtimes/linux-arm64/native/libonnxruntime_providers_shared.so": os.path.join(prefix, "lib", "libonnxruntime_providers_shared.so"),
    "build/native/include/onnxruntime_c_api.h": os.path.join(prefix, "include", "onnxruntime_c_api.h"),
}
with zipfile.ZipFile(nupkg, "r") as zf:
    for member, dest in mapping.items():
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with zf.open(member) as src, open(dest, "wb") as dst:
            dst.write(src.read())
PY
    chmod 0755 "${PREFIX}/lib/libonnxruntime.so" "${PREFIX}/lib/libonnxruntime_providers_shared.so"
    chmod 0644 "${PREFIX}/include/onnxruntime_c_api.h"
    [[ "$(sha256_file "${PREFIX}/lib/libonnxruntime.so")" == "$libort_sha" ]] || { echo "libonnxruntime.so sha mismatch" >&2; exit 1; }
    [[ "$(sha256_file "${PREFIX}/lib/libonnxruntime_providers_shared.so")" == "$libprov_sha" ]] || { echo "libonnxruntime_providers_shared.so sha mismatch" >&2; exit 1; }
    echo "[speech-install] installed ONNX Runtime 1.20.1 -> ${PREFIX}/lib/libonnxruntime.so"
  fi

  # 2. Build native ARM64 ort-wake-probe against official onnxruntime_c_api.h
  if [[ ! -x "${PREFIX}/bin/ort-wake-probe" || "${SCRIPT_DIR}/ort_wake_probe.c" -nt "${PREFIX}/bin/ort-wake-probe" ]]; then
    echo "[speech-install] compiling ort-wake-probe (ORT_API_VERSION=20)"
    gcc -O2 -Wall -Wextra -Werror \
      -I"${PREFIX}/include" \
      "${SCRIPT_DIR}/ort_wake_probe.c" \
      -o "${PREFIX}/bin/ort-wake-probe" \
      -ldl -lm
    chmod 0755 "${PREFIX}/bin/ort-wake-probe"
  else
    echo "[speech-install] ort-wake-probe already built at ${PREFIX}/bin/ort-wake-probe"
  fi

  # 3. Build real ARM64 whisper-cli from pinned ggml-org/whisper.cpp v1.9.6
  if [[ ! -x "${PREFIX}/bin/whisper-cli" ]]; then
    local w_url w_sha w_tar
    w_url="$(lock_get runtime whisper_cpp url)"
    w_sha="$(lock_get runtime whisper_cpp sha256)"
    w_tar="${CACHE}/$(lock_get runtime whisper_cpp cache_relpath)"
    fetch_verified "$w_url" "$w_sha" "$w_tar"

    local workdir
    workdir="$(mktemp -d)"
    trap 'rm -rf "$workdir"' RETURN
    tar -xzf "$w_tar" -C "$workdir"
    local src_dir
    src_dir="$(find "$workdir" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
    echo "[speech-install] building whisper-cli from $src_dir (Release, static, -j 2)"
    cmake -S "$src_dir" -B "$workdir/build" \
      -DCMAKE_BUILD_TYPE=Release \
      -DBUILD_SHARED_LIBS=OFF \
      -DWHISPER_BUILD_EXAMPLES=ON \
      -DWHISPER_BUILD_TESTS=OFF \
      -DWHISPER_SDL2=OFF \
      -DGGML_NATIVE=OFF \
      -DGGML_OPENMP=ON
    cmake --build "$workdir/build" -j 2 --target whisper-cli
    install -m 0755 "$workdir/build/bin/whisper-cli" "${PREFIX}/bin/whisper-cli"
    rm -rf "$workdir"
    trap - RETURN
    echo "[speech-install] installed whisper-cli -> ${PREFIX}/bin/whisper-cli"
  else
    echo "[speech-install] whisper-cli already built at ${PREFIX}/bin/whisper-cli"
  fi

  # Verify dynamic dependencies resolve cleanly
  if command -v ldd >/dev/null 2>&1; then
    ! ldd "${PREFIX}/bin/whisper-cli" | grep -F "not found"
    ! ldd "${PREFIX}/lib/libonnxruntime.so" | grep -F "not found"
    ! ldd "${PREFIX}/bin/ort-wake-probe" | grep -F "not found"
  fi
}

fetch_models_and_fixtures() {
  mkdir -p "${CACHE}/speech/models/wake" "${CACHE}/speech/fixtures"

  fetch_verified \
    "$(lock_get models whisper_tiny_en url)" \
    "$(lock_get models whisper_tiny_en sha256)" \
    "${CACHE}/$(lock_get models whisper_tiny_en cache_relpath)"

  fetch_verified \
    "$(lock_get models openwakeword_melspectrogram url)" \
    "$(lock_get models openwakeword_melspectrogram sha256)" \
    "${CACHE}/$(lock_get models openwakeword_melspectrogram cache_relpath)"

  fetch_verified \
    "$(lock_get models openwakeword_embedding url)" \
    "$(lock_get models openwakeword_embedding sha256)" \
    "${CACHE}/$(lock_get models openwakeword_embedding cache_relpath)"

  # Ensure whisper.cpp tarball is available for extracting bundled public-domain samples/jfk.wav
  fetch_verified \
    "$(lock_get runtime whisper_cpp url)" \
    "$(lock_get runtime whisper_cpp sha256)" \
    "${CACHE}/$(lock_get runtime whisper_cpp cache_relpath)"

  python3 "${SCRIPT_DIR}/prepare-fixtures.py" --cache "${CACHE}" --lockfile "${LOCKFILE}"
}

case "$MODE" in
  build)
    install_runtime_binaries
    ;;
  fetch)
    fetch_models_and_fixtures
    ;;
  all)
    install_runtime_binaries
    fetch_models_and_fixtures
    ;;
esac

echo "[speech-install] done (mode=${MODE}, prefix=${PREFIX}, cache=${CACHE})"
