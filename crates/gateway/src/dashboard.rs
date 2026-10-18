//! Dashboard do maître: a interface visual da demonstração.
//!
//! O HTML é compilado para dentro do binário com `include_str!`. Servir o
//! arquivo do disco em runtime criaria mais um caminho de leitura que pode
//! falhar dentro do contêiner — e o dashboard não tem por que ser editável em
//! produção. Se ele muda, muda o binário.
//!
//! A interface não fala com o destino simulado diretamente: fala com
//! `/api/cardapio` e `/api/pedido`, que são rotas do gateway. É o que faz os
//! botões exercitarem rate limit e cache de verdade, em vez de contorná-los.

use axum::response::{Html, IntoResponse, Response};

/// HTML da interface, embutido em tempo de compilação.
const DASHBOARD: &str = include_str!("../templates/index.html");

/// `GET /` — a interface do maître.
pub async fn index() -> Response {
    Html(DASHBOARD).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_esta_embutido_e_tem_conteudo() {
        assert!(
            DASHBOARD.len() > 2_000,
            "dashboard suspeito de estar vazio ou truncado: {} bytes",
            DASHBOARD.len()
        );
    }

    #[test]
    fn dashboard_aponta_para_as_rotas_do_gateway() {
        // Se as rotas mudarem de nome, a interface precisa mudar junto. Este
        // teste falha antes de alguém descobrir clicando.
        for caminho in [
            "/api/cardapio",
            "/api/pedido",
            "/health",
            "/ready",
            "/mock/health",
        ] {
            assert!(
                DASHBOARD.contains(caminho),
                "o dashboard não referencia {caminho}"
            );
        }
    }

    #[test]
    fn dashboard_nao_referencia_porta_fixa() {
        // O gateway pode estar atrás de proxy em outra porta; qualquer chamada
        // com host ou porta fixos quebraria em produção.
        assert!(!DASHBOARD.contains("localhost:"));
        assert!(!DASHBOARD.contains("127.0.0.1"));
    }
}
