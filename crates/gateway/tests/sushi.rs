//! Testes de integração da demonstração Sushi Tech.
//!
//! O que estes testes cobrem e por que deste jeito:
//!
//! - **O destino simulado está embutido**, então o `upstream.url` aponta para o
//!   próprio gateway sob `/mock`. Isso obriga a porta a ser conhecida antes da
//!   configuração — daí o `bind` acontecer primeiro e a URL ser montada depois.
//! - **Nada aqui é mock de HTTP.** O gateway fala TCP consigo mesmo, percorrendo
//!   as camadas reais: filtragem de cabeçalhos, timeout, streaming de corpo,
//!   cache e rate limit. Um duble de cliente validaria o Rust chamando o Rust.
//! - **A prova do cache é o contador do destino**, não o tempo. Medir tempo em
//!   teste é flaky sob carga; o contador prova a mesma coisa de forma exata —
//!   e a terceira chamada, com chave de cache diferente, prova que o contador
//!   sabe andar quando a ida à cozinha realmente acontece.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gateway::proxy::build_client;
use gateway::{construir_router, AppState, Config};
use gateway_cache::ResponseCache;
use gateway_rate_limiter::{ManualClock, MemoryBackend};
use tokio::net::TcpListener;

/// Gateway no ar, com o endereço já resolvido.
struct Gateway {
    base: String,
    cliente: reqwest::Client,
}

impl Gateway {
    fn url(&self, caminho: &str) -> String {
        format!("{}{}", self.base, caminho)
    }

    fn header(&self, resposta: &reqwest::Response, nome: &str) -> String {
        resposta
            .headers()
            .get(nome)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }
}

fn config_de_teste(upstream: &str, limite: u32, atraso_ms: u64) -> Config {
    toml::from_str(&format!(
        r#"
[server]
bind_addr = "127.0.0.1:0"

[upstream]
url = "{upstream}"
timeout_seconds = 5
connect_timeout_seconds = 2

[rate_limit]
enabled = true
backend = "memory"
redis_url = ""
default_limit = 1000
default_window_seconds = 60

[cache]
enabled = true
max_capacity = 100
ttl_seconds = 60

[mock]
enabled = true
delay_ms = {atraso_ms}

[[routes]]
path = "/api/cardapio"
limit = {limite}
window_seconds = 60

[[routes]]
path = "/api/pedido"
limit = {limite}
window_seconds = 60
"#
    ))
    .expect("config de teste")
}

/// Sobe o gateway com o destino simulado apontando para si mesmo.
async fn subir(limite: u32, atraso_ms: u64) -> Gateway {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let endereco = listener.local_addr().expect("endereço local");
    let upstream = format!("http://127.0.0.1:{}/mock", endereco.port());

    let config = config_de_teste(&upstream, limite, atraso_ms);
    let cliente_upstream =
        build_client(Duration::from_secs(2), Duration::from_secs(5)).expect("cliente upstream");
    let limiter = Arc::new(MemoryBackend::new(Arc::new(ManualClock::new(0))));
    let cache = ResponseCache::new(100, Duration::from_secs(60));

    let estado = AppState::new(config, cliente_upstream, limiter, cache, false);
    let app = construir_router(estado);

    tokio::spawn(async move {
        // `into_make_service_with_connect_info` é obrigatório: sem ele o handler
        // do proxy não recebe o `ConnectInfo` do cliente e toda requisição
        // proxiada falha com 500.
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Gateway {
        base: format!("http://{endereco}"),
        cliente: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("cliente de teste"),
    }
}

/// Lê `chamadas_ao_destino` de um corpo JSON.
fn contador(corpo: &str) -> u64 {
    let valor: serde_json::Value = serde_json::from_str(corpo).expect("corpo json");
    valor["chamadas_ao_destino"]
        .as_u64()
        .expect("campo chamadas_ao_destino")
}

// ---------------------------------------------------------------------------
// Interface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dashboard_e_servido_na_raiz() {
    let g = subir(8, 0).await;
    let resposta = g.cliente.get(g.url("/")).send().await.expect("requisição");

    assert_eq!(resposta.status(), 200);
    let tipo = g.header(&resposta, "content-type");
    assert!(tipo.contains("text/html"), "content-type veio {tipo}");

    let corpo = resposta.text().await.expect("corpo");
    assert!(corpo.contains("Sushi Tech Gateway"), "título ausente");
    assert!(
        corpo.contains("O Maître da sua operação"),
        "subtítulo ausente"
    );
}

// ---------------------------------------------------------------------------
// Destino simulado
// ---------------------------------------------------------------------------

#[tokio::test]
async fn destino_responde_saude() {
    let g = subir(8, 0).await;
    let resposta = g
        .cliente
        .get(g.url("/mock/health"))
        .send()
        .await
        .expect("requisição");

    assert_eq!(resposta.status(), 200);
    let valor: serde_json::Value = resposta.json().await.expect("json");
    assert_eq!(valor["status"], "ok");
}

#[tokio::test]
async fn destino_serve_o_cardapio() {
    let g = subir(8, 0).await;
    let resposta = g
        .cliente
        .get(g.url("/mock/api/cardapio"))
        .send()
        .await
        .expect("requisição");

    assert_eq!(resposta.status(), 200);
    let valor: serde_json::Value = resposta.json().await.expect("json");
    let itens = valor["cardapio"].as_array().expect("cardápio é lista");
    assert!(itens.len() >= 4, "cardápio curto demais");
    assert!(
        itens.iter().any(|i| i["nome"] == "Temaki Salmão"),
        "faltou o Temaki no cardápio"
    );
}

#[tokio::test]
async fn atraso_do_preparo_e_aplicado() {
    // O atraso é o que dá sentido à demonstração de cache. Se ele sumir, a
    // demonstração continua "passando" e deixa de provar qualquer coisa.
    let atraso = 200;
    let g = subir(8, atraso).await;

    let inicio = std::time::Instant::now();
    let resposta = g
        .cliente
        .get(g.url("/mock/api/cardapio"))
        .send()
        .await
        .expect("requisição");
    let decorrido = inicio.elapsed();

    assert_eq!(resposta.status(), 200);
    assert!(
        decorrido >= Duration::from_millis(atraso / 2),
        "o preparo não esperou: {decorrido:?} para atraso de {atraso}ms"
    );
}

// ---------------------------------------------------------------------------
// Cache — a demonstração do segundo clique
// ---------------------------------------------------------------------------

#[tokio::test]
async fn segundo_clique_vem_do_cache_e_nao_toca_o_destino() {
    let g = subir(50, 0).await;
    let url = g.url("/api/cardapio?teste=cache");

    let primeira = g.cliente.get(&url).send().await.expect("1ª requisição");
    assert_eq!(primeira.status(), 200);
    assert_eq!(
        g.header(&primeira, "x-cache"),
        "MISS",
        "a primeira leitura tem de ir à cozinha"
    );
    let corpo_um = primeira.text().await.expect("corpo 1");

    let segunda = g.cliente.get(&url).send().await.expect("2ª requisição");
    assert_eq!(segunda.status(), 200);
    assert_eq!(
        g.header(&segunda, "x-cache"),
        "HIT",
        "a segunda leitura tem de vir da memória"
    );
    let corpo_dois = segunda.text().await.expect("corpo 2");

    // Idênticos: a resposta do cache é a cópia guardada, não uma nova resposta
    // da cozinha que por acaso saiu igual.
    assert_eq!(
        corpo_um, corpo_dois,
        "o corpo do cache divergiu do guardado"
    );

    // Terceira chamada com outra chave: volta a tocar o destino e o contador
    // avança. Sem este passo, um cache que nunca existiu passaria no teste
    // acima se o contador estivesse congelado por outro motivo.
    let terceira = g
        .cliente
        .get(g.url("/api/cardapio?teste=cache-outro"))
        .send()
        .await
        .expect("3ª requisição");
    assert_eq!(g.header(&terceira, "x-cache"), "MISS");
    let corpo_tres = terceira.text().await.expect("corpo 3");

    assert!(
        contador(&corpo_tres) > contador(&corpo_um),
        "o contador do destino não avançou numa chamada sem cache: {} -> {}",
        contador(&corpo_um),
        contador(&corpo_tres)
    );
}

// ---------------------------------------------------------------------------
// Rate limit — a demonstração do ataque
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rajada_e_barrada_pelo_limite_da_rota() {
    let limite = 3_u32;
    let g = subir(limite, 0).await;
    let url = g.url("/api/cardapio");

    let mut passaram = 0_u32;
    let mut barradas = 0_u32;

    for indice in 1..=5 {
        let resposta = g.cliente.get(&url).send().await.expect("requisição");
        match resposta.status().as_u16() {
            200 => passaram += 1,
            429 => {
                assert!(
                    resposta.headers().contains_key("retry-after"),
                    "429 na requisição {indice} sem Retry-After"
                );
                barradas += 1;
            }
            outro => panic!("status inesperado na requisição {indice}: {outro}"),
        }
    }

    assert_eq!(passaram, limite, "deveriam passar exatamente {limite}");
    assert_eq!(barradas, 5 - limite);
}

#[tokio::test]
async fn resposta_liberada_traz_o_orcamento_restante() {
    let g = subir(7, 0).await;
    let resposta = g
        .cliente
        .get(g.url("/api/cardapio?teste=orcamento"))
        .send()
        .await
        .expect("requisição");

    assert_eq!(g.header(&resposta, "ratelimit-limit"), "7");
    assert_eq!(g.header(&resposta, "ratelimit-remaining"), "6");
}

#[tokio::test]
async fn escrita_barrada_nao_chega_ao_destino() {
    // Escrita não é cacheada, então cada tentativa consome orçamento sem
    // contrapartida de cache. Depois do limite, o destino não é incomodado.
    let limite = 2_u32;
    let g = subir(limite, 0).await;
    let url = g.url("/api/pedido");
    let pedido = serde_json::json!({ "item": "Hot Roll", "quantidade": 1 });

    for _ in 0..limite {
        let resposta = g
            .cliente
            .post(&url)
            .json(&pedido)
            .send()
            .await
            .expect("requisição");
        assert_eq!(resposta.status(), 201);
    }

    let barrada = g
        .cliente
        .post(&url)
        .json(&pedido)
        .send()
        .await
        .expect("requisição");
    assert_eq!(barrada.status(), 429);
}

// ---------------------------------------------------------------------------
// Recursão do autoproxy — o modo de falha que derruba o processo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn caminho_desconhecido_nao_entra_em_recursao() {
    // O proxy aponta para o próprio gateway. Um caminho que o destino não serve
    // é reescrito para `/mock/<caminho>`; se essa reescrita caísse de volta no
    // proxy, cada volta acrescentaria outro `/mock` e a requisição nunca
    // terminaria. O limite de tempo é a asserção: não travar é o requisito.
    let g = subir(1000, 0).await;

    let resposta = tokio::time::timeout(
        Duration::from_secs(10),
        g.cliente.get(g.url("/nao-existe")).send(),
    )
    .await
    .expect("a requisição travou: houve recursão no autoproxy")
    .expect("envio");

    assert_eq!(resposta.status(), 404);
}

#[tokio::test]
async fn caminho_interno_desconhecido_responde_como_destino() {
    let g = subir(1000, 0).await;
    let resposta = g
        .cliente
        .get(g.url("/mock/nao-existe"))
        .send()
        .await
        .expect("requisição");

    assert_eq!(resposta.status(), 404);
    let valor: serde_json::Value = resposta.json().await.expect("json");
    assert_eq!(valor["error"]["code"], "rota_inexistente_no_destino");
}

// ---------------------------------------------------------------------------
// Pedido
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pedido_valido_e_aceito() {
    let g = subir(100, 0).await;
    let resposta = g
        .cliente
        .post(g.url("/api/pedido"))
        .json(&serde_json::json!({ "item": "Temaki Salmão", "quantidade": 2 }))
        .send()
        .await
        .expect("requisição");

    assert_eq!(resposta.status(), 201);
    let valor: serde_json::Value = resposta.json().await.expect("json");
    assert_eq!(valor["status"], "preparando");
    assert_eq!(valor["item"], "Temaki Salmão");
    assert_eq!(valor["quantidade"], 2);
    assert!(valor["id"].as_u64().unwrap_or(0) >= 1, "protocolo inválido");
}

#[tokio::test]
async fn quantidade_zerada_e_rejeitada_antes_do_preparo() {
    let g = subir(100, 0).await;
    let resposta = g
        .cliente
        .post(g.url("/api/pedido"))
        .json(&serde_json::json!({ "item": "Hot Roll", "quantidade": 0 }))
        .send()
        .await
        .expect("requisição");

    assert_eq!(resposta.status(), 400);
    let valor: serde_json::Value = corpo_json(resposta).await;
    assert_eq!(valor["error"]["code"], "pedido_invalido");
}

async fn corpo_json(resposta: reqwest::Response) -> serde_json::Value {
    let texto = resposta.text().await.expect("texto");
    serde_json::from_str(&texto).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn pedido_sem_item_e_rejeitado() {
    let g = subir(100, 0).await;
    let resposta = g
        .cliente
        .post(g.url("/api/pedido"))
        .json(&serde_json::json!({ "quantidade": 1 }))
        .send()
        .await
        .expect("requisição");

    assert!(
        resposta.status().is_client_error(),
        "corpo sem item deveria ser recusado, veio {}",
        resposta.status()
    );
}
