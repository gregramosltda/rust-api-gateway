//! Tipos de erro compartilhados.
//!
//! Regra: todo erro que pode chegar ao cliente é um valor, nunca um `panic!`.
//! Os erros do gateway carregam um código HTTP e um código estável de máquina,
//! para que o cliente possa reagir sem fazer parsing de texto humano.

/// Erro de infraestrutura (Redis, upstream, configuração).
///
/// Distinguído de erro de requisição porque a resposta HTTP muda: falha de
/// infraestrutura é 502/503 e indica problema nosso, não do cliente.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("falha ao acessar o Redis: {0}")]
    Redis(String),

    #[error("upstream respondeu com status {status}")]
    UpstreamStatus { status: u16 },

    #[error("não foi possível falar com o upstream: {0}")]
    UpstreamTransport(String),

    #[error("corpo da requisição excedeu o limite de {limit} bytes")]
    PayloadTooLarge { limit: usize },

    #[error("nenhuma regra de rota corresponde a {0}")]
    NoRoute(String),

    #[error("configuração inválida: {0}")]
    Config(String),
}

impl GatewayError {
    /// Status HTTP apropriado para este erro.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::PayloadTooLarge { .. } => 413,
            Self::NoRoute(_) => 404,
            // Redis indisponível e upstream inalcançável são, do ponto de vista
            // do cliente, o mesmo: a origem não conseguiu ser servida.
            Self::Redis(_) | Self::UpstreamTransport(_) => 502,
            Self::UpstreamStatus { status } => *status,
            Self::Config(_) => 500,
        }
    }

    /// Código estável, adequado para correlação em logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Redis(_) => "redis_unavailable",
            Self::UpstreamStatus { .. } => "upstream_status",
            Self::UpstreamTransport(_) => "upstream_unreachable",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::NoRoute(_) => "no_route",
            Self::Config(_) => "config_invalid",
        }
    }

    /// Corpo JSON seguro para resposta ao cliente.
    ///
    /// Não inclui a mensagem interna: ela pode conter hostnames, portas e
    /// endereços que são informação da rede interna do cliente.
    pub fn public_message(&self) -> &'static str {
        match self {
            Self::Redis(_) => "backend de rate limit indisponível",
            Self::UpstreamStatus { .. } => "upstream rejeitou a requisição",
            Self::UpstreamTransport(_) => "upstream inacessível",
            Self::PayloadTooLarge { .. } => "corpo da requisição muito grande",
            Self::NoRoute(_) => "rota não encontrada",
            Self::Config(_) => "erro de configuração",
        }
    }
}

/// Corpo de erro devolvido ao cliente.
#[derive(Debug, serde::Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, serde::Serialize)]
pub struct ErrorDetail {
    /// Código estável para tratamento programático.
    pub code: &'static str,
    /// Mensagem segura para exibição.
    pub message: &'static str,
}

impl From<&GatewayError> for ErrorBody {
    fn from(err: &GatewayError) -> Self {
        Self {
            error: ErrorDetail {
                code: err.code(),
                message: err.public_message(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_mapeia_para_502() {
        let err = GatewayError::Redis("conexão recusada".to_owned());
        assert_eq!(err.status_code(), 502);
        assert_eq!(err.code(), "redis_unavailable");
    }

    #[test]
    fn status_do_upstream_e_preservado() {
        let err = GatewayError::UpstreamStatus { status: 503 };
        assert_eq!(err.status_code(), 503);
        assert_eq!(err.code(), "upstream_status");
    }

    #[test]
    fn payload_grande_mapeia_para_413() {
        let err = GatewayError::PayloadTooLarge { limit: 1024 };
        assert_eq!(err.status_code(), 413);
    }

    #[test]
    fn rota_desconhecida_mapeia_para_404() {
        let err = GatewayError::NoRoute("/inexistente".to_owned());
        assert_eq!(err.status_code(), 404);
    }

    #[test]
    fn mensagem_publica_nao_vaza_detalhe_interno() {
        let err = GatewayError::Redis("redis://10.0.0.5:6379 recusou AUTH".to_owned());
        let body = ErrorBody::from(&err);
        let serializado = serde_json::to_string(&body).expect("serializar");
        assert!(!serializado.contains("10.0.0.5"));
        assert!(!serializado.contains("AUTH"));
        assert!(serializado.contains("redis_unavailable"));
    }

    #[test]
    fn corpo_de_erro_tem_forma_esperada() {
        let err = GatewayError::Config("bind_addr inválido".to_owned());
        let json = serde_json::to_string(&ErrorBody::from(&err)).expect("serializar");
        assert!(json.contains("\"code\":\"config_invalid\""));
        assert!(json.contains("\"message\":"));
    }
}
