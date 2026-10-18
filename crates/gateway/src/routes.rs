//! Handlers de saúde e prontidão.
//!
//! `/health` e `/ready` respondem a perguntas diferentes, e confundi-las causa
//! o erro clássico de deploy:
//!
//! - `/health` — "o processo está vivo?" Só responde sobre si mesmo. Um gateway
//!   com o Redis fora do ar **deve** continuar saudável: está pronto para
//!   reconectar, e reiniciá-lo não conserta o Redis.
//! - `/ready` — "pode me mandar tráfego agora?" Verifica as dependências.
//!   Aqui o Redis fora do ar **deve** tirar o gateway de prontidão, para que o
//!   balanceador pare de enviar requisições que vão falhar.
//!
//! Misturar os dois faz o orquestrador reiniciar o gateway em loop durante uma
//! indisponibilidade do Redis — derrubando mais serviço do que protegendo.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use gateway_common::GatewayError;
use serde_json::json;

use crate::state::AppState;

/// `GET /health` — vivacidade. Não toca em dependência externa.
pub async fn health(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "servico": "rust-api-gateway",
        "rate_limit": {
            "habilitado": state.config.rate_limit.enabled,
            "backend": state.limiter.kind(),
        },
        "cache": {
            "habilitado": state.config.cache.enabled,
            "ttl_segundos": state.cache.ttl().as_secs(),
        },
        "upstream": state.upstream_base(),
        "requisicoes_atingidas": state.counter.get(),
    }))
    .into_response()
}

/// `GET /ready` — prontidão. Verifica o backend de rate limiting.
pub async fn ready(State(state): State<AppState>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut pronto = true;

    if state.config.rate_limit.enabled {
        match state.limiter.health().await {
            Ok(()) => {
                checks.insert(
                    "rate_limit".to_owned(),
                    json!({ "status": "ok", "backend": state.limiter.kind() }),
                );
            }
            Err(err) => {
                pronto = false;
                checks.insert(
                    "rate_limit".to_owned(),
                    json!({
                        "status": "erro",
                        "codigo": err.code(),
                        "backend": state.limiter.kind(),
                    }),
                );
            }
        }
    } else {
        checks.insert("rate_limit".to_owned(), json!({ "status": "desabilitado" }));
    }

    if state.config.cache.enabled {
        let stats = state.cache.stats().await;
        checks.insert(
            "cache".to_owned(),
            json!({
                "status": "ok",
                "entradas": stats.entries,
                "acertos": stats.hits,
                "erros": stats.misses,
                "razao_acerto": stats.hit_ratio,
            }),
        );
    } else {
        checks.insert("cache".to_owned(), json!({ "status": "desabilitado" }));
    }

    let status = if pronto {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let corpo = json!({
        "status": if pronto { "pronto" } else { "nao_pronto" },
        "verificacoes": checks,
    });

    (status, Json(corpo)).into_response()
}

/// `GET /metrics` — contadores em JSON.
///
/// JSON e não Prometheus: o formato Prometheus exige parser no consumidor, e o
/// objetivo aqui é legibilidade por humano e por `curl` em diagnóstico.
/// Trocar por Prometheus é trabalho de meia hora quando houver coletor.
pub async fn metrics(State(state): State<AppState>) -> Response {
    let cache = state.cache.stats().await;
    Json(json!({
        "requisicoes": state.counter.get(),
        "rate_limit": {
            "habilitado": state.config.rate_limit.enabled,
            "backend": state.limiter.kind(),
        },
        "cache": {
            "habilitado": state.config.cache.enabled,
            "entradas": cache.entries,
            "acertos": cache.hits,
            "erros": cache.misses,
            "invalidacoes": cache.invalidations,
            "razao_acerto": cache.hit_ratio,
            "ttl_segundos": cache.ttl_seconds,
        },
    }))
    .into_response()
}

/// `GET /` — informação do serviço, para quem chega na raiz sem saber o que é.
pub async fn index() -> Response {
    Json(json!({
        "servico": "rust-api-gateway",
        "descricao": "API gateway de alto throughput com rate limiting e cache",
        "endpoints": {
            "health": "GET /health",
            "ready": "GET /ready",
            "metrics": "GET /metrics",
            "proxy": "ANY /*caminho",
        },
    }))
    .into_response()
}

/// Erro 404 do router, em JSON.
pub async fn nao_encontrado() -> Response {
    crate::proxy::erro_para_resposta(&GatewayError::NoRoute("caminho".to_owned()))
}
