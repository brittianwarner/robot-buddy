#!/usr/bin/env bash
set -euo pipefail

# Reproducible Linux ARM64 qualification matrix runner for Debian Bookworm and
# Debian Trixie on pinned Rust 1.100.0 images. Runs targets sequentially with
# bounded Cargo parallelism (-j2 default) to stay within a 16 GiB Docker budget.
#
# Usage:
#   infra/run-arm64-matrix.sh [both|bookworm|trixie] [--deps-only]
#
# Environment:
#   CARGO_JOBS=2         Cargo job count inside each container (recommended: 2 or 3)
#   OPENCV_BUILD_JOBS=2  Ninja job count when building Bookworm OpenCV 4.10.0
#   ALLOW_DIRTY=0        Gate mode refuses dirty workspace sources by default; set 1 only for debugging

MODE="${1:-both}"
STAGE_SUFFIX="gate"
if [ "${2:-}" = "--deps-only" ]; then
  STAGE_SUFFIX="deps"
elif [ -n "${2:-}" ]; then
  printf 'Unknown argument: %s\n' "$2" >&2
  exit 2
fi

CARGO_JOBS="${CARGO_JOBS:-2}"
OPENCV_BUILD_JOBS="${OPENCV_BUILD_JOBS:-2}"
ALLOW_DIRTY="${ALLOW_DIRTY:-0}"

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

HEAD_SHA="$(git rev-parse HEAD)"
BRANCH="$(git branch --show-current || echo detached)"
WORKSPACE_DIRTY="$(git status --porcelain -- Cargo.toml Cargo.lock rust-toolchain.toml .cargo crates apps)"
ALL_DIRTY="$(git status --porcelain)"

if command -v sha256sum >/dev/null 2>&1; then
  HASH_CMD=(sha256sum)
else
  HASH_CMD=(shasum -a 256)
fi

EXPECTED_TREE_HASH="$(
  find Cargo.toml Cargo.lock rust-toolchain.toml .cargo crates apps -type f \
    | LC_ALL=C sort \
    | xargs "${HASH_CMD[@]}" \
    | "${HASH_CMD[@]}" \
    | awk '{print $1}'
)"

printf 'Repository:          %s\n' "$ROOT"
printf 'Branch:              %s\n' "$BRANCH"
printf 'HEAD SHA:            %s\n' "$HEAD_SHA"
printf 'Workspace tree hash: %s\n' "$EXPECTED_TREE_HASH"
if [ -n "$WORKSPACE_DIRTY" ]; then
  printf 'Workspace sources:   DIRTY\n%s\n' "$WORKSPACE_DIRTY"
  if [ "$STAGE_SUFFIX" = "gate" ] && [ "$ALLOW_DIRTY" != "1" ]; then
    printf 'ERROR: Refusing to run release gate with uncommitted workspace changes. Commit/clean first (or set ALLOW_DIRTY=1 for non-evidence debugging).\n' >&2
    exit 1
  fi
  SOURCE_SHA="${HEAD_SHA}-dirty"
else
  printf 'Workspace sources:   CLEAN (matches %s)\n' "$HEAD_SHA"
  SOURCE_SHA="$HEAD_SHA"
fi
if [ -n "$ALL_DIRTY" ] && [ -z "$WORKSPACE_DIRTY" ]; then
  printf 'Non-workspace status (excluded from build context):\n%s\n' "$ALL_DIRTY"
fi
printf 'Mode:                %s (%s), SOURCE_SHA=%s, CARGO_JOBS=%s, OPENCV_BUILD_JOBS=%s\n' \
  "$MODE" "$STAGE_SUFFIX" "$SOURCE_SHA" "$CARGO_JOBS" "$OPENCV_BUILD_JOBS"

run_os() {
  local os="$1"
  local target="${os}-${STAGE_SUFFIX}"
  printf '\n>>> Running docker buildx target=%s (linux/arm64, SOURCE_SHA=%s, TREE_HASH=%s)\n' \
    "$target" "$SOURCE_SHA" "$EXPECTED_TREE_HASH"
  if [ "$STAGE_SUFFIX" = "gate" ]; then
    docker buildx build \
      --platform linux/arm64 \
      --progress=plain \
      -f infra/arm64-matrix.Dockerfile \
      --target "$target" \
      --build-arg "CARGO_JOBS=$CARGO_JOBS" \
      --build-arg "OPENCV_BUILD_JOBS=$OPENCV_BUILD_JOBS" \
      --build-arg "SOURCE_SHA=$SOURCE_SHA" \
      --build-arg "EXPECTED_TREE_HASH=$EXPECTED_TREE_HASH" \
      --no-cache-filter "$target" \
      .
  else
    docker buildx build \
      --platform linux/arm64 \
      --progress=plain \
      -f infra/arm64-matrix.Dockerfile \
      --target "$target" \
      --build-arg "CARGO_JOBS=$CARGO_JOBS" \
      --build-arg "OPENCV_BUILD_JOBS=$OPENCV_BUILD_JOBS" \
      .
  fi
}

case "$MODE" in
  both)
    run_os trixie
    run_os bookworm
    ;;
  trixie)
    run_os trixie
    ;;
  bookworm)
    run_os bookworm
    ;;
  *)
    printf 'Usage: %s [both|bookworm|trixie] [--deps-only]\n' "$0" >&2
    exit 2
    ;;
esac
