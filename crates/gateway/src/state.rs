//! Estado compartilhado entre as rotas.
//!
//! `Arc` + `ArcSwap` não é necessário aqui: a configuração é imutável depois
//! do arranque e o rate limiter é um trait object com interior mutabilidade.
//! O que fica compartilhado é o que muda — métricas — e aí o custo é de
//! atomicidade, não de lock.

use std::sync::Arc;
use std::time::Duration;

use gateway_cache::ResponseCache;
use gateway_common::Config;
use gateway_rate_limiter::RateLimiterBackend;

use crate::proxy::{RequestCounter, UpstreamClient};

/// Estado injetado em todas as requisições via extractor do Axum.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub upstream: Arc<UpstreamClient>,
    pub limiter: Arc<dyn RateLimiterBackend>,
    pub cache: Arc<ResponseCache>,
    pub counter: Arc<RequestCounter>,
    /// Confia em `X-Forwarded-For`. Só ligar atrás de proxy que reescreve o
    /// cabeçalho; senão qualquer cliente escolhe o próprio IP e burla o limite.
    pub confia_proxy: bool,
}

impl AppState {
    /// Monta o estado a partir da configuração.
    pub fn new(
        config: Config,
        upstream: UpstreamClient,
        limiter: Arc<dyn RateLimiterBackend>,
        cache: ResponseCache,
        confia_proxy: bool,
    ) -> Self {
        Self {
            config: Arc::new(config),
            upstream: Arc::new(upstream),
            limiter,
            cache: Arc::new(cache),
            counter: Arc::new(RequestCounter::default()),
            confia_proxy,
        }
    }

    /// URL base do upstream, sem barra final.
    pub fn upstream_base(&self) -> &str {
        self.config.upstream.url.trim_end_matches('/')
    }

    /// Tempo limite de espera do proxy.
    pub fn upstream_timeout(&self) -> Duration {
        Duration::from_secs(self.config.upstream.timeout_seconds)
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("upstream", &self.upstream_base())
            .field("limiter", &self.limiter.kind())
            .field("cache_enabled", &self.config.cache.enabled)
            .field("rate_limit_enabled", &self.config.rate_limit.enabled)
            .field("confia_proxy", &self.confia_proxy)
            .finish_non_exhaustive()
    }
}
