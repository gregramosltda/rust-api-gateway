//! Destino simulado: um restaurante de sushi, servido pelo próprio processo.
//!
//! # Por que o simulador mora aqui dentro
//!
//! Uma demonstração que exige dois contêineres deixa de ser demonstrável: quem
//! clona o repositório precisa subir a topologia inteira antes de ver qualquer
//! coisa. Com o destino embutido, o gateway é autocontido — e continua
//! exercitando o caminho real, porque o proxy fala HTTP com ele pela rede,
//! atravessando as mesmas camadas que atravessaria falando com um serviço
//! externo: filtragem de cabeçalhos hop-by-hop, timeout, corpo em streaming.
//!
//! O que **não** é exercitado: autenticação mútua, TLS de saída e resolução de
//! DNS de um host real. Isso é do ambiente de produção, não do simulador.
//!
//! Em produção estas rotas ficam desligadas (`[mock] enabled = false`) e o
//! destino passa a ser o sistema do cliente.
//!
//! # O atraso é o ponto
//!
//! `delay_ms` existe para que o cache seja observável. Sem ele, uma resposta
//! servida do cache e uma que percorreu a rede levam o mesmo tempo, e a
//! aceleração que o cache entrega vira afirmação sem prova.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;

/// Um item do cardápio.
struct ItemCardapio {
    id: u32,
    nome: &'static str,
    preco_centavos: u32,
    descricao: &'static str,
}

/// Cardápio do restaurante.
///
/// Estático de propósito: um cardápio que muda a cada requisição tornaria a
/// demonstração de cache irreproduzível — e não haveria como distinguir uma
/// resposta vinda do cache de uma resposta nova. Valores em centavos para não
/// depender de ponto flutuante em dinheiro.
const CARDAPIO: &[ItemCardapio] = &[
    ItemCardapio {
        id: 1,
        nome: "Temaki Salmão",
        preco_centavos: 2890,
        descricao: "Cone de alga com salmão fresco e cream cheese",
    },
    ItemCardapio {
        id: 2,
        nome: "Uramaki Filadélfia",
        preco_centavos: 3250,
        descricao: "Arroz por fora, salmão e cream cheese por dentro",
    },
    ItemCardapio {
        id: 3,
        nome: "Sashimi de Atum",
        preco_centavos: 4100,
        descricao: "Fatias de atum fresco, sem arroz",
    },
    ItemCardapio {
        id: 4,
        nome: "Hossomaki Kani",
        preco_centavos: 2490,
        descricao: "Rolo fino com kani e pepino",
    },
    ItemCardapio {
        id: 5,
        nome: "Niguiri Salmão",
        preco_centavos: 1950,
        descricao: "Bolinho de arroz coberto com salmão",
    },
    ItemCardapio {
        id: 6,
        nome: "Hot Roll",
        preco_centavos: 3400,
        descricao: "Rolo empanado e frito, com molho tarê",
    },
];

/// Quantas vezes o cardápio foi realmente preparado pelo destino.
///
/// É o contador que prova o cache: com o cache servindo, ele para de subir
/// mesmo que o cliente insista na rota. Fica em memória de propósito — é
/// diagnóstico de demonstração, não dado de negócio.
static PREPAROS: AtomicU64 = AtomicU64::new(0);

/// Protocolo do próximo pedido.
static PEDIDOS: AtomicU64 = AtomicU64::new(0);

/// Quantidade máxima aceita em um pedido.
const QUANTIDADE_MAXIMA: u32 = 100;

/// `GET /mock/health` — o simulador está no ar.
///
/// Separado de `/health` porque responde a pergunta diferente: `/health` fala
/// do gateway, este fala do destino. Um gateway saudável com o destino fora do
/// ar é um cenário real e precisa ser distinguível na tela.
pub async fn health() -> Response {
    Json(json!({
        "status": "ok",
        "servico": "sushi-mock",
    }))
    .into_response()
}

/// Qualquer caminho sob `/mock` que o destino não serve.
///
/// Existe para fechar o prefixo: sem uma rota que o cubra por inteiro, um
/// caminho não previsto cairia no proxy e seria encaminhado para o próprio
/// gateway, acrescentando `/mock` a cada volta até esgotar o processo.
pub async fn nao_encontrado() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "rota_inexistente_no_destino",
                "message": "o destino não serve este caminho",
            }
        })),
    )
        .into_response()
}

/// `GET /mock/api/cardapio` — lista os sushis, após o atraso de preparo.
pub async fn cardapio(State(state): State<AppState>) -> Response {
    simular_preparo(&state).await;

    let chamadas = PREPAROS.fetch_add(1, Ordering::Relaxed) + 1;

    let itens: Vec<serde_json::Value> = CARDAPIO
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "nome": item.nome,
                "preco_centavos": item.preco_centavos,
                "descricao": item.descricao,
            })
        })
        .collect();

    Json(json!({
        "servico": "sushi-mock",
        "cardapio": itens,
        "chamadas_ao_destino": chamadas,
    }))
    .into_response()
}

/// Corpo aceito em `POST /mock/api/pedido`.
#[derive(Debug, Deserialize)]
pub struct Pedido {
    /// Nome do sushi escolhido.
    pub item: String,
    #[serde(default = "quantidade_padrao")]
    pub quantidade: u32,
}

fn quantidade_padrao() -> u32 {
    1
}

/// `POST /mock/api/pedido` — registra o pedido e devolve o protocolo.
pub async fn pedido(State(state): State<AppState>, Json(pedido): Json<Pedido>) -> Response {
    let item = pedido.item.trim().to_owned();

    if item.is_empty() || item.len() > 120 {
        return erro_de_pedido("item é obrigatório e deve ter até 120 caracteres");
    }
    if pedido.quantidade == 0 || pedido.quantidade > QUANTIDADE_MAXIMA {
        return erro_de_pedido("quantidade deve estar entre 1 e 100");
    }

    simular_preparo(&state).await;

    let id = PEDIDOS.fetch_add(1, Ordering::Relaxed) + 1;

    tracing::info!(
        id,
        item = %item,
        quantidade = pedido.quantidade,
        "pedido registrado pelo destino simulado"
    );

    (
        StatusCode::CREATED,
        Json(json!({
            "status": "preparando",
            "id": id,
            "item": item,
            "quantidade": pedido.quantidade,
            "previsao_minutos": 15,
        })),
    )
        .into_response()
}

fn erro_de_pedido(mensagem: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "code": "pedido_invalido",
                "message": mensagem,
            }
        })),
    )
        .into_response()
}

/// Aguarda o tempo de preparo configurado.
///
/// Um `sleep` de verdade, não um atraso artificial inoculado na resposta:
/// enquanto ele corre, a conexão está ocupada, que é exatamente a condição sob
/// a qual o cache deixa de ser conveniência e passa a ser capacidade.
async fn simular_preparo(state: &AppState) {
    let ms = state.config.mock.delay_ms;
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardapio_tem_ids_unicos_e_precos_positivos() {
        let mut ids: Vec<u32> = CARDAPIO.iter().map(|item| item.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "há id repetido no cardápio");

        for item in CARDAPIO {
            assert!(item.preco_centavos > 0, "{} sem preço", item.nome);
            assert!(!item.nome.is_empty(), "item sem nome");
            assert!(!item.descricao.is_empty(), "{} sem descrição", item.nome);
        }
    }

    #[test]
    fn cardapio_e_grande_o_bastante_para_a_demonstracao() {
        // Um cardápio de um item não mostra nada na tela.
        assert!(CARDAPIO.len() >= 4, "cardápio curto demais");
    }

    #[test]
    fn quantidade_padrao_de_pedido_e_um() {
        assert_eq!(quantidade_padrao(), 1);
    }
}
