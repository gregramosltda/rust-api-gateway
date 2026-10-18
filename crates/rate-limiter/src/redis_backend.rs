//! Backend de rate limiting sobre Redis.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gateway_common::{GatewayError, RateLimitDecision};
use redis::aio::ConnectionManager;
use tokio::sync::Mutex;

use crate::clock::{Clock, SystemClock};
use crate::lua::{RESET_SCRIPT, SLIDING_WINDOW_SCRIPT};
use crate::RateLimiterBackend;

/// Resposta bruta do script Lua: `[permitido, restante, espera_ms]`.
type ScriptReply = (i64, i64, i64);

/// Identificador de processo para diferenciar membros no sorted set.
///
/// Sem ele, duas réplicas do gateway poderiam gerar membros idênticos no mesmo
/// milissegundo e uma requisição seria sobrescrita pela outra — o limite
/// contaria duas como uma.
fn new_instance_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(0))
        .unwrap_or(0);
    nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Gera membros únicos para o sorted set do Redis.
/// Existe como tipo próprio, e não como método solto, porque é a parte com
/// invariante non-trivial (`ZADD` sobrescreve em vez de somar) e por isso
/// precisa de teste direto, sem Redis no meio.
#[derive(Debug)]
pub struct MemberFactory {
    instance: u64,
    sequence: AtomicU64,
}

impl MemberFactory {
    pub fn new() -> Self {
        Self {
            instance: new_instance_id(),
            sequence: AtomicU64::new(0),
        }
    }

    /// Constrói com identificador fixo (determinismo em teste).
    pub fn with_instance(instance: u64) -> Self {
        Self {
            instance,
            sequence: AtomicU64::new(0),
        }
    }

    /// Membro único para o instante informado.
    ///
    /// A combinação instância + instante + contador é o que impede que duas
    /// requisições no mesmo milissegundo colidam — e colisão aqui significa
    /// limite furado sem nenhum sintoma visível.
    pub fn member(&self, now_millis: u64) -> String {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}-{}", self.instance, now_millis, seq)
    }
}

impl Default for MemberFactory {
    fn default() -> Self {
        Self::new()
    }
}

/// Rate limiter distribuído. O orçamento é compartilhado por todas as
/// instâncias do gateway que apontam para o mesmo prefixo de chave.
pub struct RedisBackend {
    script: redis::Script,
    reset_script: redis::Script,
    /// O `ConnectionManager` multiplexa comandos, mas `Script::invoke_async`
    /// exige `&mut`, então o acesso é serializado por um lock.
    conn: Mutex<ConnectionManager>,
    clock: Arc<dyn Clock>,
    members: MemberFactory,
}

/// Não expõe a conexão: ela carrega credenciais que não devem aparecer em log
/// de erro nem em `unwrap` de teste.
impl std::fmt::Debug for RedisBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBackend")
            .field("script", &"<lua sliding window>")
            .finish_non_exhaustive()
    }
}

impl RedisBackend {
    /// Conecta ao Redis e prepara os scripts.
    pub async fn connect(redis_url: &str, clock: Arc<dyn Clock>) -> Result<Self, GatewayError> {
        let client =
            redis::Client::open(redis_url).map_err(|e| GatewayError::Redis(e.to_string()))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| GatewayError::Redis(e.to_string()))?;

        let instance = new_instance_id();
        tracing::info!(redis_url, instance, "rate limiter conectado ao redis");

        Ok(Self {
            script: redis::Script::new(SLIDING_WINDOW_SCRIPT),
            reset_script: redis::Script::new(RESET_SCRIPT),
            conn: Mutex::new(conn),
            clock,
            members: MemberFactory::new(),
        })
    }

    /// Constrói um backend com conexão já estabelecida (usado em testes).
    pub fn with_connection(conn: ConnectionManager, clock: Arc<dyn Clock>) -> Self {
        Self {
            script: redis::Script::new(SLIDING_WINDOW_SCRIPT),
            reset_script: redis::Script::new(RESET_SCRIPT),
            conn: Mutex::new(conn),
            clock,
            members: MemberFactory::new(),
        }
    }
}

/// Conversão do erro do cliente Redis para o erro do gateway fica explícita
/// (`.map_err`) em cada ponto de uso: implementar `From<redis::RedisError>` para
/// `GatewayError` violaria a regra de órfãos, porque ambos os tipos vêm de
/// crates externos.

#[async_trait]
impl RateLimiterBackend for RedisBackend {
    async fn check(
        &self,
        key: &str,
        limit: u32,
        window: Duration,
    ) -> Result<RateLimitDecision, GatewayError> {
        let now = self.clock.now_millis();
        let window_millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        let member = self.members.member(now);

        let mut guard = self.conn.lock().await;
        let conn: &mut ConnectionManager = &mut guard;
        let reply: ScriptReply = self
            .script
            .key(key)
            .arg(now)
            .arg(window_millis)
            .arg(limit)
            .arg(member)
            .invoke_async(conn)
            .await
            .map_err(|e| GatewayError::Redis(e.to_string()))?;
        drop(guard);

        let (allowed, remaining, wait_ms) = reply;
        // O script devolve inteiros Lua (que são float64). `as i64` de um valor
        // já inteiro é seguro; a alternativa é divisão por 1, que também
        // funciona e deixa a intenção explícita.
        let wait = Duration::from_millis(wait_ms.max(0) as u64);

        Ok(RateLimitDecision::from_parts(
            allowed == 1,
            limit,
            remaining.max(0) as u32,
            wait,
        ))
    }

    async fn reset(&self, key: &str) -> Result<(), GatewayError> {
        let mut guard = self.conn.lock().await;
        let conn: &mut ConnectionManager = &mut guard;
        let _: i64 = self
            .reset_script
            .key(key)
            .invoke_async(conn)
            .await
            .map_err(|e| GatewayError::Redis(e.to_string()))?;
        Ok(())
    }

    async fn health(&self) -> Result<(), GatewayError> {
        let mut guard = self.conn.lock().await;
        let conn: &mut ConnectionManager = &mut guard;
        let reply: redis::RedisResult<String> = redis::cmd("PING").query_async(conn).await;
        reply.map_err(|e| GatewayError::Redis(e.to_string()))?;
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "redis"
    }
}

/// Constrói backend Redis com relógio do sistema.
pub async fn connect_default(redis_url: &str) -> Result<RedisBackend, GatewayError> {
    RedisBackend::connect(redis_url, Arc::new(SystemClock)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membros_no_mesmo_milissegundo_sao_distintos() {
        // Regressão: com membro = timestamp, `ZADD` sobrescreveria em vez de
        // somar e o limite seria furado exatamente nos picos de tráfego.
        let factory = MemberFactory::with_instance(7);
        let mut vistos = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(vistos.insert(factory.member(1_234)), "membro duplicado");
        }
        assert_eq!(vistos.len(), 10_000);
    }

    #[test]
    fn membro_tem_instancia_instante_e_sequencia() {
        let factory = MemberFactory::with_instance(7);
        assert_eq!(factory.member(100), "7-100-0");
        assert_eq!(factory.member(100), "7-100-1");
        assert_eq!(factory.member(200), "7-200-2");
    }

    #[test]
    fn instancias_distintas_nao_colidem() {
        let a = MemberFactory::with_instance(1);
        let b = MemberFactory::with_instance(2);
        assert_ne!(a.member(500), b.member(500));
    }

    #[test]
    fn fabricas_reais_recebem_ids_distintos() {
        let a = MemberFactory::new();
        let b = MemberFactory::new();
        assert_ne!(a.member(0), b.member(0));
    }

    #[tokio::test]
    async fn url_invalida_falha_com_erro_tratado() {
        let err = RedisBackend::connect("nao-e-uma-url", Arc::new(SystemClock))
            .await
            .expect_err("deveria falhar");
        assert_eq!(err.code(), "redis_unavailable");
    }

    #[tokio::test]
    async fn redis_fora_do_ar_falha_com_erro_tratado() {
        let err = RedisBackend::connect("redis://127.0.0.1:1", Arc::new(SystemClock))
            .await
            .expect_err("deveria falhar");
        assert_eq!(err.code(), "redis_unavailable");
        assert!(err.public_message().contains("rate limit"));
    }

    #[tokio::test]
    async fn erro_nao_vaza_host_interno_para_o_cliente() {
        let err = RedisBackend::connect("redis://10.0.0.5:6379", Arc::new(SystemClock))
            .await
            .expect_err("deveria falhar");
        let publico = err.public_message();
        assert!(
            !publico.contains("10.0.0.5"),
            "host interno vazou: {publico}"
        );
    }
}
