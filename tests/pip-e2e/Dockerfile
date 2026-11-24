# syntax=docker/dockerfile:1
# ARM64 Linux E2E qualification image for Pip release QA.
#
# Runs the REAL release binaries inside a native linux/arm64 Debian Trixie userspace:
#   - /usr/local/bin/rivet-engine        (pinned rivetdev/engine:3.1.1)
#   - /usr/local/bin/robot-buddy-server  (release build, shared cache robot-buddy-*)
#   - /usr/local/bin/robot-buddy-robot   (release --features pi-vision,onnx-wake, shared cache pi-cargo-*)
#   - /usr/local/bin/bun                 (official bun v1.4.4 linux-aarch64, SHA-256 verified)
#
# All Cargo builds use -j 2 and share BuildKit cache IDs with Dockerfile and
# infra/pi-build.Dockerfile to stay well within a 16 GiB Docker VM.
# Private weights (e.g. hey_pip.onnx) are NEVER copied into this image; mount read-only at runtime.

FROM rivetdev/engine:3.1.1@sha256:55cdfe9a92c0ee2f7d3ce5f388dab2ef8e0be1bab8e3f1adb7ba972f862e25d4 AS engine

# 1. Build robot-buddy-server (reuses root Dockerfile's Bookworm builder + BuildKit caches).
#    Bookworm glibc 2.36 binaries are forward-compatible with Debian Trixie glibc 2.41 runtime.
FROM rust:1.100.0-slim-bookworm@sha256:bb875b0f8ce726053e259034848c471840e96bfe591fd52556178ae9a05e6126 AS build-server
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN --mount=type=cache,id=robot-buddy-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=robot-buddy-target,target=/build/target,sharing=locked \
    cargo build --locked --release -j 2 --package robot-buddy-server \
    && cp /build/target/release/robot-buddy-server /robot-buddy-server

# 2. Build robot-buddy-robot with full Pip features on Trixie ARM64 (reuses infra/pi-build.Dockerfile caches).
FROM rust:1.100.0-slim-trixie@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS build-robot
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
       clang libclang-dev libopencv-dev cmake pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && pkg-config --atleast-version=4.8 opencv4
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo/config.toml .cargo/config.toml
COPY apps/server/Cargo.toml apps/server/Cargo.toml
RUN mkdir -p apps/server/src && touch apps/server/src/main.rs
COPY apps/robot apps/robot
COPY crates/buddy crates/buddy
RUN --mount=type=cache,id=pi-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pi-cargo-target,target=/src/target,sharing=locked \
    cargo build --locked --release --target aarch64-unknown-linux-gnu -j 2 \
      --package robot-buddy-robot --features pi-vision,onnx-wake \
    && cp target/aarch64-unknown-linux-gnu/release/robot-buddy-robot /robot-buddy-robot

# 3. Fetch official Bun 1.4.4 linux-aarch64 binary with SHA-256 verification.
FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0 AS fetch-bun
ARG BUN_VERSION=1.4.4
ARG BUN_AARCH64_SHA256=baa6bb36f51d6e58561e02bbea825038a9df454f673d6d776e8bbf86f8c7850e
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-aarch64.zip" -o /tmp/bun.zip \
    && echo "${BUN_AARCH64_SHA256}  /tmp/bun.zip" | sha256sum -c - \
    && unzip -q /tmp/bun.zip -d /tmp/bun-unpack \
    && install -m 0755 /tmp/bun-unpack/bun-linux-aarch64/bun /usr/local/bin/bun \
    && rm -rf /tmp/bun.zip /tmp/bun-unpack

# 3b. Build real ARM64 whisper-cli (whisper.cpp v1.9.6) + install official ONNX Runtime 1.20.1
#     via tests/pip-e2e/speech/install-speech-stack.sh (SHA-256 verified, shared BuildKit cache).
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

# 4. Runtime image for ARM64 E2E + soak execution.
FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0 AS runtime
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
       ca-certificates \
       curl \
       espeak-ng \
       file \
       openssl \
       tini \
       python3 \
       socat \
       procps \
       iproute2 \
       libgomp1 \
       libstdc++6 \
       libopencv-core410 \
       libopencv-imgproc410 \
       libopencv-imgcodecs410 \
       libopencv-dnn410 \
       libopencv-objdetect410 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system pip-control \
    && useradd --system --user-group --home-dir /var/lib/robot-buddy --create-home --shell /usr/sbin/nologin robot-buddy \
    && usermod -aG pip-control robot-buddy \
    && mkdir -p /opt/pip-e2e/cache /opt/pip-e2e/artifacts /opt/pip-e2e/private /opt/pip-e2e/speech /workspace

COPY --from=engine /usr/bin/rivet-engine /usr/local/bin/rivet-engine
COPY --from=build-server /robot-buddy-server /usr/local/bin/robot-buddy-server
COPY --from=build-robot /robot-buddy-robot /usr/local/bin/robot-buddy-robot
COPY --from=fetch-bun /usr/local/bin/bun /usr/local/bin/bun
COPY --from=build-speech /opt/pip-e2e/speech /opt/pip-e2e/speech

# Verify every shared library resolves in the target Trixie ARM64 userspace.
RUN ! ldd /usr/local/bin/robot-buddy-robot | grep -F "not found" \
    && ! ldd /usr/local/bin/robot-buddy-server | grep -F "not found" \
    && ! ldd /opt/pip-e2e/speech/bin/whisper-cli | grep -F "not found" \
    && ! ldd /opt/pip-e2e/speech/lib/libonnxruntime.so | grep -F "not found" \
    && /usr/local/bin/bun --version \
    && /usr/local/bin/rivet-engine --version

WORKDIR /workspace
ENV BUDDY_ENGINE_BINARY=/usr/local/bin/rivet-engine \
    PIP_E2E_SERVER_BIN=/usr/local/bin/robot-buddy-server \
    PIP_E2E_ROBOT_BIN=/usr/local/bin/robot-buddy-robot \
    PIP_E2E_CACHE_DIR=/opt/pip-e2e/cache \
    PIP_E2E_ARTIFACTS_DIR=/opt/pip-e2e/artifacts \
    PIP_E2E_SPEECH_DIR=/opt/pip-e2e/speech \
    ORT_DYLIB_PATH=/opt/pip-e2e/speech/lib/libonnxruntime.so

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["bun", "tests/pip-e2e/runner.mjs"]
