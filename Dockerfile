# syntax=docker/dockerfile:1
# Multi-stage build for polytrader (main app + Dioxus UI when enabled)

FROM rust:1.88-bookworm AS builder
# post-Phase 2 WASM hydration prep (smallest viable start after wiki/log update per 2026-05-25 "deploy + next phase" entry + fidelity amend + gaps: full client bundle + asset serving + server_fns; guarded || true + comments only so current docker-build/k8s-apply/hermes ts + poly flow 100% untouched + no Cargo/src changes yet; future: rustup + wasm32 + dx build + static serve in axum while preserving all prior verified behavior; see docs/project-plan.md:167 for Phase 3 gated real def).
# RUN rustup target add wasm32-unknown-unknown || true
WORKDIR /app

# System deps for sqlx / tls if needed
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
# Dummy main to cache deps
RUN mkdir -p src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release --locked || true

# Real source
COPY . .
RUN cargo build --release --locked --bin polytrader  # explicit bin (avoids hermes bin source in this image; hermes uses separate Dockerfile)

# Runtime image for polytrader (main app + dashboard)
FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 polytrader

COPY --from=builder /app/target/release/polytrader /app/polytrader

USER polytrader
EXPOSE 8080

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/app/polytrader"]
