# syntax=docker/dockerfile:1
# Standalone ARM64 Linux speech qualification container for Pip release QA.
#
# Matches the pinned Debian Trixie ARM64 base used by tests/pip-e2e/Dockerfile and
# infra/pi-build.Dockerfile. Builds REAL ARM64 whisper-cli (whisper.cpp v1.9.6),
# installs official Microsoft ONNX Runtime 1.20.1 (ORT_API_VERSION=20), and
# compiles ort-wake-probe.
#
# Private weights (hey_pip.onnx) and private human recordings are NEVER copied
# into this image; mount them read-only at runtime.

FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0 AS build-speech
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
       build-essential \
       ca-certificates \
       cmake \
       curl \
       python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src/speech
COPY tests/pip-e2e/speech/assets.lock.json \
     tests/pip-e2e/speech/ort_wake_probe.c \
     tests/pip-e2e/speech/install-speech-stack.sh ./

RUN --mount=type=cache,id=pip-speech-src-cache,target=/opt/pip-e2e/cache,sharing=locked \
    chmod +x ./install-speech-stack.sh \
    && ./install-speech-stack.sh --prefix /opt/pip-e2e/speech --cache /opt/pip-e2e/cache --build-only

FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0 AS runtime
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
       ca-certificates \
       curl \
       espeak-ng \
       file \
       libgomp1 \
       libstdc++6 \
       procps \
       python3 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/pip-e2e/speech /opt/pip-e2e/cache /opt/pip-e2e/artifacts /opt/pip-e2e/private /workspace

COPY --from=build-speech /opt/pip-e2e/speech /opt/pip-e2e/speech

# Verify native ARM64 binaries and shared library resolution inside Trixie ARM64.
RUN ! ldd /opt/pip-e2e/speech/bin/whisper-cli | grep -F "not found" \
    && ! ldd /opt/pip-e2e/speech/lib/libonnxruntime.so | grep -F "not found" \
    && ! ldd /opt/pip-e2e/speech/bin/ort-wake-probe | grep -F "not found" \
    && file /opt/pip-e2e/speech/bin/whisper-cli | grep -F "ARM aarch64" \
    && file /opt/pip-e2e/speech/lib/libonnxruntime.so | grep -F "ARM aarch64"

WORKDIR /workspace
ENV BUDDY_LISTEN_WHISPER=/opt/pip-e2e/speech/bin/whisper-cli \
    ORT_DYLIB_PATH=/opt/pip-e2e/speech/lib/libonnxruntime.so \
    PIP_E2E_SPEECH_DIR=/opt/pip-e2e/speech \
    PIP_E2E_CACHE_DIR=/opt/pip-e2e/cache \
    PIP_E2E_ARTIFACTS_DIR=/opt/pip-e2e/artifacts

CMD ["python3", "tests/pip-e2e/speech/run-speech-qualification.py"]
