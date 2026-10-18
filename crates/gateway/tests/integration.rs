//! Testes de integração do pipeline completo.
//!
//! Sobem um upstream real (axum, em porta efêmera) e o gateway sobre ele, e
//! exercitam o que o cliente enxerga: status, cabeçalhos, rate limiting e
//! cache.
//!
//! O upstream é um servidor de verdade, e não um mock de cliente HTTP, porque o
//! que se quer testar é a integração entre as duas pontas — incluindo o
//! encaminhamento de cabeçalhos e o status de volta.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use gateway::proxy::build_client;
use gateway::{construir_router, AppState, Config};
use gateway_cache::ResponseCache;
use gateway_rate_limiter::{ManualClock, MemoryBackend};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Upstream de teste
// ---------------------------------------------------------------------------

/// Contadores observáveis do upstream.
#[derive(Clone, Default)]
struct Contadores {
    requisicoes: Arc<AtomicU32>,
}

impl Contadores {
    fn total(&self) -> u32 {
        self.requisicoes.load(Ordering::SeqCst)
    }
}

/// Handlers do upstream. Ficam no escopo do módulo porque um `async fn`
/// definido dentro de outra função vira item local, sem os bounds de trait que
/// `Handler` exige — e o erro resultante não diz qual handler é o culpado.
async fn dados(axum::Extension(c): axum::Extension<Contadores>) -> axum::response::Response {
    c.requisicoes.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        "resposta-do-upstream",
    )
        .into_response()
}

async fn criado() -> axum::response::Response {
    StatusCode::CREATED.into_response()
}

async fn falha() -> axum::response::Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn eco_headers(headers: HeaderMap) -> axum::response::Response {
    let mut chaves: Vec<String> = headers.keys().map(|k| k.as_str().to_owned()).collect();
    chaves.sort();
    axum::Json(serde_json::json!({ "headers": chaves })).into_response()
}

async fn nao_encontrado() -> axum::response::Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn subir_upstream() -> (SocketAddr, Contadores) {
    let contadores = Contadores::default();

    let app = Router::new()
        .route("/dados", get(dados))
        .route("/criar", post(criado))
        .route("/falha", get(falha))
        .route("/echo-headers", get(eco_headers))
        .fallback(nao_encontrado)
        .layer(axum::Extension(contadores.clone()));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let endereco = listener.local_addr().expect("addr upstream");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    (endereco, contadores)
}

// ---------------------------------------------------------------------------
// Gateway de teste
// ---------------------------------------------------------------------------

fn config_de_teste(upstream: SocketAddr, limite: u32, janela: u64) -> Config {
    toml::from_str(&format!(
        r#"
[server]
bind_addr = "127.0.0.1:0"

[upstream]
url = "http://{upstream}"
timeout_seconds = 5
connect_timeout_seconds = 2

[rate_limit]
enabled = true
redis_url = "redis://127.0.0.1:6379"
default_limit = {limite}
default_window_seconds = {janela}

[cache]
enabled = true
max_capacity = 100
ttl_seconds = 60
"#
    ))
    .expect("config de teste")
}

struct Harness {
    endereco: SocketAddr,
    clock: Arc<ManualClock>,
    cliente: reqwest::Client,
}

impl Harness {
    async fn subir(config: Config, clock: Arc<ManualClock>) -> Self {
        let upstream_client =
            build_client(Duration::from_secs(2), Duration::from_secs(5)).expect("cliente upstream");
        let limiter = Arc::new(MemoryBackend::new(clock.clone()));
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        let estado = AppState::new(config, upstream_client, limiter, cache, false);
        let app = construir_router(estado);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway");
        let endereco = listener.local_addr().expect("addr gateway");
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        Self {
            endereco,
            clock,
            cliente: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("cliente de teste"),
        }
    }

    fn url(&self, caminho: &str) -> String {
        format!("http://{}{caminho}", self.endereco)
    }

    async fn get(&self, caminho: &str) -> reqwest::Response {
        self.cliente
            .get(self.url(caminho))
            .send()
            .await
            .expect("GET deve responder")
    }

    async fn post(&self, caminho: &str) -> reqwest::Response {
        self.cliente
            .post(self.url(caminho))
            .send()
            .await
            .expect("POST deve responder")
    }
}

fn header<'a>(r: &'a reqwest::Response, nome: &str) -> Option<&'a str> {
    r.headers().get(nome).and_then(|v| v.to_str().ok())
}

async fn json(r: reqwest::Response) -> serde_json::Value {
    let texto = r.text().await.unwrap_or_default();
    serde_json::from_str(&texto).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_encaminha_e_devolve_corpo_do_upstream() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    let r = h.get("/dados").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.text().await.unwrap_or_default(), "resposta-do-upstream");
}

#[tokio::test]
async fn status_do_upstream_e_preservado() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    assert_eq!(
        h.get("/falha").await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(h.get("/inexistente").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rate_limiter_devolve_429_com_headers() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 3, 60), clock).await;

    for _ in 0..3 {
        assert_eq!(h.get("/dados").await.status(), StatusCode::OK);
    }

    let barrado = h.get("/dados").await;
    assert_eq!(barrado.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&barrado, "RateLimit-Limit"), Some("3"));
    assert_eq!(header(&barrado, "RateLimit-Remaining"), Some("0"));
    assert!(header(&barrado, "RateLimit-Reset").is_some());
    assert!(header(&barrado, "Retry-After").is_some());

    let corpo = json(barrado).await;
    assert_eq!(corpo["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn requisicao_barrada_nao_chega_ao_upstream() {
    let (upstream_addr, c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 2, 60), clock).await;

    let antes = c.total();
    for _ in 0..10 {
        let _ = h.get("/dados").await;
    }
    let no_upstream = c.total() - antes;
    assert!(
        no_upstream <= 2,
        "só {no_upstream} deveria(ram) chegar ao upstream"
    );
}

#[tokio::test]
async fn cache_serve_segunda_leitura_sem_bater_no_upstream() {
    let (upstream_addr, c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 1000, 60), clock).await;

    let antes = c.total();

    let primeira = h.get("/dados").await;
    assert_eq!(header(&primeira, "X-Cache"), Some("MISS"));

    let segunda = h.get("/dados").await;
    assert_eq!(header(&segunda, "X-Cache"), Some("HIT"));
    assert_eq!(
        segunda.text().await.unwrap_or_default(),
        "resposta-do-upstream"
    );

    assert_eq!(
        c.total() - antes,
        1,
        "cache deveria evitar a segunda ida ao upstream"
    );
}

#[tokio::test]
async fn queries_diferentes_nao_compartilham_cache() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 1000, 60), clock).await;

    h.get("/dados?x=1").await;
    let segunda = h.get("/dados?x=2").await;
    assert_eq!(
        header(&segunda, "X-Cache"),
        Some("MISS"),
        "queries distintas são entradas distintas"
    );
}

#[tokio::test]
async fn escrita_invalida_o_cache() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 1000, 60), clock).await;

    h.get("/dados").await;
    assert_eq!(header(&h.get("/dados").await, "X-Cache"), Some("HIT"));

    // O POST no próprio prefixo invalida; a leitura seguinte vai ao upstream.
    h.post("/dados").await;
    assert_eq!(header(&h.get("/dados").await, "X-Cache"), Some("MISS"));
}

#[tokio::test]
async fn metodo_post_nao_e_cacheado() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 1000, 60), clock).await;

    let r = h.post("/criar").await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_ne!(header(&r, "X-Cache"), Some("HIT"));
}

#[tokio::test]
async fn cabecalhos_hop_by_hop_nao_chegam_ao_upstream() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 1000, 60), clock).await;

    let r = h.get("/echo-headers").await;
    let recebidos = json(r).await;
    let lista = recebidos["headers"].as_array().expect("lista de headers");

    for proibido in ["connection", "transfer-encoding", "content-length"] {
        assert!(
            !lista.iter().any(|v| v == proibido),
            "{proibido} não deveria chegar ao upstream: {lista:?}"
        );
    }
}

#[tokio::test]
async fn health_responde_sem_dependencia() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    let r = h.get("/health").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json(r).await["status"], "ok");
}

#[tokio::test]
async fn ready_responde_ok_com_dependencias_saudaveis() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    let r = h.get("/ready").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json(r).await["status"], "pronto");
}

#[tokio::test]
async fn metrics_expoem_contadores() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    h.get("/dados").await;
    let r = h.get("/metrics").await;
    let corpo = json(r).await;
    assert!(corpo["requisicoes"].as_u64().expect("contador") >= 1);
    assert!(corpo["cache"].is_object());
}

#[tokio::test]
async fn rota_interna_tem_precedencia_sobre_o_proxy() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    // Se o wildcard fosse registrado antes, o router encaminharia /health ao
    // upstream e o upstream devolveria 404.
    assert_eq!(h.get("/health").await.status(), StatusCode::OK);
    assert_eq!(h.get("/ready").await.status(), StatusCode::OK);
    assert_eq!(h.get("/metrics").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn upstream_inacessivel_devolve_502() {
    let clock = Arc::new(ManualClock::new(0));
    let config: Config = toml::from_str(
        r#"
[upstream]
url = "http://127.0.0.1:1"
timeout_seconds = 2

[rate_limit]
enabled = false
redis_url = "redis://127.0.0.1:6379"

[cache]
enabled = false
"#,
    )
    .expect("config");

    let h = Harness::subir(config, clock).await;
    let r = h.get("/dados").await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(json(r).await["error"]["code"], "upstream_unreachable");
}

#[tokio::test]
async fn cache_desligado_por_config_nao_marca_hit() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let mut config = config_de_teste(upstream_addr, 1000, 60);
    config.cache.enabled = false;

    let h = Harness::subir(config, clock).await;
    h.get("/dados").await;
    let r = h.get("/dados").await;
    assert_ne!(header(&r, "X-Cache"), Some("HIT"));
}

#[tokio::test]
async fn janela_deslizante_libera_orcamento_apos_avancar_relogio() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 2, 10), clock).await;

    assert_eq!(h.get("/dados").await.status(), StatusCode::OK);
    assert_eq!(h.get("/dados").await.status(), StatusCode::OK);
    assert_eq!(
        h.get("/dados").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    h.clock.advance(Duration::from_secs(11));
    assert_eq!(
        h.get("/dados").await.status(),
        StatusCode::OK,
        "após a janela, o orçamento deve voltar"
    );
}

#[tokio::test]
async fn requisicoes_concorrentes_respeitam_o_limite() {
    let (upstream_addr, c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Arc::new(Harness::subir(config_de_teste(upstream_addr, 20, 60), clock).await);

    let antes = c.total();

    let tarefas: Vec<_> = (0..60)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move { h.get("/dados").await.status() })
        })
        .collect();

    let mut permitidas = 0_u32;
    for t in tarefas {
        if t.await.expect("task") == StatusCode::OK {
            permitidas += 1;
        }
    }

    assert_eq!(
        permitidas, 20,
        "exatamente o limite deveria passar, veio {permitidas}"
    );
    assert!(
        c.total() - antes <= 25,
        "o upstream não deveria ver mais que o limite (+ cache): {}",
        c.total() - antes
    );
}

#[tokio::test]
async fn request_id_correlaciona_a_resposta() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 100, 60), clock).await;

    let r = h.get("/dados").await;
    let id = header(&r, "X-Request-Id").expect("request id");
    assert!(!id.is_empty(), "o id não pode vir vazio");
}

#[tokio::test]
async fn headers_de_orcamento_tambem_vem_em_requisicao_permitida() {
    let (upstream_addr, _c) = subir_upstream().await;
    let clock = Arc::new(ManualClock::new(0));
    let h = Harness::subir(config_de_teste(upstream_addr, 5, 60), clock).await;

    let r = h.get("/dados").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(header(&r, "RateLimit-Limit"), Some("5"));
    assert!(header(&r, "RateLimit-Remaining").is_some());
}
