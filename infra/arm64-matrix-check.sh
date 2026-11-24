#!/usr/bin/env bash
set -euo pipefail

OS_LABEL="${1:?usage: arm64-matrix-check <bookworm|trixie> [cargo_jobs] [source_sha] [expected_tree_hash]}"
JOBS="${2:-2}"
SOURCE_SHA="${3:-unknown}"
EXPECTED_TREE_HASH="${4:-skip}"

run_cmd() {
  local phase="$1"
  shift
  printf '\n>>> [%s][%s] %s\n' "$OS_LABEL" "$phase" "$*"
  "$@"
}

ACTUAL_TREE_HASH="$(
  find Cargo.toml Cargo.lock rust-toolchain.toml .cargo crates apps -type f \
    | LC_ALL=C sort \
    | xargs sha256sum \
    | sha256sum \
    | awk '{print $1}'
)"

printf '=== ARM64 Matrix Gate: %s ===\n' "$OS_LABEL"
printf 'SOURCE_SHA:         %s\n' "$SOURCE_SHA"
printf 'EXPECTED_TREE_HASH: %s\n' "$EXPECTED_TREE_HASH"
printf 'ACTUAL_TREE_HASH:   %s\n' "$ACTUAL_TREE_HASH"
if [ "$EXPECTED_TREE_HASH" != "skip" ] && [ "$ACTUAL_TREE_HASH" != "$EXPECTED_TREE_HASH" ]; then
  printf 'ERROR: Workspace source tree hash mismatch (expected %s, got %s)\n' \
    "$EXPECTED_TREE_HASH" "$ACTUAL_TREE_HASH" >&2
  exit 1
fi
grep -E '^(PRETTY_NAME|VERSION_CODENAME)=' /etc/os-release || true
uname -a
rustc --version
cargo --version
cargo clippy --version
rustfmt --version
printf 'opencv4 modversion: %s\n' "$(pkg-config --modversion opencv4)"
printf 'opencv4 prefix: %s\n' "$(pkg-config --variable=prefix opencv4)"
printf 'CARGO_JOBS: %s\n' "$JOBS"

# [FMT] Formatting gate
run_cmd FMT cargo fmt --all -- --check

# [DEFAULT] Default workspace profile (lint/test + robot shared-crate no-default-features)
run_cmd DEFAULT cargo clippy --workspace --all-targets --locked -j "$JOBS" -- -D warnings
run_cmd DEFAULT cargo test --workspace --locked -j "$JOBS"
run_cmd DEFAULT cargo test -p robot-buddy --no-default-features --locked -j "$JOBS"

# [FULL] Full Pip features profile (workspace all-features clippy/tests + full-feature release binary)
run_cmd FULL cargo clippy --workspace --all-targets --all-features --locked -j "$JOBS" -- -D warnings
run_cmd FULL cargo test --workspace --all-features --locked -j "$JOBS"
run_cmd FULL cargo build --locked --release -j "$JOBS" --package robot-buddy-robot --features pi-vision,onnx-wake

# [ARTIFACT] Verify AArch64 ELF, zero missing dynamic libraries, OpenCV linkage, and unprivileged setup-card
BIN="target/release/robot-buddy-robot"
printf '\n>>> [%s][ARTIFACT] file %s\n' "$OS_LABEL" "$BIN"
FILE_OUT="$(file "$BIN")"
printf '%s\n' "$FILE_OUT"
printf '%s\n' "$FILE_OUT" | grep -q 'ELF 64-bit.*ARM aarch64'
BIN_SHA256="$(sha256sum "$BIN" | awk '{print $1}')"
BIN_BUILD_ID="$(readelf -n "$BIN" | awk '/Build ID:/ {print $3}')"
test -n "$BIN_SHA256"
test -n "$BIN_BUILD_ID"
printf 'release ELF sha256:  %s\n' "$BIN_SHA256"
printf 'release ELF BuildID: %s\n' "$BIN_BUILD_ID"

printf '\n>>> [%s][ARTIFACT] ldd %s\n' "$OS_LABEL" "$BIN"
LDD_OUT="$(ldd "$BIN")"
printf '%s\n' "$LDD_OUT"
if printf '%s\n' "$LDD_OUT" | grep -qi 'not found'; then
  printf 'ERROR: %s has unresolved dynamic libraries\n' "$BIN" >&2
  exit 1
fi
OPENCV_LDD="$(printf '%s\n' "$LDD_OUT" | grep opencv || true)"
test -n "$OPENCV_LDD"
if [ "$OS_LABEL" = "bookworm" ]; then
  if printf '%s\n' "$OPENCV_LDD" | grep -qv '/opt/robot-buddy/opencv-4.10/lib/'; then
    printf 'ERROR: Bookworm robot binary linked OpenCV outside /opt/robot-buddy/opencv-4.10/lib/\n' >&2
    exit 1
  fi
fi

# Unprivileged smoke check for setup-card using BUDDY_SETUP_WIFI_FILE in a private non-root directory
NONROOT_UID="$(id -u nobody)"
NONROOT_GID="$(id -g nobody)"
test "$NONROOT_UID" -ne 0
SMOKE_DIR="$(mktemp -d)"
SMOKE_BIN="$(mktemp)"
trap 'rm -rf "$SMOKE_DIR" "$SMOKE_BIN"' EXIT
cp "$BIN" "$SMOKE_BIN"
chmod 0755 "$SMOKE_BIN"
chown "$NONROOT_UID:$NONROOT_GID" "$SMOKE_DIR"
chmod 0700 "$SMOKE_DIR"
CARD_FILE="$SMOKE_DIR/setup-wifi.json"
printf '\n>>> [%s][ARTIFACT] setpriv --reuid=%s --regid=%s --clear-groups env BUDDY_SETUP_WIFI_FILE=%s %s setup-card\n' \
  "$OS_LABEL" "$NONROOT_UID" "$NONROOT_GID" "$CARD_FILE" "$SMOKE_BIN"
CARD_OUT="$(setpriv --reuid="$NONROOT_UID" --regid="$NONROOT_GID" --clear-groups \
  env "BUDDY_SETUP_WIFI_FILE=$CARD_FILE" "$SMOKE_BIN" setup-card)"
test -n "$CARD_OUT"
test -f "$CARD_FILE"
test "$(stat -c '%u:%g:%a' "$CARD_FILE")" = "${NONROOT_UID}:${NONROOT_GID}:600"
printf 'setup-card smoke check: ok (uid=%s gid=%s mode=600)\n' "$NONROOT_UID" "$NONROOT_GID"

# [STRESS] Fixed 20x parallel stress battery (8 threads, zero retries) on the three targeted tests
STRESS_RUNS="${STRESS_RUNS:-20}"
STRESS_THREADS="${STRESS_THREADS:-8}"
printf '\n>>> [%s][STRESS] %sx parallel (%s threads) targeted stress battery\n' \
  "$OS_LABEL" "$STRESS_RUNS" "$STRESS_THREADS"
for i in $(seq 1 "$STRESS_RUNS"); do
  STRESS_OUT="$(cargo test --locked --all-features -p robot-buddy-robot -j "$JOBS" -- \
    --test-threads="$STRESS_THREADS" \
    face::tests::a_stalled_port_times_out_without_leaking_threads_and_reopens_raw \
    face::tests::writer_reopens_only_after_the_stall_deadline_and_reopen_delay \
    listen::tests::drop_log_is_rate_limited_and_emits_only_safe_metadata 2>&1)"
  if ! printf '%s\n' "$STRESS_OUT" | grep -q 'test result: ok\. 3 passed; 0 failed; 0 ignored'; then
    printf 'ERROR: [%s][STRESS] run %s/%s failed:\n%s\n' "$OS_LABEL" "$i" "$STRESS_RUNS" "$STRESS_OUT" >&2
    exit 1
  fi
done
printf '[%s][STRESS] %s/%s passed (3 passed; 0 failed per run, --test-threads=%s)\n' \
  "$OS_LABEL" "$STRESS_RUNS" "$STRESS_RUNS" "$STRESS_THREADS"

# [IGNORED] Inventory all ignored tests across workspace (--all-features)
run_cmd IGNORED cargo test --workspace --all-features --locked -j "$JOBS" -- --list --ignored

printf '\n=== ARM64 Matrix Gate PASSED: %s (SOURCE_SHA=%s TREE_HASH=%s ELF_SHA256=%s BUILD_ID=%s) ===\n' \
  "$OS_LABEL" "$SOURCE_SHA" "$ACTUAL_TREE_HASH" "$BIN_SHA256" "$BIN_BUILD_ID"
