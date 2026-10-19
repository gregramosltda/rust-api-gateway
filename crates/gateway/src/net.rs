//! Utilidades de rede: endereço do cliente e corpo de resposta.
//!
//! Isoladas em módulo próprio porque têm regra de precedência entre
//! cabeçalhos, e essa regra precisa ser testada isoladamente.

use std::net::IpAddr;

use axum::http::header;
use axum::http::HeaderMap;
use gateway_common::client_identity;

/// Cabeçalho de IP único escrito pela borda da Cloudflare.
///
/// É o que o Render (e Fly, e Heroku) entregam: a requisição passa pela borda
/// antes de chegar ao processo, e a borda **reescreve** este valor. Como o
/// cliente não consegue escolhê-lo, ele é a fonte mais confiável disponível.
pub const HEADER_CF_CONNECTING_IP: &str = "cf-connecting-ip";
/// Cadeia `cliente, proxy1, proxy2`. O cliente pode prefixá-la, então só vale
/// quando um proxy confiável a reescreve.
pub const HEADER_X_FORWARDED_FOR: &str = "x-forwarded-for";
/// Alternativa de IP único usada por alguns proxies (nginx, por exemplo).
pub const HEADER_X_REAL_IP: &str = "x-real-ip";

/// Extrai o endereço do cliente da requisição.
///
/// Ordem de confiança, quando `confia_proxy` está ligado:
/// `CF-Connecting-IP` → `X-Forwarded-For` → `X-Real-IP` → IP do socket.
///
/// Sem a trava, nenhum cabeçalho é considerado. Ela não é zelo excessivo:
/// qualquer cliente escreve `X-Forwarded-For`, e sem a trava bastaria variar o
/// valor para ganhar um orçamento novo a cada requisição, anulando o rate
/// limit. Com a trava, assume-se que um proxy de confiança reescreveu o
/// cabeçalho antes de a requisição chegar aqui.
///
/// Cabeçalho ausente, vazio, não-UTF8 ou sem IP aproveitável não é erro: cai
/// para o próximo critério. Uma requisição nunca falha por causa disto.
pub fn client_ip(
    headers: &HeaderMap,
    socket: Option<std::net::SocketAddr>,
    confia_proxy: bool,
) -> String {
    if confia_proxy {
        if let Some(ip) = valor_unico(headers, HEADER_CF_CONNECTING_IP) {
            return ip;
        }
        if let Some(ip) = primeiro_da_cadeia(headers, HEADER_X_FORWARDED_FOR) {
            return ip;
        }
        if let Some(ip) = valor_unico(headers, HEADER_X_REAL_IP) {
            return ip;
        }
    }

    socket.map_or_else(|| "desconhecido".to_owned(), |s| s.ip().to_string())
}

/// Lê um cabeçalho que carrega um único endereço.
///
/// Devolve `None` para ausente, não-UTF8 ou vazio, e é isso que permite ao
/// chamador tentar o próximo critério sem `unwrap` nenhum.
fn valor_unico(headers: &HeaderMap, nome: &str) -> Option<String> {
    let valor = headers.get(nome)?.to_str().ok()?;
    let limpo = client_identity(valor);
    (!limpo.is_empty()).then_some(limpo)
}

/// Lê o primeiro salto de um cabeçalho em forma de cadeia.
///
/// O cliente real é o primeiro elemento. Um cliente malicioso pode prefixar
/// entradas falsas, e é exatamente por isso que este critério fica **depois**
/// do `CF-Connecting-IP`: só um proxy que reescreve a cadeia torna este valor
/// confiável.
fn primeiro_da_cadeia(headers: &HeaderMap, nome: &str) -> Option<String> {
    let valor = headers.get(nome)?.to_str().ok()?;
    let primeiro = valor.split(',').next()?;
    let limpo = client_identity(primeiro);
    (!limpo.is_empty()).then_some(limpo)
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
    fn cf_connecting_ip_tem_prioridade_sobre_xff() {
        // R1: a borda da Cloudflare reescreve o CF-Connecting-IP e não permite
        // que o cliente o escolha; o XFF o cliente prefixa. A ordem importa.
        let h = headers_com(&[
            ("cf-connecting-ip", "203.0.113.7"),
            ("x-forwarded-for", "1.2.3.4, 10.0.0.1"),
        ]);
        assert_eq!(
            client_ip(&h, Some(socket("10.1.2.3", 5)), true),
            "203.0.113.7"
        );
    }

    #[test]
    fn cf_connecting_ip_tem_prioridade_sobre_x_real_ip() {
        let h = headers_com(&[
            ("cf-connecting-ip", "203.0.113.7"),
            ("x-real-ip", "8.8.8.8"),
        ]);
        assert_eq!(client_ip(&h, None, true), "203.0.113.7");
    }

    #[test]
    fn cf_connecting_ip_e_usado_quando_nao_ha_xff() {
        let h = headers_com(&[("cf-connecting-ip", "203.0.113.8")]);
        assert_eq!(
            client_ip(&h, Some(socket("10.1.2.3", 5)), true),
            "203.0.113.8"
        );
    }

    #[test]
    fn cf_connecting_ip_e_ignorado_sem_proxy_confiavel() {
        // R2: o padrão é não confiar. Sem esta trava, o próprio atacante
        // escolheria o IP da chave de orçamento e o rate limit não valeria nada.
        let h = headers_com(&[("cf-connecting-ip", "203.0.113.9")]);
        assert_eq!(
            client_ip(&h, Some(socket("10.1.2.3", 5)), false),
            "10.1.2.3",
            "CF-Connecting-IP não deve ser considerado"
        );
    }

    #[test]
    fn cf_connecting_ip_ausente_usa_o_socket() {
        // R14: cabeçalho ausente não pode quebrar a requisição.
        assert_eq!(
            client_ip(&headers_com(&[]), Some(socket("10.1.2.3", 5)), true),
            "10.1.2.3"
        );
    }

    #[test]
    fn cf_connecting_ip_vazio_cai_para_o_proximo_criterio() {
        // R14: valor presente mas inútil não bloqueia a cadeia de fallback.
        let h = headers_com(&[("cf-connecting-ip", "   "), ("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_ip(&h, Some(socket("10.1.2.3", 5)), true), "1.2.3.4");
    }

    #[test]
    fn cf_connecting_ip_com_espacos_e_limpo() {
        let h = headers_com(&[("cf-connecting-ip", "  203.0.113.10  ")]);
        assert_eq!(client_ip(&h, None, true), "203.0.113.10");
    }

    #[test]
    fn cf_connecting_ip_ipv6_e_preservado_inteiro() {
        // O separador de porta não pode ser confundido com os dois-pontos do
        // IPv6: sem isso, `2001:db8::1` viraria `2001:db8:` e todo cliente
        // IPv6 cairia numa chave só.
        let h = headers_com(&[("cf-connecting-ip", "2001:db8::1")]);
        assert_eq!(client_ip(&h, None, true), "2001:db8::1");
    }

    #[test]
    fn cf_connecting_ip_vazio_e_sem_socket_nao_quebra() {
        let h = headers_com(&[("cf-connecting-ip", "")]);
        assert_eq!(client_ip(&h, None, true), "desconhecido");
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
