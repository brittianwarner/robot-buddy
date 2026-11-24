#!/usr/bin/env bash
# Reproducible ARM64 Linux E2E build, cache hydration, and execution wrapper.
#
# Examples:
#   tests/pip-e2e/run-in-docker.sh build
#   tests/pip-e2e/run-in-docker.sh prepare-cache
#   tests/pip-e2e/run-in-docker.sh run --suite smoke
#   tests/pip-e2e/run-in-docker.sh run --suite all
#   tests/pip-e2e/run-in-docker.sh run --suite soak --duration 30m
#
# Optional private read-only mounts (explicit env only; never copied, committed, or logged):
#   PIP_E2E_HEY_PIP_ONNX=/abs/path/to/hey_pip.onnx
#   PIP_E2E_HUMAN_SPEECH_DIR=/abs/path/to/human-speech-dir
#   PIP_E2E_HUMAN_WAKE_POSITIVE_WAV=/abs/path/to/wake_positive.wav
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE_TAG="${PIP_E2E_IMAGE:-robot-buddy-pip-e2e:arm64}"
CACHE_DIR="${PIP_E2E_HOST_CACHE:-$REPO_ROOT/tests/pip-e2e/.cache}"
ARTIFACTS_DIR="${PIP_E2E_HOST_ARTIFACTS:-$REPO_ROOT/tests/pip-e2e/.artifacts}"
HEY_PIP_HOST="${PIP_E2E_HEY_PIP_ONNX:-}"
HUMAN_SPEECH_HOST="${PIP_E2E_HUMAN_SPEECH_DIR:-}"
HUMAN_WAKE_WAV_HOST="${PIP_E2E_HUMAN_WAKE_POSITIVE_WAV:-}"

mkdir -p "$CACHE_DIR" "$ARTIFACTS_DIR"

cmd="${1:-run}"
shift || true

DATA_VOL=""
cleanup_vol() {
  if [[ -n "$DATA_VOL" ]]; then
    docker volume rm -f "$DATA_VOL" >/dev/null 2>&1 || true
  fi
}
trap cleanup_vol EXIT

build_image() {
  echo "[pip-e2e] building ARM64 image $IMAGE_TAG (shared caches, -j 2)..."
  docker buildx build \
    --platform linux/arm64 \
    -f "$REPO_ROOT/tests/pip-e2e/Dockerfile" \
    -t "$IMAGE_TAG" \
    --load \
    "$REPO_ROOT"
}

build_docker_run_args() {
  # Real ext4 Docker volume for BUDDY_DATA_DIR so Rivet 3.1.1's kernel-local log
  # directory lock works across server+engine restarts (matching compose.yaml).
  DATA_VOL="pip-e2e-data-$$-$(date +%s)"
  docker volume create "$DATA_VOL" >/dev/null

  RUN_ARGS=(
    --rm
    --platform linux/arm64
    -v "$DATA_VOL:/var/tmp/pip-e2e-data"
    -v "$REPO_ROOT:/workspace:ro"
    -v "$CACHE_DIR:/opt/pip-e2e/cache"
    -v "$ARTIFACTS_DIR:/opt/pip-e2e/artifacts"
    -e "PIP_E2E_DATA_ROOT=/var/tmp/pip-e2e-data"
    -e "PIP_E2E_CACHE_DIR=/opt/pip-e2e/cache"
    -e "PIP_E2E_SPEECH_DIR=/opt/pip-e2e/speech"
    -e "PIP_E2E_ARTIFACTS_DIR=/opt/pip-e2e/artifacts"
    -e "PIP_E2E_VISION_CACHE=/opt/pip-e2e/cache/vision"
  )
  if [[ -n "$HEY_PIP_HOST" && -f "$HEY_PIP_HOST" ]]; then
    RUN_ARGS+=(-v "$HEY_PIP_HOST:/opt/pip-e2e/private/hey_pip.onnx:ro")
    RUN_ARGS+=(-e "PIP_E2E_HEY_PIP_ONNX=/opt/pip-e2e/private/hey_pip.onnx")
  fi
  if [[ -n "$HUMAN_SPEECH_HOST" && -d "$HUMAN_SPEECH_HOST" ]]; then
    RUN_ARGS+=(-v "$HUMAN_SPEECH_HOST:/opt/pip-e2e/private/human-speech:ro")
    RUN_ARGS+=(-e "PIP_E2E_HUMAN_SPEECH_DIR=/opt/pip-e2e/private/human-speech")
  fi
  if [[ -n "$HUMAN_WAKE_WAV_HOST" && -f "$HUMAN_WAKE_WAV_HOST" ]]; then
    RUN_ARGS+=(-v "$HUMAN_WAKE_WAV_HOST:/opt/pip-e2e/private/wake_positive.wav:ro")
    RUN_ARGS+=(-e "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV=/opt/pip-e2e/private/wake_positive.wav")
  fi
}

case "$cmd" in
  build)
    build_image
    ;;
  prepare-cache)
    echo "[pip-e2e] hydrating official checksum-verified speech & vision caches..."
    if [[ -f "$REPO_ROOT/tests/pip-e2e/speech/install-speech-stack.sh" ]]; then
      docker run --rm --platform linux/arm64 \
        -v "$REPO_ROOT:/workspace:ro" \
        -v "$CACHE_DIR:/opt/pip-e2e/cache" \
        "$IMAGE_TAG" \
        bash /workspace/tests/pip-e2e/speech/install-speech-stack.sh \
          --prefix /opt/pip-e2e/speech \
          --cache /opt/pip-e2e/cache \
          --fetch-only
    else
      echo "[pip-e2e] warning: tests/pip-e2e/speech/install-speech-stack.sh not yet present" >&2
    fi
    if [[ -f "$REPO_ROOT/tests/pip-e2e/peripherals/fetch-vision-models.sh" ]]; then
      docker run --rm --platform linux/arm64 \
        -v "$REPO_ROOT:/workspace:ro" \
        -v "$CACHE_DIR:/opt/pip-e2e/cache" \
        -e "PIP_E2E_VISION_CACHE=/opt/pip-e2e/cache/vision" \
        "$IMAGE_TAG" \
        bash /workspace/tests/pip-e2e/peripherals/fetch-vision-models.sh
    else
      echo "[pip-e2e] warning: tests/pip-e2e/peripherals/fetch-vision-models.sh not yet present" >&2
    fi
    ;;
  run)
    build_docker_run_args
    docker run "${RUN_ARGS[@]}" "$IMAGE_TAG" \
      bun /workspace/tests/pip-e2e/runner.mjs "$@"
    ;;
  shell)
    build_docker_run_args
    docker run -it "${RUN_ARGS[@]}" "$IMAGE_TAG" bash "$@"
    ;;
  *)
    echo "Usage: $0 <build|prepare-cache|run [runner flags...]|shell>" >&2
    exit 2
    ;;
esac
