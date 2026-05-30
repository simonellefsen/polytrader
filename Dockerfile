# syntax=docker/dockerfile:1
# Multi-stage build for polytrader (main app + Dioxus UI when enabled)

FROM rust:1.91-bookworm AS builder
# post-Phase 2 WASM hydration prep (smallest viable start after wiki/log update per 2026-05-25 "deploy + next phase" entry + fidelity amend + gaps: full client bundle + asset serving + server_fns; guarded || true + comments only so current docker-build/k8s-apply/hermes ts + poly flow 100% untouched + no Cargo/src changes yet; future: rustup + wasm32 + dx build + static serve in axum while preserving all prior verified behavior; see docs/project-plan.md:167 for Phase 3 gated real def).
# RUN rustup target add wasm32-unknown-unknown || true
WORKDIR /app

# System deps for sqlx / tls if needed
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates build-essential cmake clang \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
# Dummy main to cache deps
RUN mkdir -p src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release --locked --features native-l2 --bin polytrader || true

# Real source
COPY . .
# The dependency-cache build above creates a dummy src/main.rs binary. Docker/BuildKit
# can preserve source mtimes older than that artifact, so Cargo may otherwise ship the
# no-op binary and Kubernetes sees an immediate clean exit. Keep deps cached, but force
# this binary source newer than the cached artifact before building the real app.
RUN touch src/main.rs && cargo build --release --locked --features native-l2 --bin polytrader

# Runtime image for polytrader (main app + dashboard)
# Keep a slim glibc runtime with OpenSSL available for sqlx/reqwest and the native
# Polymarket SDK path.
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
