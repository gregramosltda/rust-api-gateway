//! Tipos compartilhados entre os crates do gateway.
//!
//! Fica livre de dependência de framework HTTP de propósito: `common` é a
//! vocabulary do domínio (configuração, decisões, erros). O mapeamento para
//! HTTP, Redis e cache mora nos crates que realmente falam esses protocolos.

pub mod config;
pub mod error;
pub mod ratelimit;

pub use config::{
    CacheConfig, Config, ConfigError, RateLimitConfig, RouteRule, ServerConfig, UpstreamConfig,
};
pub use error::{ErrorBody, ErrorDetail, GatewayError};
pub use ratelimit::{
    budget_key, client_identity, RateLimitDecision, HEADER_LIMIT, HEADER_REMAINING, HEADER_RESET,
    HEADER_RETRY_AFTER,
};
