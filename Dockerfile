# trixie (Debian 13), not bookworm: fastembed/ort's downloaded prebuilt
# ONNX Runtime static libs reference glibc >= 2.38 symbols (e.g.
# `__isoc23_strtoll`) that bookworm's glibc 2.36 doesn't have, which fails
# at link time. The runtime stage below must match this glibc version too,
# since the final binary is linked against it.
FROM rust:1-slim-trixie AS builder
WORKDIR /app

# rusqlite's "bundled" feature compiles vendored SQLite (and sqlite-vec) from
# source, which needs a C toolchain — `rust:*-slim` doesn't ship one by
# default the way the full `rust:*` image does.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# No compile-time content to embed: unlike the old generic-catalog scaffold
# this project started as, an OKF vault is entirely runtime data (mounted
# in, not baked into the image), so a single release build is enough.
RUN cargo build --locked --release

# `fastembed`/`ort` may dynamically link an ONNX Runtime shared library
# rather than statically linking it — if `cargo build --release` above
# succeeds but the binary fails at runtime with a missing
# `libonnxruntime.so`, this runtime stage needs an explicit
# `COPY --from=builder` of that library (or a system package providing it).
FROM debian:trixie-slim AS runtime
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/okf-mcp ./okf-mcp
COPY --from=builder /app/target/release/okf-mcp-healthcheck ./okf-mcp-healthcheck

HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["./okf-mcp-healthcheck"]

ENTRYPOINT ["./okf-mcp"]
