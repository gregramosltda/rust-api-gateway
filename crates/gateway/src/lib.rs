//! API Gateway de alto throughput.
//!
//! Organiza-se em camadas com responsabilidades separadas:
//!
//! - [`config`] e [`net`] — tipos e utilitários sem dependência de I/O.
//! - [`state`] — o que é compartilhado entre requisições.
//! - [`proxy`] — falar com o upstream.
//! - [`pipeline`] — a ordem das camadas por requisição.
//! - [`routes`] — endpoints que não são proxy (saúde, métricas).
//! - [`dashboard`] — a interface visual servida na raiz.
//! - [`mock`] — o destino simulado sob `/mock/*`.

pub mod dashboard;
pub mod mock;
pub mod net;
pub mod pipeline;
pub mod proxy;
pub mod routes;
pub mod state;

use axum::routing::{any, get, post};
use axum::Router;

pub use gateway_common::Config;
pub use state::AppState;

/// Monta o router completo.
///
/// Rotas internas (`/health`, `/ready`, `/metrics`, o dashboard) são registradas
/// **antes** do fallback `/*caminho`. No Axum a precedência é por registro,
/// então o proxy curinga precisa vir por último — registrado antes, ele
/// capturaria `/health` e tentaria encaminhá-lo ao upstream.
///
/// O destino simulado (`/mock/*`) obedece à mesma regra, por um motivo mais
/// afiado: o proxy aponta para o próprio gateway (`upstream.url` termina em
/// `/mock`). Sem uma rota que cubra **todo** o prefixo `/mock`, um caminho não
/// previsto — `/qualquer-coisa`, por exemplo, que o proxy reescreve para
/// `/mock/qualquer-coisa` — cairia no curinga outra vez e seria encaminhado
/// para si mesmo, acrescentando `/mock` a cada volta. Não é um erro de
/// configuração: é uma única requisição que derruba o processo. A rota
/// `mock/{*resto}` existe para que o prefixo `/mock` seja sempre resolvido
/// localmente, e a recursão seja estruturalmente impossível.
pub fn construir_router(state: AppState) -> Router {
    let mock_ligado = state.config.mock.enabled;

    let mut router = Router::new()
        .route("/", get(dashboard::index))
        .route("/health", get(routes::health))
        .route("/ready", get(routes::ready))
        .route("/metrics", get(routes::metrics));

    if mock_ligado {
        router = router
            .route("/mock/health", get(mock::health))
            .route("/mock/api/cardapio", get(mock::cardapio))
            .route("/mock/api/pedido", post(mock::pedido))
            // Fecha o prefixo: ver a nota acima sobre recursão.
            .route("/mock/{*resto}", any(mock::nao_encontrado));
    }

    router
        .route("/{*caminho}", any(pipeline::proxy))
        .fallback(routes::nao_encontrado)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::build_client;
    use gateway_cache::ResponseCache;
    use gateway_common::Config;
    use gateway_rate_limiter::{ManualClock, MemoryBackend};
    use std::sync::Arc;
    use std::time::Duration;

    pub fn estado_de_teste() -> AppState {
        let config: Config = toml::from_str(
            r#"
[server]
bind_addr = "127.0.0.1:0"

[upstream]
url = "http://127.0.0.1:9999"
timeout_seconds = 5

[rate_limit]
enabled = true
redis_url = "redis://127.0.0.1:6379"
default_limit = 100
default_window_seconds = 60

[cache]
enabled = true
max_capacity = 100
ttl_seconds = 30
"#,
        )
        .expect("config de teste");

        let upstream =
            build_client(Duration::from_secs(1), Duration::from_secs(5)).expect("cliente de teste");
        let limiter = Arc::new(MemoryBackend::new(Arc::new(ManualClock::new(0))));
        let cache = ResponseCache::new(100, Duration::from_secs(30));

        AppState::new(config, upstream, limiter, cache, false)
    }

    #[test]
    fn router_constroi_sem_erro() {
        let _router = construir_router(estado_de_teste());
    }

    #[test]
    fn estado_nao_panica_ao_ser_depurado() {
        let texto = format!("{:?}", estado_de_teste());
        assert!(
            texto.contains("memoria"),
            "estado deve expor o backend de rate limit em uso: {texto}"
        );
    }

    #[test]
    fn upstream_base_remove_barra_final() {
        // `config` é `Arc`, então se constrói um estado novo com a URL alterada
        // em vez de tentar escrever por trás do `Arc`.
        let config: Config = toml::from_str(
            r#"
[upstream]
url = "http://exemplo:9000/"
"#,
        )
        .expect("config");
        let upstream =
            build_client(Duration::from_millis(100), Duration::from_millis(500)).expect("cliente");
        let limiter = Arc::new(MemoryBackend::new(Arc::new(ManualClock::new(0))));
        let cache = ResponseCache::new(10, Duration::from_secs(1));
        let estado = AppState::new(config, upstream, limiter, cache, false);
        assert_eq!(estado.upstream_base(), "http://exemplo:9000");
    }

    #[test]
    fn timeout_vem_da_configuracao() {
        let estado = estado_de_teste();
        assert_eq!(estado.upstream_timeout(), Duration::from_secs(5));
    }
}
