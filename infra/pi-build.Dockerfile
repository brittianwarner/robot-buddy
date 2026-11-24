# syntax=docker/dockerfile:1
# Use an arm64 userspace (native or Buildx/QEMU), so clang sees exactly the
# OpenCV headers, C++ ABI and target libraries used by the linker.
FROM rust:1.100.0-slim-trixie@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS toolchain
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 && apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev libopencv-dev cmake pkg-config && rm -rf /var/lib/apt/lists/* \
    && pkg-config --atleast-version=4.8 opencv4
WORKDIR /src

FROM toolchain AS source
COPY Cargo.toml Cargo.lock ./
COPY .cargo/config.toml .cargo/config.toml
COPY apps/server/Cargo.toml apps/server/Cargo.toml
# Cargo validates every workspace member's targets even in a package-only build.
RUN mkdir -p apps/server/src && touch apps/server/src/main.rs
COPY apps/robot apps/robot
COPY crates/buddy crates/buddy

FROM source AS build
RUN --mount=type=cache,id=pi-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pi-cargo-target,target=/src/target,sharing=locked \
    cargo build --locked --release --target aarch64-unknown-linux-gnu -j 2 \
    --package robot-buddy-robot --features pi-vision,onnx-wake \
    && cp target/aarch64-unknown-linux-gnu/release/robot-buddy-robot /robot-buddy-robot

# Run these inside the arm64 target userspace, not on the workstation.
FROM source AS test
RUN --mount=type=cache,id=pi-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pi-cargo-target,target=/src/target,sharing=locked \
    cargo test --locked --all-features --target aarch64-unknown-linux-gnu -j 2 --package robot-buddy-robot

FROM scratch AS artifact
COPY --from=build /robot-buddy-robot /
