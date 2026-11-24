#!/usr/bin/env sh
# Run the ARM64 Linux peripheral fixture qualification suite inside the ARM64 E2E image.
# Uses --cap-add NET_ADMIN so 10.42.0.1/32 can be added to container-local loopback to
# verify non-empty Wi-Fi setup QR readback after soft-reset hello without fd close.
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)"
IMAGE="${PIP_E2E_IMAGE:-robot-buddy-pip-e2e:arm64}"

mkdir -p "$REPO_ROOT/tests/pip-e2e/.cache" "$REPO_ROOT/tests/pip-e2e/.artifacts"

# Ensure vision models & I420 frames are cached (idempotent, SHA-256 verified)
"$SCRIPT_DIR/fetch-vision-models.sh" "$REPO_ROOT/tests/pip-e2e/.cache/vision" >/dev/null

exec docker run --rm --platform linux/arm64 \
  --cap-add NET_ADMIN \
  -e PYTHONDONTWRITEBYTECODE=1 \
  -v "$REPO_ROOT:/workspace" \
  -v "$REPO_ROOT/tests/pip-e2e/.cache:/opt/pip-e2e/cache" \
  -v "$REPO_ROOT/tests/pip-e2e/.artifacts:/opt/pip-e2e/artifacts" \
  -w /workspace \
  "$IMAGE" \
  python3 /workspace/tests/pip-e2e/peripherals/test_peripherals.py
