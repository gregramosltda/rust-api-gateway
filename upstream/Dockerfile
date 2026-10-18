# =============================================================================
# Upstream falso para desenvolvimento e teste do docker-compose.
#
# Devolve um contador de requisições em JSON. O objetivo é tornar observável
# que o cache funciona: a requisição 2 em diante deve mostrar o mesmo total,
# porque o gateway deixou de encaminhá-la.
# =============================================================================

FROM rust:1.99-slim AS build
WORKDIR /build

RUN mkdir -p src \
    && cat > Cargo.toml <<'EOF'
[package]
name = "upstream-fake"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
EOF

COPY src ./src
RUN cargo build --release
RUN cp target/release/upstream-fake /usr/local/bin/

# -----------------------------------------------------------------------------

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /usr/local/bin/upstream-fake /usr/local/bin/upstream-fake

RUN useradd --system --uid 1002 --no-create-home --shell /usr/sbin/nologin upstream
USER upstream

EXPOSE 9000

ENV PORTA=9000

HEALTHCHECK --interval=10s --timeout=2s --retries=3 \
    CMD wget -qO- http://127.0.0.1:9000/health || exit 1

ENTRYPOINT ["/usr/local/bin/upstream-fake"]