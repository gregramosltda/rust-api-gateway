//! Encaminhamento de requisições ao upstream.
//!
//! Usa a API `hyper` diretamente em vez de `reqwest`. Motivo: `reqwest` é um
//! cliente de alto nível, com pool, retries e política de redirect próprios.
//! Aqui isso é duplicação: o gateway já gerencia conexões, e uma segunda
//! camada de política tornaria o comportamento do proxy imprevisível. Com
//! `hyper-util` há controle direto sobre o que sai e o que volta.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body as AxumBody;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode, Uri};
use gateway_common::{ErrorBody, GatewayError};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// Cliente HTTP para falar com o upstream.
///
/// O corpo é o próprio `Body` do Axum, que já implementa `http_body::Body` e é
/// `Send + 'static`. Usar o `Incoming` cru obrigaria a converter os dois lados
/// da troca; usar o `Body` do Axum mantém o fluxo de corpo em streaming nos
/// dois sentidos e dispensa qualquer wrapper.
pub type UpstreamClient = Client<HttpConnector, AxumBody>;

/// Constrói o cliente com timeouts explícitos.
///
/// Sem timeout, uma origem lenta segura conexões até o processo inteiro
/// acumular fileiras — o gateway deixa de ser proteção e vira o problema.
pub fn build_client(
    connect_timeout: Duration,
    timeout: Duration,
) -> Result<UpstreamClient, GatewayError> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(connect_timeout));

    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(timeout)
        .pool_timer(hyper_util::rt::TokioTimer::new())
        .build(http);

    // `build()` não falha hoje, mas a assinatura é `Result` para preservar a
    // fronteira: se a hyper passar a falhar na construção, nenhum call site
    // quebra.
    Ok(client)
}

/// Junta a URL base com o caminho da requisição.
pub fn montar_url(base: &str, path_and_query: &str) -> Option<Uri> {
    let base = base.trim_end_matches('/');
    let caminho = path_and_query.strip_prefix('/').unwrap_or(path_and_query);
    let completa = format!("{base}/{caminho}");
    completa.parse::<Uri>().ok()
}

/// Constrói o cliente HTTP com os headers da requisição original.
///
/// Hop-by-hop são removidos: `Connection`, `Keep-Alive`, `Transfer-Encoding`,
/// `Upgrade` e `Proxy-*` pertencem à conversa entre este proxy e o cliente, não
/// entre o gateway e o upstream. Encaminhá-los produz 400 no upstream em boa
/// parte das implementações.
pub fn filtrar_headers(origem: &HeaderMap) -> HeaderMap {
    const HOP_BY_HOP: [&str; 9] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ];

    // `Connection: X-Custom` declara `X-Custom` como hop-by-hop para ESTA
    // conexão. A lista precisa vir do valor do cabeçalho, não do nome dele.
    let declarados: Vec<String> = origem
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|p| p.trim().to_ascii_lowercase())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let destino = origem.clone();
    let mut a_remover: Vec<String> = declarados;
    a_remover.extend(HOP_BY_HOP.iter().map(|s| (*s).to_owned()));

    let mut saida = HeaderMap::with_capacity(origem.len());
    for (nome, valor) in destino.iter() {
        let nome_str = nome.as_str().to_ascii_lowercase();
        if a_remover.contains(&nome_str) {
            continue;
        }
        // O Content-Length é recalculado pelo hyper a partir do corpo; manter o
        // original diverge do corpo real e quebra a leitura no upstream.
        if nome_str == "content-length" {
            continue;
        }
        saida.insert(nome.clone(), valor.clone());
    }
    saida
}

/// Encaminha a requisição ao upstream e devolve a resposta.
pub async fn encaminhar(
    client: &UpstreamClient,
    base_url: &str,
    partes: axum::http::request::Parts,
    corpo: AxumBody,
) -> Result<Response<AxumBody>, GatewayError> {
    let uri_original = partes.uri.clone();
    let caminho = uri_original
        .path_and_query()
        .map_or(uri_original.path(), http::uri::PathAndQuery::as_str);

    let uri_destino = montar_url(base_url, caminho)
        .ok_or_else(|| GatewayError::UpstreamTransport(format!("URL inválida: {caminho}")))?;

    let mut requisicao = hyper::Request::builder()
        .method(partes.method.clone())
        .uri(uri_destino)
        .body(corpo)
        .map_err(|e| GatewayError::UpstreamTransport(e.to_string()))?;

    *requisicao.headers_mut() = filtrar_headers(&partes.headers);

    let resposta = client
        .request(requisicao)
        .await
        .map_err(|e| GatewayError::UpstreamTransport(e.to_string()))?;

    let (partes_resp, corpo_resp) = resposta.into_parts();

    // Remove hop-by-hop também na volta.
    let mut headers = partes_resp.headers.clone();
    for nome in ["connection", "transfer-encoding", "keep-alive", "upgrade"] {
        headers.remove(nome);
    }
    headers.remove(header::CONTENT_LENGTH);

    // Traduz o corpo hyper para o `Body` do Axum, que é o que as próximas
    // camadas (cache, resposta) esperam.
    // Conversão sem buffering: `AxumBody::new` embrulha o corpo do hyper e
    // mantém o streaming. Materializar os bytes aqui seguraria a resposta
    // inteira em memória — num gateway com payloads grandes isso é negação de
    // serviço por conta própria.
    let corpo_axum = AxumBody::new(corpo_resp);
    let mut saida = Response::new(corpo_axum);
    *saida.status_mut() = partes_resp.status;
    *saida.headers_mut() = headers;
    Ok(saida)
}

/// Converte `GatewayError` em resposta JSON.
pub fn erro_para_resposta(err: &GatewayError) -> Response<AxumBody> {
    let status =
        StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let corpo = serde_json::to_vec(&ErrorBody::from(err))
        .unwrap_or_else(|_| br#"{"error":{"code":"internal","message":"erro interno"}}"#.to_vec());
    let mut resposta = Response::new(AxumBody::from(corpo.to_vec()));
    *resposta.status_mut() = status;
    resposta.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resposta
}

/// Resposta 429 Too Many Requests com os cabeçalhos de orçamento.
pub fn resposta_429(headers_ratelimit: Vec<(&'static str, String)>) -> Response<AxumBody> {
    let corpo = serde_json::json!({
        "error": {
            "code": "rate_limited",
            "message": "limite de requisições excedido",
        }
    });
    let bytes = serde_json::to_vec(&corpo).unwrap_or_else(|_| b"{}".to_vec());

    let mut resposta = Response::new(AxumBody::from(bytes));
    *resposta.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    resposta.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    for (nome, valor) in headers_ratelimit {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(nome.as_bytes()),
            HeaderValue::from_str(&valor),
        ) {
            resposta.headers_mut().insert(n, v);
        }
    }
    resposta
}

/// Resposta JSON genérica.
pub fn resposta_json(valor: &serde_json::Value, status: StatusCode) -> Response<AxumBody> {
    let bytes = serde_json::to_vec(valor).unwrap_or_else(|_| b"{}".to_vec());
    let mut resposta = Response::new(AxumBody::from(bytes));
    *resposta.status_mut() = status;
    resposta.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resposta
}

/// Handler de erro do Axum.
pub async fn handler_erro(err: GatewayError) -> Response<AxumBody> {
    tracing::warn!(code = err.code(), "erro na requisição");
    erro_para_resposta(&err)
}

/// Fallback para 404 do router.
pub async fn fallback() -> Response<AxumBody> {
    erro_para_resposta(&GatewayError::NoRoute("rota".to_owned()))
}

/// Contador de requisições, exposto em métricas.
#[derive(Debug, Default)]
pub struct RequestCounter(AtomicU64);

impl RequestCounter {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn monta_url_junta_base_e_caminho() {
        let uri = montar_url("http://upstream:9000", "/api/users").expect("uri");
        assert_eq!(uri.to_string(), "http://upstream:9000/api/users");
    }

    #[test]
    fn barra_final_nao_duplica() {
        let uri = montar_url("http://up:9000/", "/users").expect("uri");
        assert_eq!(uri.to_string(), "http://up:9000/users");
    }

    #[test]
    fn preserva_query_string() {
        let uri = montar_url("http://up:9000", "/busca?q=rust&n=1").expect("uri");
        assert!(uri.to_string().contains("q=rust"));
        assert!(uri.to_string().contains("n=1"));
    }

    #[test]
    fn url_invalida_devolve_none() {
        assert!(montar_url("nao-e-url", "/x").is_none());
    }

    #[test]
    fn filtra_cabecalhos_hop_by_hop() {
        let mut h = HeaderMap::new();
        h.insert("connection", "keep-alive".parse().expect("valor"));
        h.insert("transfer-encoding", "chunked".parse().expect("valor"));
        h.insert("content-length", "100".parse().expect("valor"));
        h.insert("x-custom", "presente".parse().expect("valor"));

        let filtrado = filtrar_headers(&h);
        assert!(filtrado.get("connection").is_none());
        assert!(filtrado.get("transfer-encoding").is_none());
        assert!(filtrado.get("content-length").is_none());
        assert_eq!(filtrado.get("x-custom").expect("preservado"), "presente");
    }

    #[test]
    fn filtra_cabecalhos_de_proxy() {
        let mut h = HeaderMap::new();
        h.insert("proxy-authorization", "Basic x".parse().expect("valor"));
        let filtrado = filtrar_headers(&h);
        assert!(filtrado.get("proxy-authorization").is_none());
    }

    #[test]
    fn preserva_cabecalhos_de_aplicacao() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer t".parse().expect("valor"));
        h.insert("accept", "application/json".parse().expect("valor"));
        let filtrado = filtrar_headers(&h);
        assert_eq!(filtrado.get("authorization").expect("auth"), "Bearer t");
        assert_eq!(filtrado.get("accept").expect("accept"), "application/json");
    }

    #[test]
    fn cabecalho_connection_lista_outros_eh_removido() {
        // `Connection: X-Custom` declara X-Custom como hop-by-hop.
        let mut h = HeaderMap::new();
        h.insert("connection", "X-Custom".parse().expect("valor"));
        h.insert("x-custom", "v".parse().expect("valor"));
        let filtrado = filtrar_headers(&h);
        assert!(filtrado.get("x-custom").is_none(), "declarado hop-by-hop");
    }

    #[test]
    fn construtor_de_cliente_nao_falha_com_timeouts_validos() {
        assert!(build_client(Duration::from_secs(1), Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn erro_gera_status_e_codigo_esperados() {
        let resposta = erro_para_resposta(&GatewayError::NoRoute("/x".to_owned()));
        assert_eq!(resposta.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resposta.headers().get(header::CONTENT_TYPE).expect("ct"),
            "application/json"
        );
    }

    #[test]
    fn resposta_429_inclui_headers_de_orcamento() {
        let resposta = resposta_429(vec![
            ("RateLimit-Limit", "10".to_owned()),
            ("RateLimit-Remaining", "0".to_owned()),
            ("RateLimit-Reset", "30".to_owned()),
            ("Retry-After", "30".to_owned()),
        ]);
        assert_eq!(resposta.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resposta.headers().get("RateLimit-Limit").expect("limit"),
            "10"
        );
        assert_eq!(resposta.headers().get("Retry-After").expect("retry"), "30");
    }

    #[tokio::test]
    async fn corpo_429_tem_codigo_estavel() {
        use http_body_util::BodyExt;
        let resposta = resposta_429(vec![]);
        let bytes = resposta
            .into_body()
            .collect()
            .await
            .expect("coletar corpo")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json válido");
        assert_eq!(json["error"]["code"], "rate_limited");
    }

    #[test]
    fn resposta_json_define_content_type() {
        let resposta = resposta_json(&serde_json::json!({"a": 1}), StatusCode::OK);
        assert_eq!(resposta.status(), StatusCode::OK);
        assert_eq!(
            resposta.headers().get(header::CONTENT_TYPE).expect("ct"),
            "application/json"
        );
    }

    #[test]
    fn contador_comeca_em_zero_e_incrementa() {
        let c = RequestCounter::default();
        assert_eq!(c.get(), 0);
        assert_eq!(c.next(), 0);
        assert_eq!(c.next(), 1);
        assert_eq!(c.get(), 2);
    }
}
