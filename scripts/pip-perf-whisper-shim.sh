#!/bin/sh
# Benchmark-only shim for Variant 2 (warm resident whisper-server).
# Implements the exact stdin-WAV / stdout-text contract invoked by
# apps/robot/src/listen/io.rs:
#   <whisper> -m <model> -l en -t 2 -np -nt -f -
#
# Because listen/io.rs clears child env (retaining PATH and HOME), the loopback
# endpoint defaults to http://127.0.0.1:8178/inference and may be overridden via
# a one-line loopback URL in "$HOME/.pip-perf-whisper-url" (for tests/bench).
# Never logs audio or transcript text.
set -eu
umask 077

url="http://127.0.0.1:8178/inference"
if [ -n "${HOME:-}" ] && [ -r "${HOME}/.pip-perf-whisper-url" ]; then
  url="$(head -n 1 "${HOME}/.pip-perf-whisper-url" | tr -d '\r\n')"
fi

case "$url" in
  http://127.0.0.1:*/*|http://\[::1\]:*/*) ;;
  *)
    printf 'pip-perf-whisper-shim: loopback URL required\n' >&2
    exit 2
    ;;
esac

tmp="$(mktemp "${TMPDIR:-/tmp}/pip-perf-wav.XXXXXX")"
trap 'rm -f "$tmp"' EXIT INT TERM
cat > "$tmp"

curl \
  --silent \
  --show-error \
  --fail \
  --max-time 25 \
  --noproxy '*' \
  -F "file=@${tmp};type=audio/wav" \
  -F "response_format=text" \
  "$url"
