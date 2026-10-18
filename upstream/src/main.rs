//! Upstream falso para desenvolvimento.
//!
//! Devolve um contador de requisições em JSON, o que torna observável o
//! efeito do cache: requisições repetidas devem mostrar o mesmo total, porque
//! o gateway deixou de encaminhá-las.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;

#[derive(Clone, Default)]
struct Contador(Arc<AtomicU64>);

// Retorno concreto: `impl IntoResponse` não satisfaz a trait `Handler` do
// Axum 0.8, e a mensagem de erro não identifica qual handler falhou.
async fn dados(axum::Extension(c): axum::Extension<Contador>) -> axum::response::Response {
    let n = c.0.fetch_add(1, Ordering::SeqCst) + 1;
    axum::Json(serde_json::json!({
        "servico": "upstream-fake",
        "requisicoes": n,
    }))
        .into_response()
}

async fn health() -> axum::response::Response {
    (StatusCode::OK, "ok").into_response()
}

async fn nao_encontrado() -> axum::response::Response {
    StatusCode::NOT_FOUND.into_response()
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let porta: u16 = std::env::var("PORTA")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9000);

    let app = Router::new()
        .route("/health", get(health))
        .route("/dados", get(dados))
        .fallback(nao_encontrado)
        .layer(axum::Extension(Contador::default()));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", porta)).await?;
    println!("upstream-fake ouvindo em 0.0.0.0:{porta}");
    axum::serve(listener, app).await?;
    Ok(())
}