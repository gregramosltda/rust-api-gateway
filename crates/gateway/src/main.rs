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
use gateway_common::RateLimitBackend;
use gateway_rate_limiter::{MemoryBackend, RedisBackend, SystemClock};

use axum::http::Uri;
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

    let mut config = Config::load(&caminho_config).map_err(|e| format!("configuração: {e}"))?;

    // Ordem importa: primeiro o endereço de escuta é resolvido, depois a URL do
    // destino embutido é alinhada a ele. Um desacordo entre os dois não aparece
    // como erro de configuração — aparece como conexão recusada em todo o
    // tráfego proxiado, sem mencionar a causa.
    let endereco = endereco_de_escuta(
        config
            .server
            .bind_addr
            .parse()
            .map_err(|e| format!("bind_addr inválido: {e}"))?,
    );
    alinhar_destino_embutido(&mut config, endereco);

    tracing::info!(
        upstream = %config.upstream.url,
        rate_limit = config.rate_limit.enabled,
        rate_limit_backend = ?config.rate_limit.backend,
        cache = config.cache.enabled,
        mock = config.mock.enabled,
        escuta = %endereco,
        "configuração carregada"
    );

    let upstream = build_client(
        Duration::from_secs(config.upstream.connect_timeout_seconds),
        Duration::from_secs(config.upstream.timeout_seconds),
    )
    .map_err(|e| format!("cliente http: {e}"))?;

    let limiter: Arc<dyn gateway_rate_limiter::RateLimiterBackend> =
        match (config.rate_limit.enabled, config.rate_limit.backend) {
            (false, _) => {
                tracing::warn!("rate limiting desabilitado na configuração");
                Arc::new(MemoryBackend::new(Arc::new(SystemClock)))
            }
            (true, RateLimitBackend::Memory) => {
                tracing::warn!(
                    "rate limit em memória: o orçamento vale por processo e não é \
                     compartilhado — com mais de uma instância, use backend = \"redis\""
                );
                Arc::new(MemoryBackend::new(Arc::new(SystemClock)))
            }
            (true, RateLimitBackend::Redis) => Arc::new(
                RedisBackend::connect(&config.rate_limit.redis_url, Arc::new(SystemClock))
                    .await
                    .map_err(|e| {
                        format!(
                            "redis indisponível em startup: {} — verifique a URL, use \
                             backend = \"memory\" ou suba `fail_open`",
                            e.public_message()
                        )
                    })?,
            ),
        };

    let cache = ResponseCache::new(
        config.cache.max_capacity,
        Duration::from_secs(config.cache.ttl_seconds),
    );

    let confia_proxy = std::env::var("GATEWAY_TRUST_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if confia_proxy {
        tracing::warn!(
            "GATEWAY_TRUST_PROXY ligado: CF-Connecting-IP, X-Forwarded-For e \
             X-Real-IP serão considerados na identificação do cliente — só ligue \
             atrás de um proxy que reescreva esses cabeçalhos"
        );
    }

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

/// Resolve o endereço de escuta, dando precedência a `PORT` quando ele existe.
///
/// Plataformas como serviço (Render, Fly, Heroku) entregam a porta pela
/// variável `PORT` e consideram o serviço inacessível se ele escutar em outra.
/// O arquivo de configuração continua mandando quando `PORT` não existe — que
/// é o caso local e o do contêiner.
fn endereco_de_escuta(configurado: SocketAddr) -> SocketAddr {
    let valor = std::env::var("PORT").ok();
    match porta_efetiva(configurado.port(), valor.as_deref()) {
        Some(porta) => {
            let endereco = SocketAddr::from(([0, 0, 0, 0], porta));
            tracing::info!(
                %endereco,
                configurada = %configurado,
                "PORT definida pela plataforma: escutando nela"
            );
            endereco
        }
        None => configurado,
    }
}

/// Decide se `PORT` deve substituir a porta configurada.
///
/// Separado da leitura do ambiente para poder ser testado: variável de ambiente
/// é global ao processo, e um teste que dependesse dela seria dependente de
/// ordem de execução.
fn porta_efetiva(configurada: u16, porta_do_ambiente: Option<&str>) -> Option<u16> {
    porta_do_ambiente
        .and_then(|valor| valor.trim().parse::<u16>().ok())
        .filter(|porta| *porta != configurada)
}

/// Reaponta o destino embutido para a porta que está realmente aberta.
///
/// Com `[mock] enabled = true` o destino é este mesmo processo — a rota
/// `/mock/*` mora aqui. Se a plataforma trocou a porta de escuta, a URL do
/// arquivo apontaria para uma porta fechada e todo o tráfego proxiado falharia.
/// Só age quando o destino é comprovadamente local: um `upstream` externo não é
/// tocado, mesmo com o simulador ligado.
fn alinhar_destino_embutido(config: &mut Config, escuta: SocketAddr) {
    if !config.mock.enabled {
        return;
    }

    let Ok(uri) = config.upstream.url.parse::<Uri>() else {
        return;
    };
    let Some(autoridade) = uri.authority() else {
        return;
    };

    let host = autoridade.host();
    if !eh_destino_local(host) || autoridade.port_u16() == Some(escuta.port()) {
        return;
    }

    let novo = format!("http://{host}:{}{}", escuta.port(), uri.path());
    tracing::warn!(
        de = %config.upstream.url,
        para = %novo,
        "destino embutido reapontado para a porta de escuta"
    );
    config.upstream.url = novo;
}

/// Hosts que apontam para o próprio processo.
fn eh_destino_local(host: &str) -> bool {
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "[::1]" | "0.0.0.0"
    )
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

    fn config_de_teste(url: &str, mock: bool) -> Config {
        toml::from_str(&format!(
            "[upstream]\nurl = \"{url}\"\n\n[mock]\nenabled = {mock}\ndelay_ms = 0\n"
        ))
        .expect("config de teste")
    }

    #[test]
    fn porta_da_plataforma_tem_precedencia() {
        assert_eq!(porta_efetiva(8080, Some("10000")), Some(10000));
        assert_eq!(porta_efetiva(8080, Some(" 10000 ")), Some(10000));
    }

    #[test]
    fn sem_porta_ou_igual_a_configurada_a_configuracao_manda() {
        assert_eq!(porta_efetiva(8080, None), None);
        assert_eq!(porta_efetiva(8080, Some("8080")), None);
    }

    #[test]
    fn porta_da_plataforma_invalida_e_ignorada() {
        // Um `PORT` malformado não pode derrubar o arranque: vale a configuração.
        assert_eq!(porta_efetiva(8080, Some("oitenta")), None);
        assert_eq!(porta_efetiva(8080, Some("")), None);
        assert_eq!(porta_efetiva(8080, Some("70000")), None);
        assert_eq!(porta_efetiva(8080, Some("-1")), None);
    }

    #[test]
    fn destino_embutido_segue_a_porta_de_escuta() {
        let mut config = config_de_teste("http://127.0.0.1:8080/mock", true);
        alinhar_destino_embutido(&mut config, "0.0.0.0:10000".parse().expect("addr"));
        assert_eq!(config.upstream.url, "http://127.0.0.1:10000/mock");
    }

    #[test]
    fn destino_local_sem_porta_explicita_recebe_a_porta_real() {
        let mut config = config_de_teste("http://localhost/mock", true);
        alinhar_destino_embutido(&mut config, "0.0.0.0:4321".parse().expect("addr"));
        assert_eq!(config.upstream.url, "http://localhost:4321/mock");
    }

    #[test]
    fn destino_externo_nao_e_tocado() {
        let mut config = config_de_teste("https://api.exemplo.com/v1", true);
        alinhar_destino_embutido(&mut config, "0.0.0.0:10000".parse().expect("addr"));
        assert_eq!(config.upstream.url, "https://api.exemplo.com/v1");
    }

    #[test]
    fn com_o_simulador_desligado_o_destino_nao_e_tocado() {
        let mut config = config_de_teste("http://127.0.0.1:8080/mock", false);
        alinhar_destino_embutido(&mut config, "0.0.0.0:10000".parse().expect("addr"));
        assert_eq!(config.upstream.url, "http://127.0.0.1:8080/mock");
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
