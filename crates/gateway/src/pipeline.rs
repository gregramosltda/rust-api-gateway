//! Pipeline de requisição: rate limit → cache → invalidação → proxy → cache.
//!
//! A ordem das etapas não é arbitrária.
//!
//! **Rate limit antes do cache** de propósito. Uma requisição barrada (429) é
//! recusada antes de o cache sequer ser consultado: ela não lê, não escreve e
//! não consome recurso algum. A ordem oposta — cache primeiro — parece mais
//! eficiente, porque uma leitura repetida não gastaria orçamento nem chegaria
//! ao upstream. Mas isso esvazia o limite: um cliente que só lê o mesmo recurso
//! em loop, que é exatamente o abuso que o rate limiter existe para conter,
//! seria servido do cache e nunca contaria. E o recurso em cache é justamente o
//! quente, o mais visado.
//!
//! O preço dessa escolha é conhecido e aceito: uma leitura repetida gasta
//! orçamento mesmo sendo servida do cache. O cache continua economizando carga
//! do upstream; o orçamento continua contando todas as requisições. Um limite
//! que sempre vale é melhor que um limite que só vale quando a resposta é cara.
//!
//! **Invalidação antes de encaminhar escrita.** Um `POST` bem-sucedido que não
//! invalida o cache deixa a resposta antiga servida até o TTL expirar — o
//! cliente vê o dado pré-escrita. Errar para "mostrar dado velho" é melhor que
//! errar para "não mostrar o dado", mas do jeito certo: invalida-se antes.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, OriginalUri, State};
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use gateway_cache::{CacheOutcome, CachedResponse};
use gateway_common::{budget_key, GatewayError};

use crate::net::{anexar_diagnostico, client_ip, gerar_request_id, ip_to_key, HEADER_CACHE_STATUS};
use crate::proxy::{encaminhar, erro_para_resposta, resposta_429};
use crate::state::AppState;

/// Limite máximo de corpo lido do cliente (1 MiB).
pub const LIMITE_CORPO: usize = 1024 * 1024;

/// Handler único de proxy.
pub async fn proxy(
    State(state): State<AppState>,
    OriginalUri(uri_original): OriginalUri,
    ConnectInfo(socket): ConnectInfo<std::net::SocketAddr>,
    requisicao: Request<Body>,
) -> Response {
    let sequencia = state.counter.next();
    let inicio = std::time::Instant::now();

    let (mut partes, corpo) = requisicao.into_parts();
    // Usa a URI original: o wildcard `/*caminho` reescreve `partes.uri`, e
    // encaminhar esse caminho interno vazaria a rota do router no upstream.
    let metodo = partes.method.as_str().to_owned();
    let caminho = uri_original.path().to_owned();
    let query = uri_original.query().map(str::to_owned);
    partes.uri = uri_original;

    let ip = ip_to_key(&client_ip(
        &partes.headers,
        Some(socket),
        state.confia_proxy,
    ));
    let request_id = gerar_request_id(sequencia, inicio.elapsed().as_millis() as u64);

    tracing::info!(
        request_id = %request_id,
        metodo = %metodo,
        caminho = %caminho,
        cliente = %ip,
        "requisição recebida"
    );

    // --- 1. Rate limit ----------------------------------------------------
    // Rate limit ANTES do cache. A ordem oposta — cache primeiro — parece mais
    // eficiente, porque uma leitura repetida não gasta orçamento nem chega ao
    // upstream. Mas deixa o limite sem efeito prático: um cliente que só lê o
    // mesmo recurso em loop nunca é barrado, que é exatamente o abuso que o
    // rate limiter existe para conter. O cache continua economizando carga do
    // upstream; o orçamento continua contando todas as requisições.
    let mut headers_orcamento: Vec<(&'static str, String)> = Vec::new();

    if state.config.rate_limit.enabled {
        let (limite, janela) = state.config.limit_for(&caminho);
        let chave = budget_key(&ip, &caminho, &state.config.rate_limit.key_prefix);

        match state.limiter.check(&chave, limite, janela).await {
            Ok(decisao) => {
                headers_orcamento = decisao.headers();
                if !decisao.is_allowed() {
                    tracing::warn!(
                        request_id = %request_id,
                        cliente = %ip,
                        caminho = %caminho,
                        limite,
                        "rate limit excedido"
                    );
                    let mut resposta = resposta_429(decisao.headers());
                    anexar_diagnostico(
                        resposta.headers_mut(),
                        None,
                        None,
                        Some(state.upstream_base()),
                        Some(&request_id),
                    );
                    return resposta;
                }
                tracing::debug!(
                    request_id = %request_id,
                    restante = decisao.remaining(),
                    "rate limit liberado"
                );
            }
            Err(err) => {
                // `fail_open` decide o trade-off: disponibilidade contra proteção.
                // Um gateway que barra tudo quando o Redis cai é um outage
                // autoinflito; um que libera tudo vira passarela em DDoS.
                if state.config.rate_limit.fail_open {
                    tracing::error!(
                        request_id = %request_id,
                        codigo = err.code(),
                        "rate limiter indisponível, liberando (fail_open)"
                    );
                } else {
                    tracing::error!(
                        request_id = %request_id,
                        codigo = err.code(),
                        "rate limiter indisponível, bloqueando (fail_closed)"
                    );
                    return erro_para_resposta(&err);
                }
            }
        }
    }

    // --- 2. Cache ---------------------------------------------------------
    let chave_cache = gateway_cache::ResponseCache::key(&metodo, &caminho, query.as_deref());
    let elegivel_cache = state.config.cache_enabled_for(&caminho)
        && gateway_cache::is_cacheable_method(&metodo, &state.config.cache.cacheable_methods);

    if elegivel_cache {
        match state.cache.get(&chave_cache).await {
            CacheOutcome::Hit(cached) => {
                let mut resposta = resposta_de_cache(&cached);
                anexar_diagnostico(
                    resposta.headers_mut(),
                    Some("HIT"),
                    Some(&chave_cache),
                    Some(state.upstream_base()),
                    Some(&request_id),
                );
                anexar_orcamento(resposta.headers_mut(), &headers_orcamento);
                tracing::info!(request_id = %request_id, "cache HIT");
                return resposta;
            }
            CacheOutcome::Miss => {
                tracing::debug!(request_id = %request_id, chave = %chave_cache, "cache MISS");
            }
        }
    }

    // --- 3. Invalidação por escrita --------------------------------------
    // Antes de encaminhar: se o upstream rejeitar, o cache já válido continua
    // servindo dado correto.
    let eh_escrita =
        !gateway_cache::is_cacheable_method(&metodo, &state.config.cache.cacheable_methods);
    if state.config.cache.enabled && eh_escrita {
        let antes = state.cache.invalidate_prefix(&caminho).await;
        tracing::debug!(
            request_id = %request_id,
            metodo = %metodo,
            caminho = %caminho,
            removidas = antes,
            "cache invalidado por escrita"
        );
    }

    // --- 4. Proxy ---------------------------------------------------------
    let base = state.upstream_base().to_owned();
    let client = Arc::clone(&state.upstream);

    let resposta = match tokio::time::timeout(
        state.upstream_timeout(),
        encaminhar(&client, &base, partes, corpo),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(err)) => {
            tracing::error!(request_id = %request_id, codigo = err.code(), "falha no upstream");
            return erro_para_resposta(&err);
        }
        Err(_) => {
            tracing::error!(request_id = %request_id, "timeout ao falar com o upstream");
            return erro_para_resposta(&GatewayError::UpstreamTransport("timeout".to_owned()));
        }
    };

    let status = resposta.status();
    let cacheavel = elegivel_cache && status.is_success();

    if cacheavel {
        // O corpo precisa ser materializado para ser cacheado. O custo é uma
        // cópia — aceitável porque só respostas pequenas são cacheadas (o
        // limite de corpo vale para a resposta também, por simetria de uso).
        let (partes_resp, corpo_resp) = resposta.into_parts();
        let bytes = match axum::body::to_bytes(corpo_resp, LIMITE_CORPO).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(request_id = %request_id, erro = %err, "não foi possível ler o corpo do upstream");
                return erro_para_resposta(&GatewayError::UpstreamTransport(
                    "corpo ilegível".to_owned(),
                ));
            }
        };
        if bytes.len() <= LIMITE_CORPO {
            let content_type = partes_resp
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            state
                .cache
                .insert(
                    &chave_cache,
                    CachedResponse::new(status.as_u16(), content_type, bytes.to_vec()),
                )
                .await;
        }

        let mut saida = Response::new(Body::from(bytes));
        *saida.status_mut() = status;
        *saida.headers_mut() = partes_resp.headers;
        anexar_diagnostico(
            saida.headers_mut(),
            Some("MISS"),
            Some(&chave_cache),
            Some(&base),
            Some(&request_id),
        );
        anexar_orcamento(saida.headers_mut(), &headers_orcamento);
        return saida;
    }

    let mut saida = resposta;
    anexar_diagnostico(
        saida.headers_mut(),
        None,
        None,
        Some(&base),
        Some(&request_id),
    );
    anexar_orcamento(saida.headers_mut(), &headers_orcamento);

    tracing::info!(
        request_id = %request_id,
        status = status.as_u16(),
        ms = inicio.elapsed().as_millis() as u64,
        "requisição concluída"
    );

    saida
}

/// Anexa `RateLimit-*` à resposta.
///
/// Valor de cabeçalho inválido é descartado em vez de derrubar a resposta: um
/// `Remaining` malformado é um defeito de observação, não motivo para falhar uma
/// requisição que já deu certo.
fn anexar_orcamento(headers: &mut axum::http::HeaderMap, orcamento: &[(&'static str, String)]) {
    use axum::http::HeaderName;
    for (nome, valor) in orcamento {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(nome.as_bytes()), valor.parse()) {
            headers.insert(n, v);
        } else {
            tracing::warn!(header = nome, valor = %valor, "header de orçamento inválido, descartado");
        }
    }
}

fn resposta_de_cache(cached: &CachedResponse) -> Response {
    let mut resposta = Response::new(Body::from(cached.body.as_ref().clone()));
    *resposta.status_mut() = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK);
    if let Some(ct) = &cached.content_type {
        if let Ok(valor) = axum::http::HeaderValue::from_str(ct) {
            resposta.headers_mut().insert(header::CONTENT_TYPE, valor);
        }
    }
    resposta.headers_mut().insert(
        "X-Cache",
        HEADER_CACHE_STATUS
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("HIT")),
    );
    resposta
}
