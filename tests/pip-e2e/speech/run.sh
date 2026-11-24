#!/usr/bin/env bash
# Repeatable host entrypoint for ARM64 Pip speech qualification.
#
# Usage:
#   tests/pip-e2e/speech/run.sh [--skip-build] [--rust-tests]
#
# Optional environment variables (all read-only mounted, never copied into repo):
#   PIP_E2E_HEY_PIP_ONNX            Path to private hey_pip.onnx
#                                   (default: /Users/brittianwarner/scratch/butty/brain/models/wake/hey_pip.onnx)
#   PIP_E2E_HUMAN_SPEECH_DIR        Optional dir with wake_positive.wav, wake_negative.wav,
#                                   followup.wav, reference_clip.pcm, reference_scores.txt
#   PIP_E2E_HUMAN_WAKE_POSITIVE_WAV Optional 16kHz mono S16_LE WAV of human saying "Hey Pip"
#   PIP_E2E_HUMAN_WAKE_NEGATIVE_WAV Optional 16kHz mono S16_LE WAV of human non-wake speech
#   PIP_E2E_HUMAN_FOLLOWUP_WAV      Optional 16kHz mono S16_LE WAV of human follow-up speech
#   PIP_E2E_HUMAN_WAKE_PCM          Optional raw PCM for compare_reference_scores
#   PIP_E2E_HUMAN_WAKE_SCORES       Optional reference scores for compare_reference_scores

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd)"
IMAGE_TAG="robot-buddy-pip-speech:arm64"
SKIP_BUILD=0
RUN_RUST_TESTS=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --rust-tests)
      RUN_RUST_TESTS=1
      shift
      ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

CACHE_HOST="${REPO_ROOT}/tests/pip-e2e/.cache"
ARTIFACTS_HOST="${REPO_ROOT}/tests/pip-e2e/.artifacts"
mkdir -p "${CACHE_HOST}" "${ARTIFACTS_HOST}"

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "[speech-run] building ARM64 speech container (${IMAGE_TAG})..."
  docker buildx build \
    --platform linux/arm64 \
    -f "${SCRIPT_DIR}/Dockerfile" \
    --load \
    -t "${IMAGE_TAG}" \
    "${REPO_ROOT}"
fi

DOCKER_ARGS=(
  run --rm --platform linux/arm64
  -v "${REPO_ROOT}:/workspace:ro"
  -v "${CACHE_HOST}:/opt/pip-e2e/cache"
  -v "${ARTIFACTS_HOST}:/opt/pip-e2e/artifacts"
)

HEY_PIP_HOST="${PIP_E2E_HEY_PIP_ONNX:-/Users/brittianwarner/scratch/butty/brain/models/wake/hey_pip.onnx}"
if [[ -f "${HEY_PIP_HOST}" ]]; then
  echo "[speech-run] mounting private hey_pip.onnx read-only from ${HEY_PIP_HOST}"
  DOCKER_ARGS+=(
    -v "${HEY_PIP_HOST}:/opt/pip-e2e/private/hey_pip.onnx:ro"
    -e "PIP_E2E_HEY_PIP_ONNX=/opt/pip-e2e/private/hey_pip.onnx"
  )
else
  echo "[speech-run] NOTE: private hey_pip.onnx not found at ${HEY_PIP_HOST}; neural wake tests will report missing_evidence."
fi

if [[ -n "${PIP_E2E_HUMAN_SPEECH_DIR:-}" && -d "${PIP_E2E_HUMAN_SPEECH_DIR}" ]]; then
  DOCKER_ARGS+=(
    -v "${PIP_E2E_HUMAN_SPEECH_DIR}:/opt/pip-e2e/private/human-speech:ro"
    -e "PIP_E2E_HUMAN_SPEECH_DIR=/opt/pip-e2e/private/human-speech"
  )
fi

mount_opt_in_file() {
  local env_name="$1"
  local container_name="$2"
  local host_val="${!env_name:-}"
  if [[ -n "$host_val" && -f "$host_val" ]]; then
    DOCKER_ARGS+=(
      -v "${host_val}:/opt/pip-e2e/private/${container_name}:ro"
      -e "${env_name}=/opt/pip-e2e/private/${container_name}"
    )
  fi
}

mount_opt_in_file "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV" "wake_positive.wav"
mount_opt_in_file "PIP_E2E_HUMAN_WAKE_NEGATIVE_WAV" "wake_negative.wav"
mount_opt_in_file "PIP_E2E_HUMAN_FOLLOWUP_WAV" "followup.wav"
mount_opt_in_file "PIP_E2E_HUMAN_WAKE_PCM" "reference_clip.pcm"
mount_opt_in_file "PIP_E2E_HUMAN_WAKE_SCORES" "reference_scores.txt"

echo "[speech-run] executing ARM64 speech qualification suite..."
docker "${DOCKER_ARGS[@]}" "${IMAGE_TAG}" \
  python3 /workspace/tests/pip-e2e/speech/run-speech-qualification.py

if [[ "$RUN_RUST_TESTS" -eq 1 ]]; then
  echo "[speech-run] executing Rust ARM64 robot listen:: unit tests (pi-vision,onnx-wake)..."
  docker buildx build \
    --platform linux/arm64 \
    --target test \
    -f "${REPO_ROOT}/infra/pi-build.Dockerfile" \
    "${REPO_ROOT}"
fi
