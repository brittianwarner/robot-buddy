#!/bin/sh
# Benchmark-only ExecStart wrapper for Variant B (warm-whisper).
# Starts whisper-server and robot-buddy-robot inside the SAME robot-buddy.service
# 1 GiB systemd cgroup so resident ggml memory is honestly counted in MemoryPeak.
# Use only with a temporary benchmark drop-in setting NotifyAccess=all and
# Environment=BUDDY_LISTEN_WHISPER=/usr/local/bin/pip-perf-whisper-shim.sh.
set -eu

WHISPER_SERVER="${PIP_PERF_WHISPER_SERVER:-/usr/local/bin/whisper-server}"
WHISPER_MODEL="${BUDDY_LISTEN_MODEL:-/opt/robot-buddy/models/ggml-tiny.en.bin}"
WHISPER_HOST="127.0.0.1"
WHISPER_PORT="${PIP_PERF_WHISPER_PORT:-8178}"
ROBOT_BIN="${PIP_PERF_ROBOT_BIN:-/usr/local/bin/robot-buddy-robot}"

whisper_pid=""
robot_pid=""

cleanup() {
  if [ -n "$robot_pid" ]; then
    kill -TERM "$robot_pid" 2>/dev/null || true
    wait "$robot_pid" 2>/dev/null || true
  fi
  if [ -n "$whisper_pid" ]; then
    kill -TERM "$whisper_pid" 2>/dev/null || true
    wait "$whisper_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

"$WHISPER_SERVER" \
  -m "$WHISPER_MODEL" \
  -l en \
  -t 2 \
  --host "$WHISPER_HOST" \
  --port "$WHISPER_PORT" \
  >/dev/null 2>&1 &
whisper_pid=$!

# Wait briefly for whisper-server to stay alive and bind loopback before starting the robot.
ready=0
i=0
while [ "$i" -lt 30 ]; do
  if ! kill -0 "$whisper_pid" 2>/dev/null; then
    printf 'pip-perf-warm-whisper-exec: whisper-server exited early\n' >&2
    exit 1
  fi
  if curl --silent --max-time 1 --noproxy '*' "http://${WHISPER_HOST}:${WHISPER_PORT}/" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.5
  i=$((i + 1))
done
if [ "$ready" -ne 1 ]; then
  printf 'pip-perf-warm-whisper-exec: whisper-server did not answer on %s:%s\n' "$WHISPER_HOST" "$WHISPER_PORT" >&2
  exit 1
fi

"$ROBOT_BIN" "$@" &
robot_pid=$!
wait "$robot_pid"
rc=$?
robot_pid=""
exit "$rc"
