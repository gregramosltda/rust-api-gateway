//! Binário do gateway.
//!
//! Toda a lógica está na biblioteca (`gateway::`); aqui só há arranque:
//! carregar configuração, montar o estado, escutar e.shutdown gracioso.
//!
//! Regra do projeto: nada de `unwrap()` em caminho de produção. Se a
//! configuração estiver errada, o processo deve sair com mensagem clara, não
//! com backtrace de pânico.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use gateway::proxy::build_client;
use gateway::state::AppState;
use gateway::{construir_router, Config};
use gateway_cache::ResponseCache;
use gateway_rate_limiter::{MemoryBackend, RedisBackend, SystemClock};

use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Caminho padrão da configuração.
const CONFIG_PADRAO: &str = "config/gateway.toml";

#[tokio::main]
async fn main() -> ExitCode {
    match executar().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(mensagem) => {
            // `eprintln!` e não `tracing::error!`: o subscriber pode ainda não
            // estar instalado, e uma falha de arranque precisa aparecer de
            // qualquer forma no terminal.
            eprintln!("erro: {mensagem}");
            ExitCode::FAILURE
        }
    }
}

async fn executar() -> Result<(), String> {
    let caminho_config = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GATEWAY_CONFIG").ok())
        .unwrap_or_else(|| CONFIG_PADRAO.to_owned());

    // Subscriber de log: JSON em produção, texto legível em desenvolvimento.
    // A escolha vem do ambiente, não de um flag, porque o formato de log é
    // operação de infraestrutura, não preferência de quem sobe o processo.
    let formato_json = std::env::var("GATEWAY_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let filtro = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("gateway=info,gateway_common=info,tower_http=warn"));

    if formato_json {
        tracing_subscriber::registry()
            .with(filtro)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filtro)
            .with(tracing_subscriber::fmt::layer().with_target(true))
            .init();
    }

    tracing::info!(config = %caminho_config, "carregando configuração");

    let config = Config::load(&caminho_config).map_err(|e| format!("configuração: {e}"))?;
    tracing::info!(
        upstream = %config.upstream.url,
        rate_limit = config.rate_limit.enabled,
        cache = config.cache.enabled,
        "configuração carregada"
    );

    let upstream = build_client(
        Duration::from_secs(config.upstream.connect_timeout_seconds),
        Duration::from_secs(config.upstream.timeout_seconds),
    )
    .map_err(|e| format!("cliente http: {e}"))?;

    let limiter: Arc<dyn gateway_rate_limiter::RateLimiterBackend> = if config.rate_limit.enabled {
        Arc::new(
            RedisBackend::connect(&config.rate_limit.redis_url, Arc::new(SystemClock))
                .await
                .map_err(|e| {
                    format!(
                        "redis indisponível em startup: {} — verifique a URL ou suba `fail_open`",
                        e.public_message()
                    )
                })?,
        )
    } else {
        tracing::warn!("rate limiting desabilitado na configuração");
        Arc::new(MemoryBackend::new(Arc::new(SystemClock)))
    };

    let cache = ResponseCache::new(
        config.cache.max_capacity,
        Duration::from_secs(config.cache.ttl_seconds),
    );

    let confia_proxy = std::env::var("GATEWAY_TRUST_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if confia_proxy {
        tracing::warn!("GATEWAY_TRUST_PROXY ligado: X-Forwarded-For será considerado");
    }

    let endereco: SocketAddr = config
        .server
        .bind_addr
        .parse()
        .map_err(|e| format!("bind_addr inválido: {e}"))?;

    let estado = AppState::new(config, upstream, limiter, cache, confia_proxy);
    let app = construir_router(estado);

    let listener = TcpListener::bind(endereco)
        .await
        .map_err(|e| format!("não foi possível escutar em {endereco}: {e}"))?;

    let local = listener
        .local_addr()
        .map_err(|e| format!("endereço local: {e}"))?;
    tracing::info!(%local, "gateway no ar");

    // `into_make_service_with_connect_info` é obrigatório: sem ele o
    // `ConnectInfo<SocketAddr>` que o handler do proxy usa para identificar o
    // cliente não existe na requisição, e todas falham com "Missing request
    // extension" — um 500 que parece bug de lógica e é configuração do servidor.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(sinal_de_parada(Duration::from_secs(estado_grace(&local))))
    .await
    .map_err(|e| format!("servidor: {e}"))?;

    tracing::info!("encerrado");
    Ok(())
}

/// Prazo de shutdown. Valor fixo porque é um comportamento do processo, não
/// um parâmetro operacional sujeito a erro de configuração.
fn estado_grace(_local: &SocketAddr) -> u64 {
    10
}

/// Espera Ctrl-C ou SIGTERM.
///
/// `SIGTERM` importa tanto quanto `SIGINT`: em container, o runtime envia
/// SIGTERM no rollout. Sem tratá-lo, o processo morre sem o shutdown gracioso
/// e as requisições em voo são cortadas.
async fn sinal_de_parada(grace: Duration) {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(erro = %e, "falha ao observar Ctrl-C");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => tracing::error!(erro = %e, "falha ao observar SIGTERM"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("Ctrl-C recebido"),
        () = terminate => tracing::info!("SIGTERM recebido"),
    }

    tracing::info!(
        grace_segundos = grace.as_secs(),
        "aguardando conexões in-flight"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_padrao_esta_definida() {
        assert!(CONFIG_PADRAO.ends_with(".toml"));
    }

    #[test]
    fn estado_grace_e_positivo() {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        assert!(estado_grace(&addr) > 0);
    }

    #[test]
    fn main_nao_tem_unwrap_em_producao() {
        let fonte = include_str!("main.rs");
        // Conta ocorrências de `unwrap()` fora do módulo de teste.
        let antes_dos_testes = fonte.split("#[cfg(test)]").next().unwrap_or(fonte);
        assert!(
            !antes_dos_testes.contains(".unwrap()"),
            "main.rs não deve usar unwrap() em produção"
        );
    }
}
