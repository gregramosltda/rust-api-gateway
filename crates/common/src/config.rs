//! Configuração do gateway, carregada de arquivo TOML.
//!
//! Regra de projeto: nada de valor fixo no código. Todo parâmetro operacional
//! — porta, upstream, limites, TTLs — vem do arquivo de configuração e é
//! validado na carga, com mensagem de erro que aponta o caminho do problema.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Raiz da configuração do gateway.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    /// Regras por rota. A regra de prefixo mais longo vence.
    #[serde(default, rename = "routes")]
    pub routes: Vec<RouteRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endereço de escuta. Padrão `127.0.0.1` por segurança: exposição em rede
    /// deve ser uma decisão explícita, não o padrão silencioso.
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    /// Prazo para conexões in-flight terminarem no shutdown gracioso.
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind(),
            shutdown_grace_seconds: default_shutdown_grace(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// URL base do serviço de destino.
    pub url: String,
    #[serde(default = "default_upstream_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            timeout_seconds: default_upstream_timeout(),
            connect_timeout_seconds: default_connect_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    /// URL do Redis. Necessária quando `enabled = true`.
    #[serde(default)]
    pub redis_url: String,
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    #[serde(default = "default_limit")]
    pub default_limit: u32,
    #[serde(default = "default_window")]
    pub default_window_seconds: u64,
    /// Quando o Redis está indisponível: `true` libera o tráfego (disponibilidade
    /// preferida), `false` bloqueia (proteção preferida). Não existe escolha
    /// neutra — por isso é explícita na configuração.
    #[serde(default = "default_true")]
    pub fail_open: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            redis_url: String::new(),
            key_prefix: default_key_prefix(),
            default_limit: default_limit(),
            default_window_seconds: default_window(),
            fail_open: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cache_capacity")]
    pub max_capacity: u64,
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
    /// Métodos elegíveis a cache. Só verbos seguros fazem sentido.
    #[serde(default = "default_cacheable_methods")]
    pub cacheable_methods: Vec<String>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_capacity: default_cache_capacity(),
            ttl_seconds: default_cache_ttl(),
            cacheable_methods: default_cacheable_methods(),
        }
    }
}

/// Ajuste específico para um prefixo de rota.
///
/// Todos os campos são opcionais de propósito: uma rota pode querer alterar só
/// o limite, só a janela, ou só o cache. Obrigar a declarar `limit` só para
/// desligar o cache de uma rota seria exigir um valor que não tem uso ali.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    /// Prefixo testado contra o caminho da requisição.
    pub path: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub window_seconds: Option<u64>,
    /// Cache pode ser desligado por rota, sem afetar as demais.
    #[serde(default)]
    pub cache: Option<bool>,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_shutdown_grace() -> u64 {
    10
}
fn default_upstream_timeout() -> u64 {
    30
}
fn default_connect_timeout() -> u64 {
    5
}
fn default_key_prefix() -> String {
    "ratelimit".to_owned()
}
fn default_limit() -> u32 {
    100
}
fn default_window() -> u64 {
    60
}
fn default_true() -> bool {
    true
}
fn default_cache_capacity() -> u64 {
    10_000
}
fn default_cache_ttl() -> u64 {
    30
}
fn default_cacheable_methods() -> Vec<String> {
    vec!["GET".to_owned(), "HEAD".to_owned()]
}

/// Erros de carga e validação da configuração.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("não foi possível ler o arquivo de configuração {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("TOML inválido em {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("campo inválido: {0}")]
    Invalid(String),
}

impl Config {
    /// Carrega e valida a configuração de um arquivo TOML.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let display = path_ref.display().to_string();

        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Io {
            path: display.clone(),
            source,
        })?;

        let config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: display,
            source,
        })?;

        config.validate()?;
        Ok(config)
    }

    /// Valida invariantes que o desserializador não consegue expressar.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.bind_addr.parse::<SocketAddr>().is_err() {
            return Err(ConfigError::Invalid(format!(
                "server.bind_addr deve ser host:porta — recebido {:?}",
                self.server.bind_addr
            )));
        }

        if self.upstream.url.is_empty() {
            return Err(ConfigError::Invalid(
                "upstream.url é obrigatório".to_owned(),
            ));
        }
        if !(self.upstream.url.starts_with("http://") || self.upstream.url.starts_with("https://"))
        {
            return Err(ConfigError::Invalid(format!(
                "upstream.url deve começar com http:// ou https:// — recebido {:?}",
                self.upstream.url
            )));
        }

        if self.rate_limit.enabled && self.rate_limit.redis_url.is_empty() {
            return Err(ConfigError::Invalid(
                "rate_limit.redis_url é obrigatório quando rate_limit.enabled = true".to_owned(),
            ));
        }
        if self.rate_limit.default_limit == 0 {
            return Err(ConfigError::Invalid(
                "rate_limit.default_limit deve ser maior que zero".to_owned(),
            ));
        }
        if self.rate_limit.default_window_seconds == 0 {
            return Err(ConfigError::Invalid(
                "rate_limit.default_window_seconds deve ser maior que zero".to_owned(),
            ));
        }

        if self.cache.enabled {
            if self.cache.max_capacity == 0 {
                return Err(ConfigError::Invalid(
                    "cache.max_capacity deve ser maior que zero".to_owned(),
                ));
            }
            if self.cache.ttl_seconds == 0 {
                return Err(ConfigError::Invalid(
                    "cache.ttl_seconds deve ser maior que zero".to_owned(),
                ));
            }
            // Normaliza em caixa alta para aceitar "get" e "GET" como o mesmo
            // método. Dedup evita reportar o mesmo erro duas vezes.
            let seen: HashSet<String> = self
                .cache
                .cacheable_methods
                .iter()
                .map(|m| m.to_ascii_uppercase())
                .collect();
            for method in &seen {
                if method != "GET" && method != "HEAD" {
                    return Err(ConfigError::Invalid(format!(
                        "cache.cacheable_methods aceita apenas GET e HEAD — recebido {method}"
                    )));
                }
            }
        }

        let mut paths = HashSet::new();
        for rule in &self.routes {
            if rule.limit == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "rota {:?}: limit deve ser maior que zero",
                    rule.path
                )));
            }
            if rule.window_seconds == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "rota {:?}: window_seconds deve ser maior que zero",
                    rule.path
                )));
            }
            if !rule.path.starts_with('/') {
                return Err(ConfigError::Invalid(format!(
                    "rota {:?}: path deve começar com /",
                    rule.path
                )));
            }
            if !paths.insert(rule.path.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "rota duplicada: {:?}",
                    rule.path
                )));
            }
        }

        Ok(())
    }

    /// Regra que casa com `path`: vence o prefixo mais longo.
    pub fn rule_for(&self, path: &str) -> Option<&RouteRule> {
        self.routes
            .iter()
            .filter(|rule| path.starts_with(&rule.path))
            .max_by_key(|rule| rule.path.len())
    }

    /// Limite efetivo para um caminho.
    pub fn limit_for(&self, path: &str) -> (u32, Duration) {
        let rule = self.rule_for(path);
        let limit = rule
            .and_then(|r| r.limit)
            .unwrap_or(self.rate_limit.default_limit);
        let window = rule
            .and_then(|r| r.window_seconds)
            .unwrap_or(self.rate_limit.default_window_seconds);
        (limit, Duration::from_secs(window))
    }

    /// Cache habilitado para um caminho, considerando override por rota.
    pub fn cache_enabled_for(&self, path: &str) -> bool {
        if !self.cache.enabled {
            return false;
        }
        // Sem override na rota, o cache vale — e o cache global já foi
        // verificado acima.
        self.rule_for(path)
            .and_then(|rule| rule.cache)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_toml() -> String {
        r#"
[server]
bind_addr = "0.0.0.0:9999"

[upstream]
url = "http://localhost:9000"

[rate_limit]
enabled = true
redis_url = "redis://127.0.0.1:6379"
default_limit = 42
default_window_seconds = 7
"#
        .to_owned()
    }

    #[test]
    fn defaults_sao_validos() {
        let config = Config::default();
        // upstream.url vazio torna o default inválido de propósito.
        assert!(config.validate().is_err());
    }

    #[test]
    fn carrega_toml_completo() {
        let config: Config = toml::from_str(&base_toml()).expect("parse");
        assert_eq!(config.server.bind_addr, "0.0.0.0:9999");
        assert_eq!(config.rate_limit.default_limit, 42);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn defaults_aplicam_quando_ausentes() {
        let config: Config = toml::from_str(
            r#"
[upstream]
url = "https://api.exemplo.com"
"#,
        )
        .expect("parse");
        assert_eq!(config.server.bind_addr, "127.0.0.1:8080");
        assert_eq!(config.server.shutdown_grace_seconds, 10);
        assert_eq!(config.upstream.timeout_seconds, 30);
        assert!(!config.rate_limit.enabled);
        assert!(!config.cache.enabled);
    }

    #[test]
    fn rejeita_bind_addr_invalido() {
        let raw = base_toml().replace("\"0.0.0.0:9999\"", "\"nao-e-endereco\"");
        let config: Config = toml::from_str(&raw).expect("parse");
        let err = config.validate().expect_err("deveria rejeitar");
        assert!(matches!(err, ConfigError::Invalid(ref m) if m.contains("bind_addr")));
    }

    #[test]
    fn rejeita_upstream_sem_esquema() {
        let raw = base_toml().replace("http://localhost:9000", "localhost:9000");
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejeita_limit_zero() {
        let raw = base_toml().replace("default_limit = 42", "default_limit = 0");
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejeita_metodo_nao_seguro_no_cache() {
        let raw = format!(
            "{}\n[cache]\nenabled = true\ncacheable_methods = [\"POST\"]\n",
            base_toml()
        );
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejeita_campo_desconhecido() {
        let raw = format!("{}\nchave_inexistente = 1\n", base_toml());
        assert!(toml::from_str::<Config>(&raw).is_err());
    }

    #[test]
    fn rejeita_rota_duplicada() {
        let raw = format!(
            "{}\n[[routes]]\npath = \"/api\"\nlimit = 5\n[[routes]]\npath = \"/api\"\nlimit = 9\n",
            base_toml()
        );
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejeita_rota_sem_barra() {
        let raw = format!("{}\n[[routes]]\npath = \"api\"\nlimit = 5\n", base_toml());
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn prefixo_mais_longo_vence() {
        let raw = format!(
            "{}\n[[routes]]\npath = \"/api\"\nlimit = 10\n[[routes]]\npath = \"/api/v1\"\nlimit = 3\n",
            base_toml()
        );
        let config: Config = toml::from_str(&raw).expect("parse");
        assert_eq!(config.limit_for("/api/v1/users").0, 3);
        assert_eq!(config.limit_for("/api/v2/users").0, 10);
        assert_eq!(config.limit_for("/outro").0, 42);
    }

    #[test]
    fn janela_por_rota_sobrescreve_padrao() {
        let raw = format!(
            "{}\n[[routes]]\npath = \"/api\"\nlimit = 10\nwindow_seconds = 30\n",
            base_toml()
        );
        let config: Config = toml::from_str(&raw).expect("parse");
        assert_eq!(config.limit_for("/api/x").1, Duration::from_secs(30));
        assert_eq!(config.limit_for("/outro").1, Duration::from_secs(7));
    }

    #[test]
    fn cache_desligado_por_rota() {
        let raw = format!(
            "{}\n[cache]\nenabled = true\n[[routes]]\npath = \"/privado\"\ncache = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&raw).expect("parse");
        assert!(config.validate().is_ok());
        assert!(!config.cache_enabled_for("/privado/x"));
        assert!(config.cache_enabled_for("/publico/x"));
    }

    #[test]
    fn arquivo_inexistente_reporta_erro_com_caminho() {
        let err = Config::load("/nao/existe/config.toml").expect_err("deveria falhar");
        assert!(matches!(err, ConfigError::Io { .. }));
        assert!(err.to_string().contains("/nao/existe/config.toml"));
    }

    #[test]
    fn toml_invalido_reporta_caminho() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("c.toml");
        std::fs::write(&path, "isto nao e toml = = =").expect("write");
        let err = Config::load(&path).expect_err("deveria falhar");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn carrega_arquivo_real_do_disco() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gateway.toml");
        std::fs::write(&path, base_toml()).expect("write");
        let config = Config::load(&path).expect("deveria carregar");
        assert_eq!(config.rate_limit.default_limit, 42);
    }
}
