# =============================================================================
# Build multi-stage.
#
# A imagem final não contém nem o toolchain Rust nem o código-fonte. Isso não é
# estética: uma imagem de runtime com toolchain e sources inclui o código que o
# contêiner deveria proteger, e multiplica o tamanho da superfície de ataque.
# =============================================================================

# --- Estágio de build --------------------------------------------------------
FROM rust:1.99-slim AS build

WORKDIR /build

# Dependências em camada própria: só recompilam quando o manifesto muda, não a
# cada alteração de código-fonte. Num build de CI, isso corta minutos.
COPY Cargo.toml Cargo.lock ./
COPY crates/common/Cargo.toml crates/common/
COPY crates/rate-limiter/Cargo.toml crates/rate-limiter/
COPY crates/cache/Cargo.toml crates/cache/
COPY crates/gateway/Cargo.toml crates/gateway/

# Artefatos vazios para o Cargo resolver o grafo sem compilar de fato.
RUN mkdir -p crates/common/src crates/rate-limiter/src crates/cache/src crates/gateway/src \
    && echo "" > crates/common/src/lib.rs \
    && echo "fn main() {}" > crates/gateway/src/main.rs \
    && echo "" > crates/rate-limiter/src/lib.rs \
    && echo "" > crates/cache/src/lib.rs \
    && cargo build --release --locked 2>/dev/null || true \
    && rm -rf crates/*/src

COPY crates ./crates
COPY config ./config

# Build limpo de verdade. `--locked` garante que o Cargo.lock é respeitado: sem
# ele, o build poderia resolver versões diferentes das auditadas em CI.
RUN touch crates/common/src/lib.rs crates/rate-limiter/src/lib.rs \
             crates/cache/src/lib.rs crates/gateway/src/main.rs \
    && cargo build --release --locked --bin gateway

# --- Estágio de runtime ------------------------------------------------------
FROM debian:trixie-slim AS runtime

# `curl` serve ao healthcheck do compose; `ca-certificates` é necessário para
# qualquer chamada TLS de saída. `libssl3` é exigido pelo cliente rustls.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Usuário sem privilégios: um gateway exposto à internet não deve rodar como
# root, e isso não é cenário hipotético — é o estado normal de um serviço.
RUN groupadd --system --gid 1001 gateway \
    && useradd --system --uid 1001 --gid gateway --no-create-home --shell /usr/sbin/nologin gateway

COPY --from=build /build/target/release/gateway /usr/local/bin/gateway
COPY --from=build /build/config /app/config

WORKDIR /app
USER gateway

EXPOSE 8080

ENV RUST_LOG=gateway=info \
    GATEWAY_LOG_FORMAT=json

HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/gateway"]
CMD ["config/gateway.toml"]