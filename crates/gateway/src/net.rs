//! Utilidades de rede: endereço do cliente e corpo de resposta.
//!
//! Isoladas em módulo próprio porque têm regra de precedência entre
//! cabeçalhos, e essa regra precisa ser testada isoladamente.

use std::net::IpAddr;

use axum::http::header;
use axum::http::HeaderMap;
use gateway_common::client_identity;

/// Extrai o endereço do cliente da requisição.
///
/// Ordem de confiança: `X-Forwarded-For` só é considerado quando `confia_proxy`
/// está ligado. Sem essa trava, qualquer cliente escreveria o cabeçalho e
/// burlaria o rate limit, reaproveitando o próprio IP a cada requisição. Com a
/// trava, assume-se que só um proxy de confiança preencheu o cabeçalho.
pub fn client_ip(
    headers: &HeaderMap,
    socket: Option<std::net::SocketAddr>,
    confia_proxy: bool,
) -> String {
    if confia_proxy {
        if let Some(valor) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            // `XFF` é uma cadeia "cliente, proxy1, proxy2"; o cliente real é o
            // primeiro. Um cliente malicioso pode prefixar entradas falsas,
            // então o proxy confiável deve reescrever o cabeçalho.
            if let Some(primeiro) = valor.split(',').next() {
                let limpo = client_identity(primeiro);
                if !limpo.is_empty() {
                    return limpo;
                }
            }
        }
        if let Some(valor) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let limpo = client_identity(valor);
            if !limpo.is_empty() {
                return limpo;
            }
        }
    }

    socket.map_or_else(|| "desconhecido".to_owned(), |s| s.ip().to_string())
}

/// Converte `IpAddr` em string estável para usar como chave.
pub fn ip_to_key(ip: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(parseado) => parseado.to_string(),
        Err(_) => ip.to_owned(),
    }
}

/// Nome do cabeçalho de status do cache.
pub const HEADER_CACHE_STATUS: &str = "X-Cache";
pub const HEADER_CACHE_KEY: &str = "X-Cache-Key";
pub const HEADER_UPSTREAM: &str = "X-Upstream";
pub const HEADER_REQUEST_ID: &str = "X-Request-Id";

/// Anexa cabeçalhos de diagnóstico à resposta.
///
/// Erros são ignorados de propósito: um cabeçalho que não pode ser escrito não
/// deve derrubar a resposta que já foi servida com sucesso.
pub fn anexar_diagnostico(
    headers: &mut HeaderMap,
    cache_status: Option<&str>,
    cache_key: Option<&str>,
    upstream: Option<&str>,
    request_id: Option<&str>,
) {
    if let Some(valor) = cache_status {
        if let Ok(nome) = header::HeaderName::from_bytes(HEADER_CACHE_STATUS.as_bytes()) {
            if let Ok(v) = valor.parse() {
                headers.insert(nome, v);
            }
        }
    }
    if let Some(valor) = cache_key {
        if let Ok(nome) = header::HeaderName::from_bytes(HEADER_CACHE_KEY.as_bytes()) {
            if let Ok(v) = valor.parse() {
                headers.insert(nome, v);
            }
        }
    }
    if let Some(valor) = upstream {
        if let Ok(nome) = header::HeaderName::from_bytes(HEADER_UPSTREAM.as_bytes()) {
            if let Ok(v) = valor.parse() {
                headers.insert(nome, v);
            }
        }
    }
    if let Some(valor) = request_id {
        if let Ok(nome) = header::HeaderName::from_bytes(HEADER_REQUEST_ID.as_bytes()) {
            if let Ok(v) = valor.parse() {
                headers.insert(nome, v);
            }
        }
    }
}

/// Gera um identificador de requisição.
///
/// Sem um ID de correlação, o log do gateway e o do upstream não podem ser
/// unidos, e um incidente vira adivinhação de horário.
pub fn gerar_request_id(counter: u64, millis: u64) -> String {
    format!("{millis:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn socket(ip: &str, port: u16) -> SocketAddr {
        format!("{ip}:{port}").parse().expect("endereço válido")
    }

    fn headers_com(pares: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pares {
            if let (Ok(nome), Ok(valor)) = (
                header::HeaderName::from_bytes(k.as_bytes()),
                header::HeaderValue::from_str(v),
            ) {
                h.insert(nome, valor);
            }
        }
        h
    }

    #[test]
    fn usa_ip_do_socket_quando_nao_ha_proxy() {
        let h = headers_com(&[]);
        let ip = client_ip(&h, Some(socket("10.1.2.3", 5555)), false);
        assert_eq!(ip, "10.1.2.3");
    }

    #[test]
    fn ignora_xff_quando_nao_confia_no_proxy() {
        // Sem a trava, o cliente burlaria o rate limit a cada requisição.
        let h = headers_com(&[("x-forwarded-for", "1.2.3.4")]);
        let ip = client_ip(&h, Some(socket("10.1.2.3", 5555)), false);
        assert_eq!(ip, "10.1.2.3", "XFF não deve ser considerado");
    }

    #[test]
    fn usa_primeiro_salto_do_xff_com_proxy_confiavel() {
        let h = headers_com(&[("x-forwarded-for", "1.2.3.4, 10.0.0.1, 10.0.0.2")]);
        let ip = client_ip(&h, Some(socket("10.1.2.3", 5555)), true);
        assert_eq!(ip, "1.2.3.4");
    }

    #[test]
    fn usa_x_real_ip_como_alternativa() {
        let h = headers_com(&[("x-real-ip", "8.8.8.8")]);
        assert_eq!(client_ip(&h, Some(socket("10.0.0.1", 1)), true), "8.8.8.8");
    }

    #[test]
    fn prefere_xff_sobre_x_real_ip() {
        let h = headers_com(&[("x-forwarded-for", "1.1.1.1"), ("x-real-ip", "2.2.2.2")]);
        assert_eq!(client_ip(&h, None, true), "1.1.1.1");
    }

    #[test]
    fn cai_para_socket_quando_cabecalhos_sao_invalidos() {
        let h = headers_com(&[("x-forwarded-for", "   ")]);
        let ip = client_ip(&h, Some(socket("10.0.0.9", 22)), true);
        assert_eq!(ip, "10.0.0.9");
    }

    #[test]
    fn sem_socket_e_sem_header_devolve_desconhecido() {
        assert_eq!(client_ip(&headers_com(&[]), None, true), "desconhecido");
    }

    #[test]
    fn xff_com_espacos_e_removido() {
        let h = headers_com(&[("x-forwarded-for", "  9.9.9.9 , 10.0.0.1")]);
        assert_eq!(client_ip(&h, None, true), "9.9.9.9");
    }

    #[test]
    fn normaliza_ipv6_em_chave() {
        assert_eq!(ip_to_key("::1"), "::1");
        assert_eq!(ip_to_key("2001:db8::1"), "2001:db8::1");
    }

    #[test]
    fn chave_de_ip_invalido_cai_para_texto() {
        assert_eq!(ip_to_key("nao-e-ip"), "nao-e-ip");
    }

    #[test]
    fn anexa_cabecalhos_de_diagnostico() {
        let mut h = HeaderMap::new();
        anexar_diagnostico(
            &mut h,
            Some("HIT"),
            Some("GET /a"),
            Some("http://up"),
            Some("abc"),
        );
        assert_eq!(h.get(HEADER_CACHE_STATUS).expect("status"), "HIT");
        assert_eq!(h.get(HEADER_CACHE_KEY).expect("chave"), "GET /a");
        assert_eq!(h.get(HEADER_UPSTREAM).expect("upstream"), "http://up");
        assert_eq!(h.get(HEADER_REQUEST_ID).expect("id"), "abc");
    }

    #[test]
    fn cabecalhos_ausentes_simplesmente_nao_sao_escritos() {
        let mut h = HeaderMap::new();
        anexar_diagnostico(&mut h, None, None, None, None);
        assert!(h.get(HEADER_CACHE_STATUS).is_none());
    }

    #[test]
    fn valor_invalido_nao_derruba_a_resposta() {
        let mut h = HeaderMap::new();
        // Contém quebra de linha: inválido em cabeçalho HTTP.
        anexar_diagnostico(&mut h, Some("com\nquebra"), None, None, None);
        assert!(h.get(HEADER_CACHE_STATUS).is_none());
    }

    #[test]
    fn request_id_une_contador_e_instante() {
        assert_eq!(gerar_request_id(5, 1_000), "3e8-5");
    }

    #[test]
    fn request_ids_sao_distintos_para_chamadas_seguidas() {
        let a = gerar_request_id(1, 1_000);
        let b = gerar_request_id(2, 1_000);
        assert_ne!(a, b);
    }
}
