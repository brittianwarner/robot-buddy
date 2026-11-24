# syntax=docker/dockerfile:1
FROM rivetdev/engine:3.1.1@sha256:55cdfe9a92c0ee2f7d3ce5f388dab2ef8e0be1bab8e3f1adb7ba972f862e25d4 AS engine
FROM rust:1.97.1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN --mount=type=cache,id=robot-buddy-registry,target=/usr/local/cargo/registry,sharing=locked --mount=type=cache,id=robot-buddy-target,target=/build/target,sharing=locked cargo build --locked --release --package robot-buddy-server && cp /build/target/release/robot-buddy-server /robot-buddy-server
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl tini && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home buddy && mkdir -p /data && chown buddy:buddy /data
COPY --from=engine /usr/bin/rivet-engine /usr/local/bin/rivet-engine
COPY --from=build /robot-buddy-server /usr/local/bin/robot-buddy-server
USER buddy
ENV BUDDY_ENGINE_BINARY=/usr/local/bin/rivet-engine BUDDY_DATA_DIR=/data RIVET_ENDPOINT=http://127.0.0.1:6420 RIVETKIT_ENGINE_SPAWN=never RIVET_NAMESPACE=default RIVET_POOL=default RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080 6420
HEALTHCHECK --interval=15s --timeout=5s --start-period=45s CMD curl -fsS http://127.0.0.1:8080/ready || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/robot-buddy-server"]
