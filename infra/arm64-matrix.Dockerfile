# syntax=docker/dockerfile:1
# Reproducible Linux ARM64 workspace qualification matrix for Rust 1.100.0
# across both Debian Bookworm (private-prefix OpenCV 4.10.0) and Debian Trixie
# (packaged OpenCV 4.10). Run via `infra/run-arm64-matrix.sh` with bounded
# parallelism (-j2/-j3) to stay within a 16 GiB Docker memory budget.

FROM rust:1.100.0-slim-bookworm@sha256:bb875b0f8ce726053e259034848c471840e96bfe591fd52556178ae9a05e6126 AS bookworm-toolchain
ARG TARGETARCH
ARG OPENCV_BUILD_JOBS=2
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
      build-essential git cmake ninja-build clang libclang-dev pkg-config \
      libjpeg-dev zlib1g-dev ca-certificates file \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy \
    && git clone --depth 1 --branch 4.10.0 https://github.com/opencv/opencv.git /tmp/opencv-4.10.0 \
    && test "$(git -C /tmp/opencv-4.10.0 rev-parse HEAD)" = 71d3237a093b60a27601c20e9ee6c3e52154e8b1 \
    && cmake -S /tmp/opencv-4.10.0 -B /tmp/opencv-pi-build -G Ninja \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=/opt/robot-buddy/opencv-4.10 \
      -DOPENCV_LIB_INSTALL_PATH=lib \
      -DBUILD_LIST=core,imgproc,imgcodecs,dnn,objdetect \
      -DBUILD_SHARED_LIBS=ON \
      -DOPENCV_GENERATE_PKGCONFIG=ON \
      -DWITH_JPEG=ON \
      -DWITH_OPENCL=OFF \
      -DBUILD_TESTS=OFF \
      -DBUILD_PERF_TESTS=OFF \
      -DBUILD_EXAMPLES=OFF \
    && cmake --build /tmp/opencv-pi-build -j "$OPENCV_BUILD_JOBS" \
    && cmake --install /tmp/opencv-pi-build \
    && printf '%s\n' '/opt/robot-buddy/opencv-4.10/lib' > /etc/ld.so.conf.d/robot-buddy-opencv.conf \
    && ldconfig \
    && rm -rf /tmp/opencv-4.10.0 /tmp/opencv-pi-build
ENV PKG_CONFIG_PATH=/opt/robot-buddy/opencv-4.10/lib/pkgconfig
RUN test "$(pkg-config --modversion opencv4)" = 4.10.0 \
    && test "$(pkg-config --variable=prefix opencv4)" = /opt/robot-buddy/opencv-4.10
WORKDIR /src

FROM bookworm-toolchain AS bookworm-source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/config.toml .cargo/config.toml
COPY crates ./crates
COPY apps ./apps
COPY infra/arm64-matrix-check.sh /usr/local/bin/arm64-matrix-check
RUN chmod +x /usr/local/bin/arm64-matrix-check

FROM bookworm-source AS bookworm-deps
ARG CARGO_JOBS=2
RUN --mount=type=cache,id=arm64-matrix-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=arm64-matrix-bookworm-target,target=/src/target,sharing=locked \
    cargo check --workspace --all-targets --locked -j "$CARGO_JOBS" \
    && cargo check --workspace --all-targets --all-features --locked -j "$CARGO_JOBS"

FROM bookworm-source AS bookworm-gate
ARG CARGO_JOBS=2
ARG SOURCE_SHA=unknown
ARG EXPECTED_TREE_HASH=skip
RUN --mount=type=cache,id=arm64-matrix-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=arm64-matrix-bookworm-target,target=/src/target,sharing=locked \
    /usr/local/bin/arm64-matrix-check bookworm "$CARGO_JOBS" "$SOURCE_SHA" "$EXPECTED_TREE_HASH"

FROM rust:1.100.0-slim-trixie@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS trixie-toolchain
ARG TARGETARCH
RUN test "$TARGETARCH" = arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang libclang-dev libopencv-dev cmake pkg-config ca-certificates file \
    && rm -rf /var/lib/apt/lists/* \
    && pkg-config --atleast-version=4.8 opencv4 \
    && rustup component add rustfmt clippy
WORKDIR /src

FROM trixie-toolchain AS trixie-source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/config.toml .cargo/config.toml
COPY crates ./crates
COPY apps ./apps
COPY infra/arm64-matrix-check.sh /usr/local/bin/arm64-matrix-check
RUN chmod +x /usr/local/bin/arm64-matrix-check

FROM trixie-source AS trixie-deps
ARG CARGO_JOBS=2
RUN --mount=type=cache,id=arm64-matrix-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=arm64-matrix-trixie-target,target=/src/target,sharing=locked \
    cargo check --workspace --all-targets --locked -j "$CARGO_JOBS" \
    && cargo check --workspace --all-targets --all-features --locked -j "$CARGO_JOBS"

FROM trixie-source AS trixie-gate
ARG CARGO_JOBS=2
ARG SOURCE_SHA=unknown
ARG EXPECTED_TREE_HASH=skip
RUN --mount=type=cache,id=arm64-matrix-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=arm64-matrix-trixie-target,target=/src/target,sharing=locked \
    /usr/local/bin/arm64-matrix-check trixie "$CARGO_JOBS" "$SOURCE_SHA" "$EXPECTED_TREE_HASH"

