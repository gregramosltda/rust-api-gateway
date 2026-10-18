//! Contratos de rate limit compartilhados entre backends e camada HTTP.
//!
//! Este módulo é deliberadamente livre de dependência de framework HTTP: a
//! decisão (permitido ou negado, com que folga) é um conceito de domínio, e o
//! mapeamento para cabeçalhos pertence à camada web.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Nomes de cabeçalho emitidos pelo gateway.
///
/// Usa o formato clássico (`RateLimit-Limit`, `RateLimit-Remaining`,
/// `RateLimit-Reset`), que é o que clientes, SDKs e proxies em produção já
/// reconhecem. A RFC 9333 fundiu tudo em um cabeçalho `RateLimit` único; o
/// formato clássico tem interoperabilidade muito maior.
pub const HEADER_LIMIT: &str = "RateLimit-Limit";
pub const HEADER_REMAINING: &str = "RateLimit-Remaining";
pub const HEADER_RESET: &str = "RateLimit-Reset";
pub const HEADER_RETRY_AFTER: &str = "Retry-After";

/// Resultado da consulta de orçamento para uma chave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RateLimitDecision {
    /// Ainda há orçamento. `remaining` já descontou esta requisição.
    Allowed {
        limit: u32,
        remaining: u32,
        /// Tempo até a janela deslizar o suficiente para liberar uma unidade.
        reset_after: Duration,
    },
    /// Orçamento esgotado. `retry_after` é o tempo mínimo a esperar.
    Rejected {
        limit: u32,
        remaining: u32,
        retry_after: Duration,
    },
}

impl RateLimitDecision {
    /// Constrói a decisão a partir dos valores crus do backend.
    pub fn from_parts(allowed: bool, limit: u32, remaining: u32, wait: Duration) -> Self {
        if allowed {
            Self::Allowed {
                limit,
                remaining,
                reset_after: wait,
            }
        } else {
            Self::Rejected {
                limit,
                remaining: 0,
                retry_after: wait,
            }
        }
    }

    /// `true` quando a requisição pode seguir.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    pub fn limit(&self) -> u32 {
        match self {
            Self::Allowed { limit, .. } | Self::Rejected { limit, .. } => *limit,
        }
    }

    pub fn remaining(&self) -> u32 {
        match self {
            Self::Allowed { remaining, .. } | Self::Rejected { remaining, .. } => *remaining,
        }
    }

    /// Segundos a reportar em `RateLimit-Reset` / `Retry-After`.
    /// Arredonda para cima porque esperar 4,2 s e tentar de novo em 4 s é negado.
    pub fn wait_seconds(&self) -> u64 {
        let secs = match self {
            Self::Allowed { reset_after, .. } => *reset_after,
            Self::Rejected { retry_after, .. } => *retry_after,
        };
        secs.as_secs()
            .saturating_add(u64::from(secs.subsec_nanos() > 0))
    }

    /// Pares (nome, valor) dos cabeçalhos a emitir.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![
            (HEADER_LIMIT, self.limit().to_string()),
            (HEADER_REMAINING, self.remaining().to_string()),
            (HEADER_RESET, self.wait_seconds().to_string()),
        ];
        if !self.is_allowed() {
            out.push((HEADER_RETRY_AFTER, self.wait_seconds().to_string()));
        }
        out
    }
}

/// Monta a chave de orçamento.
///
/// Combina identidade do cliente com a rota porque os dois eixos são
/// independentes: sem o cliente, um botão dispara o limite de todo mundo; sem
/// a rota, um cliente heavy ofunde rotas leves.
pub fn budget_key(client_id: &str, path: &str, prefix: &str) -> String {
    format!("{prefix}:{client_id}:{path}")
}

/// Normaliza um endereço para identificador de cliente.
///
/// Remove a porta e o prefixo de IPv6 para que a mesma origem não consuma
/// orçamento distinta em cada conexão.
pub fn client_identity(addr: &str) -> String {
    let trimmed = addr.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        // Forma IPv6: [::1]:8080 -> ::1
        return rest.split(']').next().unwrap_or(trimmed).to_owned();
    }
    match trimmed.rsplit_once(':') {
        Some((host, _port)) if host.contains('.') => host.to_owned(),
        // Sem ponto nem IPv6: o endereço não tem porta, usa como está.
        _ => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permitido_expoe_folga_de_um() {
        let decision = RateLimitDecision::from_parts(true, 100, 99, Duration::from_secs(5));
        assert!(decision.is_allowed());
        assert_eq!(decision.limit(), 100);
        assert_eq!(decision.remaining(), 99);
    }

    #[test]
    fn negado_zera_folga_e_marca_tentativa() {
        let decision = RateLimitDecision::from_parts(false, 50, 0, Duration::from_secs(12));
        assert!(!decision.is_allowed());
        assert_eq!(decision.remaining(), 0);
        assert_eq!(decision.wait_seconds(), 12);
    }

    #[test]
    fn espera_fracao_arredonda_para_cima() {
        let decision = RateLimitDecision::from_parts(false, 10, 0, Duration::from_millis(4200));
        assert_eq!(decision.wait_seconds(), 5);
    }

    #[test]
    fn espera_inteira_nao_sobe() {
        let decision = RateLimitDecision::from_parts(false, 10, 0, Duration::from_secs(4));
        assert_eq!(decision.wait_seconds(), 4);
    }

    #[test]
    fn headers_permitido_nao_tem_retry_after() {
        let decision = RateLimitDecision::from_parts(true, 100, 50, Duration::from_secs(10));
        let names: Vec<&str> = decision.headers().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec![HEADER_LIMIT, HEADER_REMAINING, HEADER_RESET]);
    }

    #[test]
    fn headers_negado_inclui_retry_after() {
        let decision = RateLimitDecision::from_parts(false, 100, 0, Duration::from_secs(30));
        let names: Vec<&str> = decision.headers().iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&HEADER_RETRY_AFTER));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn valores_de_header_sao_texto() {
        let decision = RateLimitDecision::from_parts(true, 7, 3, Duration::from_secs(9));
        let headers = decision.headers();
        let find = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(find(HEADER_LIMIT), "7");
        assert_eq!(find(HEADER_REMAINING), "3");
        assert_eq!(find(HEADER_RESET), "9");
    }

    #[test]
    fn chave_combina_cliente_rota_e_prefixo() {
        let key = budget_key("10.0.0.1", "/api/v1", "rl");
        assert_eq!(key, "rl:10.0.0.1:/api/v1");
    }

    #[test]
    fn identidade_remove_porta_ipv4() {
        assert_eq!(client_identity("192.168.0.10:54321"), "192.168.0.10");
    }

    #[test]
    fn identidade_trata_ipv6_com_porta() {
        assert_eq!(client_identity("[::1]:8080"), "::1");
        assert_eq!(client_identity("[2001:db8::1]:443"), "2001:db8::1");
    }

    #[test]
    fn identidade_trata_ativa_interno() {
        assert_eq!(client_identity("localhost"), "localhost");
    }

    #[test]
    fn serializa_com_tag_de_resultado() {
        let decision = RateLimitDecision::from_parts(true, 5, 4, Duration::from_secs(1));
        let json = serde_json::to_string(&decision).expect("serializar");
        assert!(json.contains("\"outcome\":\"allowed\""));

        let volta: RateLimitDecision = serde_json::from_str(&json).expect("desserializar");
        assert_eq!(volta, decision);
    }
}
