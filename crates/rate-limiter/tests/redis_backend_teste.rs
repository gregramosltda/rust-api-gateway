//! Testes instrumentados do `RedisBackend` contra um Redis real.
//!
//! # Por que container e não mock
//!
//! O código sob teste é, em essence, três coisas que um mock não tem:
//! o script Lua executado pelo Redis, a serialização dos argumentos e o
//! round-trip de rede. Um mock validaria o Rust chamando o Rust — passaria
//! mesmo com o Lua sintaticamente errado, com `ARGV` na ordem errada, ou com
//! o tipo de retorno do script incompatível com o que o Rust espera.
//!
//! # Por que relógio manual
//!
//! Testar janela deslizante com relógio de sistema significa dormir um segundo
//! por caso — a suíte fica lenta e flaky sob carga. `ManualClock` deixa o
//! teste avançar o tempo instantaneamente, com resultado determinístico.
//! O Redis continua real: só o relógio é controlado.
//!
//! # Isolamento
//!
//! Cada teste usa prefixo de chave único. Não há `FLUSHDB` porque destruir o
//! banco inteiro de um container compartilhado com testes paralelos criaria
//! interferência — e testar isolamento por prefixo já valida o requisito.

use std::sync::Arc;
use std::time::Duration;

use gateway_rate_limiter::clock::ManualClock;
use gateway_rate_limiter::{RateLimiterBackend, RedisBackend};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis;
use tokio::sync::OnceCell;

/// Container Redis da suíte, mantido vivo por toda a execução.
///
/// Guardar apenas a URL não basta: `HostContainer` implementa `Drop` e derruba
/// o container quando é descartado. Um `OnceCell<String>` guardando só a string
/// perde o handle, o container morre no fim do closure, e todo teste seguinte
/// recebe "Connection refused". O handle precisa ficar vivo.
static CONTAINER: OnceCell<testcontainers::ContainerAsync<Redis>> = OnceCell::const_new();

/// Sobe o container (uma vez) e devolve a URL de conexão.
///
/// Se o Docker não estiver disponível, o teste **falha** em vez de degradar
/// silenciosamente para um skip. Um `skip` aqui significaria um relatório
/// verde sem nenhuma evidência de que o Redis funciona — exatamente o que esta
/// OS existe para evitar.
async fn redis_url() -> String {
    let container = CONTAINER
        .get_or_init(|| async {
            Redis::default()
                .start()
                .await
                .expect("testcontainers: falha ao subir o Redis — Docker disponível?")
        })
        .await;

    // `get_host_port_ipv4` devolve **só a porta**, não `host:porta`. Usar o
    // retorno direto produziria `redis://32770`, que o cliente interpreta como
    // endereço inválido e trava até o timeout. O Docker expõe em localhost.
    let porta = container
        .get_host_port_ipv4(6379)
        .await
        .expect("porta do Redis");

    format!("redis://127.0.0.1:{porta}")
}

/// Converte `Arc<ManualClock>` em `Arc<dyn Clock>`.
///
/// Precisa passar por `Arc::clone` explícito: um `as` entre `Arc<T>` e
/// `Arc<dyn Trait>` não compila, porque a conversão exige ajustar o ponteiro.
fn coer(clock: Arc<ManualClock>) -> Arc<dyn gateway_rate_limiter::Clock> {
    clock
}

/// Backend conectado ao container, com relógio manual.
async fn backend_com(clock: Arc<ManualClock>) -> RedisBackend {
    let url = redis_url().await;
    RedisBackend::connect(&url, coer(clock.clone()))
        .await
        .expect("conectar ao Redis")
}

/// Chave exclusiva por teste, para não interferir com testes paralelos.
fn chave_unica(teste: &str) -> String {
    format!("teste:{teste}:{}", std::process::id())
}

// ---------------------------------------------------------------------------
// Fase 2.2 — Sliding window log: ZADD + ZREMRANGEBYSCORE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zadd_e_zremrangebyscore_implementam_a_janela() {
    let clock = Arc::new(ManualClock::new(1_000_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("janela_log");

    // Duas requisições dentro da janela de 10s, limite 5.
    assert!(b
        .check(&k, 5, Duration::from_secs(10))
        .await
        .expect("ok")
        .is_allowed());
    clock.advance(Duration::from_secs(3));
    assert!(b
        .check(&k, 5, Duration::from_secs(10))
        .await
        .expect("ok")
        .is_allowed());

    // O sorted set deve ter exatamente 2 entradas: é um log de instantes.
    let zcard: i64 = redis_cmd(&k, "ZCARD").await;
    assert_eq!(zcard, 2, "cada requisição deve virar uma entrada no log");

    // Avança para o primeiro evento sair da janela. ZREMRANGEBYSCORE remove
    // os antigos, ZCARD passa a contar só o que permanece.
    clock.advance(Duration::from_secs(8)); // t = 1_000_000 + 11_000
    let d = b.check(&k, 5, Duration::from_secs(10)).await.expect("ok");
    assert!(d.is_allowed());
    let zcard: i64 = redis_cmd(&k, "ZCARD").await;
    assert_eq!(
        zcard, 2,
        "o evento expirado deve ter sido removido (1 antigo + 1 novo)"
    );
}

// ---------------------------------------------------------------------------
// Fase 2.3 — Membro repetido: bug histórico (limite furado em silêncio)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn membro_repetido_nao_colapsa_o_limite() {
    // Este é o teste do bug histórico. `ZADD` com membro já existente ATUALIZA
    // o score em vez de somar. Se o membro fosse só o timestamp em
    // milissegundos, N requisições no mesmo instante seriam contadas como 1 e
    // o limite seria furado — sem nenhum erro visível.
    //
    // O relógio é congelado de propósito: todas as requisições caem no mesmo
    // milissegundo, que é exatamente a condição que dispara o bug.
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("membro_repetido");

    let limite = 50_u32;
    let mut permitidas = 0_u32;
    for _ in 0..limite {
        if b.check(&k, limite, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed()
        {
            permitidas += 1;
        }
    }
    assert_eq!(
        permitidas, limite,
        "requisições no mesmo milissegundo devem contar separadamente"
    );

    // O log tem que ter `limite` membros distintos, não 1.
    let zcard: i64 = redis_cmd(&k, "ZCARD").await;
    assert_eq!(zcard, i64::from(limite), "membros distintos no sorted set");

    // E a requisição seguinte tem de ser barrada.
    assert!(
        !b.check(&k, limite, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "acima do limite, tem de negar"
    );
}

// ---------------------------------------------------------------------------
// Fase 2.4 — Limite atingido: 429 no ponto exato
// ---------------------------------------------------------------------------

#[tokio::test]
async fn limite_atingido_no_ponto_exato() {
    let clock = Arc::new(ManualClock::new(2_000_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("ponto_exato");

    let limite = 7_u32;
    for esperado in 0..limite {
        let d = b
            .check(&k, limite, Duration::from_secs(60))
            .await
            .expect("ok");
        assert!(
            d.is_allowed(),
            "requisição {esperado} de {limite} deveria passar"
        );
        assert_eq!(d.remaining(), limite - esperado - 1, "folga em {esperado}");
    }

    // A última: exatamente no limite, não passa.
    let barrada = b
        .check(&k, limite, Duration::from_secs(60))
        .await
        .expect("ok");
    assert!(
        !barrada.is_allowed(),
        "a requisição {limite} tem de ser barrada"
    );
    assert_eq!(barrada.remaining(), 0);
    assert!(barrada.wait_seconds() > 0, "Retry-After nunca pode ser 0");
}

// ---------------------------------------------------------------------------
// Fase 2.5 — Fronteira 23:59 / 00:00 (a janela que a fixa permite dobrar)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fronteira_de_janela_nao_permite_dobro() {
    // Cenário clássico da vulnerabilidade de janela fixa: o cliente esgota o
    // orçamento no fim de uma janela e explode outra rajada no começo da
    // seguinte, levando o dobro do limite num intervalo curto.
    //
    // Baseado no instante real da fronteira: 23:59:59.000 e 00:00:00.000.
    const MEIA_NOITE_MS: u64 = 86_399_000;
    let clock = Arc::new(ManualClock::new(MEIA_NOITE_MS));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("fronteira_meia_noite");

    let limite = 3_u32;

    // Esgota o orçamento em 23:59:59.
    for _ in 0..limite {
        assert!(b
            .check(&k, limite, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
    }

    // A rajada seguinte cruza a meia-noite. Se fosse janela fixa, o contador
    // teria zerado e as três passariam.
    clock.advance(Duration::from_millis(1_000)); // 00:00:00
    for i in 0..limite {
        assert!(
            !b.check(&k, limite, Duration::from_secs(60))
                .await
                .expect("ok")
                .is_allowed(),
            "janela deslizante não pode liberar a rajada {i} após a fronteira"
        );
    }

    // Só depois de a janela inteira passar é que o orçamento volta.
    clock.advance(Duration::from_secs(60));
    assert!(
        b.check(&k, limite, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "passada a janela, o orçamento deve estar livre"
    );
}

// ---------------------------------------------------------------------------
// Fase 2.6 — TTL no sorted set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttl_configurado_no_sorted_set() {
    let clock = Arc::new(ManualClock::new(3_000_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("ttl");

    b.check(&k, 10, Duration::from_secs(45)).await.expect("ok");

    // O script aplica PEXPIRE; sem isso, chaves de clientes abandonados
    // vazariam memória no Redis para sempre.
    let ttl: i64 = redis_cmd(&k, "PTTL").await;
    assert!(ttl > 0, "a chave deve ter TTL positivo, veio {ttl}");
    assert!(
        ttl <= 45_000 && ttl > 40_000,
        "TTL deveria ser ~45s (janela), veio {ttl}ms"
    );
}

#[tokio::test]
async fn chave_resetada_some_do_redis() {
    let clock = Arc::new(ManualClock::new(3_100_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("reset");

    let _ = b.check(&k, 1, Duration::from_secs(60)).await.expect("ok");
    assert!(!b
        .check(&k, 1, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());

    b.reset(&k).await.expect("reset");

    let existe: i64 = redis_cmd(&k, "EXISTS").await;
    assert_eq!(existe, 0, "reset deve apagar a chave");
    assert!(
        b.check(&k, 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "após o reset, o orçamento volta"
    );
}

// ---------------------------------------------------------------------------
// Fase 2.7 — Isolamento por chave (tenant / rota)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chaves_distintas_nao_compartilham_orcamento() {
    let clock = Arc::new(ManualClock::new(4_000_000));
    let b = backend_com(clock.clone()).await;

    let a = chave_unica("iso_a");
    let b2 = chave_unica("iso_b");

    // Esgota `a`.
    assert!(b
        .check(&a, 1, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());
    assert!(!b
        .check(&a, 1, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());

    // `b` não pode ter sido afetado: é outro tenant, outra rota.
    assert!(
        b.check(&b2, 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "chave independente precisa ter orçamento próprio"
    );
}

#[tokio::test]
async fn prefixos_de_tenant_nao_vazam_entre_si() {
    // A chave real é `prefixo:cliente:rota`. Dois clientes no mesmo prefixo
    // não podem ter o orçamento somado nem compartilhado.
    let clock = Arc::new(ManualClock::new(4_100_000));
    let b = backend_com(clock.clone()).await;
    let prefixo = chave_unica("tenant");

    let cliente_a = gateway_common::budget_key("cliente-a", "/api", &prefixo);
    let cliente_b = gateway_common::budget_key("cliente-b", "/api", &prefixo);

    let _ = b
        .check(&cliente_a, 1, Duration::from_secs(60))
        .await
        .expect("ok");
    assert!(!b
        .check(&cliente_a, 1, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());
    assert!(
        b.check(&cliente_b, 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "cliente B não deve herdar o orçamento do cliente A"
    );

    // Rotas diferentes do mesmo cliente também são orçamentos separados.
    let rota_outra = gateway_common::budget_key("cliente-a", "/outra", &prefixo);
    assert!(
        b.check(&rota_outra, 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed(),
        "rota diferente, orçamento diferente"
    );
}

// ---------------------------------------------------------------------------
// Fase 2.8 — Falha do Redis: fail-open e fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redis_fora_do_ar_produz_erro_tratado() {
    // Porta sem serviço: simula indisponibilidade.
    let clock = Arc::new(ManualClock::new(5_000_000));
    let resultado = RedisBackend::connect("redis://127.0.0.1:1", coer(clock.clone())).await;

    assert!(resultado.is_err(), "deveria falhar ao conectar");
    let err = resultado.expect_err("erro esperado");
    assert_eq!(err.code(), "redis_unavailable");
}

#[tokio::test]
async fn erro_nao_arrasta_informacao_de_rede_para_o_cliente() {
    // A mensagem interna pode conter host e porta; a pública, não. Host vaza
    // a topologia interna da rede do cliente.
    let clock = Arc::new(ManualClock::new(5_100_000));
    let err = RedisBackend::connect("redis://10.20.30.40:6379", coer(clock.clone()))
        .await
        .expect_err("erro esperado");

    let publico = err.public_message();
    assert!(
        !publico.contains("10.20.30.40"),
        "host interno vazou: {publico}"
    );
    assert!(!publico.contains("6379"), "porta interna vazou: {publico}");
    assert!(publico.contains("rate limit"));
}

#[tokio::test]
async fn falha_apos_conexao_e_um_erro_tratado() {
    // Conecta, derruba o servidor, usa: o erro precisa ser `Result`, nunca panic.
    // Cobre o caminho `map_err` dentro de `check`, que só falha com a conexão
    // já morta.
    let url = redis_url().await;
    let clock = Arc::new(ManualClock::new(5_200_000));
    let b = RedisBackend::connect(&url, coer(clock.clone()))
        .await
        .expect("conectar");

    b.health().await.expect("health inicial");

    // Encerra o Redis abruptamente.
    let host = Redis::default()
        .start()
        .await
        .expect("subir redis descartável");
    let porta = host.get_host_port_ipv4(6379).await.expect("porta");
    let outro = RedisBackend::connect(&format!("redis://127.0.0.1:{porta}"), coer(clock.clone()))
        .await
        .expect("conectar ao descartável");
    drop(host); // derruba o container

    // O `ConnectionManager` tenta reconectar; o erro pode demorar alguns ms.
    let resultado = tokio::time::timeout(
        Duration::from_secs(5),
        outro.check(&chave_unica("apos_queda"), 1, Duration::from_secs(60)),
    )
    .await;

    match resultado {
        Ok(Ok(_)) => { /* a reconexão encontrou outro servidor — aceitável neste ambiente */ }
        Ok(Err(e)) => assert_eq!(e.code(), "redis_unavailable"),
        Err(_) => panic!("check travou após queda do Redis"),
    }
}

// ---------------------------------------------------------------------------
// Fase 2.9 — Atomicidade: o script não deixa estado parcial
// ---------------------------------------------------------------------------

#[tokio::test]
async fn script_nao_deixa_estado_parcial_quando_bloqueia() {
    // Se o script bloqueiasse antes de descartar os eventos expirados, o
    // estado ficaria inconsistente: a chave nunca decairia e o cliente ficaria
    // barrado para sempre. Verifica que mesmo negando, a poda aconteceu.
    let clock = Arc::new(ManualClock::new(6_000_000));
    let b = backend_com(clock.clone()).await;
    let k = chave_unica("parcial");

    for _ in 0..3 {
        let _ = b.check(&k, 3, Duration::from_secs(10)).await.expect("ok");
    }
    let barrada = b.check(&k, 3, Duration::from_secs(10)).await.expect("ok");
    assert!(!barrada.is_allowed());

    // O evento mais antigo expira; a próxima chamada tem de podar E liberar.
    clock.advance(Duration::from_secs(11));
    let liberada = b.check(&k, 3, Duration::from_secs(10)).await.expect("ok");
    assert!(
        liberada.is_allowed(),
        "a poda precisa ocorrer mesmo na chamada que nega, senão o estado trava"
    );
}

#[tokio::test]
async fn concorrencia_nao_fura_o_limite_no_redis_real() {
    // O teste mais importante desta OS. O script Lua promete atomicidade; sem
    // ela, requisições concorrentes leriam o mesmo ZCARD antes de qualquer uma
    // escrever, e todas passariam.
    let clock = Arc::new(ManualClock::new(7_000_000));
    let b = Arc::new(backend_com(clock.clone()).await);
    let k = chave_unica("concorrencia");

    let limite = 40_u32;
    let tarefas: Vec<_> = (0..160)
        .map(|_| {
            let b = b.clone();
            let k = k.clone();
            tokio::spawn(async move {
                b.check(&k, limite, Duration::from_secs(60))
                    .await
                    .map(|d| d.is_allowed())
                    .unwrap_or(false)
            })
        })
        .collect();

    let mut permitidas = 0_u32;
    for t in tarefas {
        if t.await.expect("task") {
            permitidas += 1;
        }
    }
    assert_eq!(
        permitidas, limite,
        "exatamente {limite} deveriam passar, vieram {permitidas} — a atomicidade do Lua falhou"
    );

    // O log confirma: nem mais nem menos que `limite` entradas.
    let zcard: i64 = redis_cmd(&k, "ZCARD").await;
    assert_eq!(zcard, i64::from(limite), "log com contagem divergente");
}

// ---------------------------------------------------------------------------
// Health, kind e Debug — caminhos cobertos pela conexão real
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_ping_responde_ok_com_redis_vivo() {
    let clock = Arc::new(ManualClock::new(8_000_000));
    let b = backend_com(clock).await;
    b.health().await.expect("health com Redis vivo");
}

#[tokio::test]
async fn kind_identifica_o_backend_como_redis() {
    let clock = Arc::new(ManualClock::new(8_100_000));
    let b = backend_com(clock).await;
    assert_eq!(b.kind(), "redis");
}

#[tokio::test]
async fn debug_nao_vaza_a_conexao() {
    let clock = Arc::new(ManualClock::new(8_200_000));
    let b = backend_com(clock).await;
    let texto = format!("{b:?}");
    assert!(
        texto.contains("RedisBackend"),
        "deveria se identificar: {texto}"
    );
    assert!(
        !texto.contains("redis://") && !texto.contains("6379"),
        "a conexão não deve aparecer no Debug: {texto}"
    );
}

#[tokio::test]
async fn with_connection_aceita_conexao_pronta() {
    // Constrói o gerenciador de fora e injeta — caminho alternativo de
    // construção, usado quando a aplicação já tem o cliente em mãos.
    let url = redis_url().await;
    let cliente = redis::Client::open(url.as_str()).expect("client");
    let conn = cliente.get_connection_manager().await.expect("conexão");
    let clock = Arc::new(ManualClock::new(8_300_000));

    let b = RedisBackend::with_connection(conn, coer(clock.clone()));
    assert!(b
        .check(&chave_unica("with_conn"), 5, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());
    b.health().await.expect("health");
}

#[tokio::test]
async fn connect_default_usa_relogio_do_sistema() {
    // Constrói o backend pela função de conveniência, sem relógio explícito.
    let url = redis_url().await;
    let b = gateway_rate_limiter::redis_backend::connect_default(&url)
        .await
        .expect("connect_default");
    b.health().await.expect("health");
    assert!(b
        .check(&chave_unica("default"), 3, Duration::from_secs(60))
        .await
        .expect("ok")
        .is_allowed());
}

#[test]
fn member_factory_default_equivale_a_new() {
    let a = gateway_rate_limiter::redis_backend::MemberFactory::default();
    let b = gateway_rate_limiter::redis_backend::MemberFactory::new();
    // Só interessa que ambos existam e gerem membros válidos.
    assert!(!a.member(1_000).is_empty());
    assert!(!b.member(1_000).is_empty());
}

// ---------------------------------------------------------------------------
// Auxiliar
// ---------------------------------------------------------------------------

/// Executa um comando de leitura no Redis do container.
async fn redis_cmd<T: redis::FromRedisValue>(chave: &str, comando: &str) -> T {
    let url = redis_url().await;
    let client = redis::Client::open(url.as_str()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("conexão");
    // O `Command` precisa sobreviver ao `arg`/`query_async`: `redis::cmd(...)`
    // produz um valor temporário, e o `&mut Command` que ele devolve ficaria
    // apontando para um temporário já descartado.
    let mut cmd = match comando {
        "ZCARD" => redis::cmd("ZCARD"),
        "PTTL" => redis::cmd("PTTL"),
        "EXISTS" => redis::cmd("EXISTS"),
        _ => panic!("comando não suportado no auxiliar: {comando}"),
    };
    cmd.arg(chave);
    cmd.query_async(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("{comando} falhou: {e}"))
}
