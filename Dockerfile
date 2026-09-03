# LionFS 2.0 development container: build + test with the io_uring
# feature available (whether the host kernel allows the ring is
# probed at runtime; the engine degrades gracefully).
FROM rust:1-bookworm

# FUSE for mount experiments (tests don't need it).
RUN apt-get update \
    && apt-get install -y --no-install-recommends fuse3 libfuse3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Cache the dependency build in a layer.
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY tools tools
COPY benches benches
COPY userspace userspace
COPY tests tests
RUN cargo build --features io_uring && cargo test --features io_uring || true

# Incremental rebuilds mount the source over the cached layer.
CMD ["cargo", "test", "--features", "io_uring"]
