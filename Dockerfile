# Build context is the workspace root: lulan-api path-depends on the other
# crates, so all of crates/ is needed to compile — but only the API binary
# ships. packages/ and apps/ are excluded via .dockerignore.

FROM rust:1.96-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release -p lulan-api \
    && cp target/release/lulan-api /usr/local/bin/lulan-api

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 lulan
COPY --from=builder /usr/local/bin/lulan-api /usr/local/bin/lulan-api
USER lulan
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s \
    CMD curl -fsS http://localhost:8080/health/live || exit 1
ENTRYPOINT ["lulan-api"]
