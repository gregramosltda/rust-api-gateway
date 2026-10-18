//! Rate limiting distribuído por janela deslizante.
//!
//! # Por que janela deslizante e não janela fixa
//!
//! Na janela fixa (o que a maioria dos rate limiters ingênuos usa), um cliente
//! pode emitir o dobro do limite: enche a janela às 23:59 e enche de novo às
//! 00:00. A janela deslizante elimina a fronteira — o orçamento é sempre
//! "quantas requisições nos últimos N milissegundos".
//!
//! # Atomicidade
//!
//! A verificação e o registro precisam ser uma operação só. Entre "contei 99
//! eventos" e "adicionei o meu" existe uma janela em que 10 requisições
//! concorrentes veem 99 e todas passam. Por isso a operação inteira vive num
//! script Lua, que o Redis executa atomicamente.

pub mod clock;
pub mod lua;
pub mod memory;
pub mod redis_backend;

use std::time::Duration;

use async_trait::async_trait;
use gateway_common::{GatewayError, RateLimitDecision};

pub use clock::{Clock, ManualClock, SystemClock};
pub use memory::MemoryBackend;
pub use redis_backend::RedisBackend;

/// Contador de orçamento para uma chave.
///
/// Abstrair o backend é o que permite testar o comportamento de limites sem
/// subir um Redis, e o que permite trocar Redis por outra implementação sem
/// tocar na camada HTTP.
#[async_trait]
pub trait RateLimiterBackend: Send + Sync {
    /// Consome uma unidade do orçamento de `key`.
    ///
    /// `limit` e `window` vêm da configuração efetiva da rota — um único
    /// backend atende limites diferentes por rota.
    async fn check(
        &self,
        key: &str,
        limit: u32,
        window: Duration,
    ) -> Result<RateLimitDecision, GatewayError>;

    /// Zera o orçamento de uma chave.
    async fn reset(&self, key: &str) -> Result<(), GatewayError>;

    /// Verifica se o backend está operacional.
    async fn health(&self) -> Result<(), GatewayError>;

    /// Nome do backend, para log de inicialização.
    fn kind(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Fluxo completo sobre backend em memória: N requisições, limite M.
    async fn consume_until_rejected(
        backend: &Arc<MemoryBackend>,
        key: &str,
        limit: u32,
        window: Duration,
    ) -> Vec<RateLimitDecision> {
        let mut out = Vec::new();
        for _ in 0..(limit + 2) {
            match backend.check(key, limit, window).await {
                Ok(decision) => out.push(decision),
                Err(_) => break,
            }
        }
        out
    }

    fn backend() -> Arc<MemoryBackend> {
        Arc::new(MemoryBackend::new(Arc::new(ManualClock::new(1_000))))
    }

    #[tokio::test]
    async fn permite_ate_o_limite_e_depois_nega() {
        let b = backend();
        let decisions = consume_until_rejected(&b, "k", 3, Duration::from_secs(60)).await;
        assert_eq!(decisions.len(), 5, "esperava limite+2 respostas");
        assert!(decisions[0].is_allowed());
        assert!(decisions[1].is_allowed());
        assert!(decisions[2].is_allowed());
        assert!(!decisions[3].is_allowed());
        assert!(!decisions[4].is_allowed());
    }

    #[tokio::test]
    async fn folga_decrece_a_cada_requisicao() {
        let b = backend();
        let first = b.check("k", 10, Duration::from_secs(60)).await.expect("ok");
        let second = b.check("k", 10, Duration::from_secs(60)).await.expect("ok");
        assert_eq!(first.remaining(), 9);
        assert_eq!(second.remaining(), 8);
    }

    #[tokio::test]
    async fn chaves_independentes_nao_compartilham_orcamento() {
        let b = backend();
        let _ = b
            .check("cliente-a", 1, Duration::from_secs(60))
            .await
            .expect("ok");
        let denied = b
            .check("cliente-a", 1, Duration::from_secs(60))
            .await
            .expect("ok");
        let other = b
            .check("cliente-b", 1, Duration::from_secs(60))
            .await
            .expect("ok");
        assert!(!denied.is_allowed());
        assert!(other.is_allowed(), "outra chave não deve ser afetada");
    }

    #[tokio::test]
    async fn janela_expirada_libera_orcamento() {
        let clock = Arc::new(ManualClock::new(1_000));
        let b = Arc::new(MemoryBackend::new(clock.clone()));
        let window = Duration::from_secs(10);

        assert!(b.check("k", 1, window).await.expect("ok").is_allowed());
        assert!(!b.check("k", 1, window).await.expect("ok").is_allowed());

        // Evento em t=1000, janela [now-10000, now]. Expira quando
        // now - 10000 > 1000, ou seja, now > 11000.
        clock.advance(Duration::from_secs(10)); // t = 11000: borda exata, ainda vale
        assert!(
            !b.check("k", 1, window).await.expect("ok").is_allowed(),
            "na borda exata o evento ainda está dentro da janela"
        );

        clock.advance(Duration::from_millis(1)); // t = 11001: expirou
        assert!(
            b.check("k", 1, window).await.expect("ok").is_allowed(),
            "passado o instante da janela, o orçamento deve estar livre"
        );
    }

    #[tokio::test]
    async fn relogio_em_zero_nao_perde_o_evento() {
        // Regressão: com o relógio em 0 e o filtro `event > cutoff`, o evento
        // inserido (score 0) era descartado na chamada seguinte, e o limite
        // nunca era atingido — o limiter ficava permanentemente aberto.
        let clock = Arc::new(ManualClock::new(0));
        let b = Arc::new(MemoryBackend::new(clock.clone()));

        let mut rem = Vec::new();
        for _ in 0..4 {
            let d = b.check("k", 3, Duration::from_secs(60)).await.expect("ok");
            rem.push((d.is_allowed(), d.remaining()));
        }

        assert_eq!(rem[0], (true, 2));
        assert_eq!(rem[1], (true, 1));
        assert_eq!(rem[2], (true, 0));
        assert!(!rem[3].0, "com o relógio em zero, o limite ainda vale");
    }

    #[tokio::test]
    async fn requisicoes_na_mesma_hora_nao_colapsam() {
        // Regressão: se o membro do sorted set fosse só o timestamp em ms,
        // requisições no mesmo milissegundo se sobrescreveriam e o limite
        // seria furado silenciosamente.
        let clock = Arc::new(ManualClock::new(5_000));
        let b = Arc::new(MemoryBackend::new(clock.clone()));
        let limit = 100;

        let mut allowed = 0_u32;
        for _ in 0..limit {
            if b.check("k", limit, Duration::from_secs(60))
                .await
                .expect("ok")
                .is_allowed()
            {
                allowed += 1;
            }
        }
        assert_eq!(allowed, limit, "todas as requisições devem contar");
        assert!(!b
            .check("k", limit, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
    }

    #[tokio::test]
    async fn reset_zera_o_orcamento() {
        let b = backend();
        let _ = b.check("k", 1, Duration::from_secs(60)).await.expect("ok");
        assert!(!b
            .check("k", 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
        b.reset("k").await.expect("reset");
        assert!(b
            .check("k", 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
    }

    #[tokio::test]
    async fn health_do_backend_em_memoria_sempre_ok() {
        let b = backend();
        assert!(b.health().await.is_ok());
        assert_eq!(b.kind(), "memoria");
    }

    #[tokio::test]
    async fn limite_zero_nega_sempre() {
        let b = backend();
        let decision = b.check("k", 0, Duration::from_secs(60)).await.expect("ok");
        assert!(!decision.is_allowed());
    }

    #[tokio::test]
    async fn retry_after_cresce_conforme_a_janela_desliza() {
        // Eventos em t=1000; janela de 30s. A espera é 31000 - now, ou seja,
        // cai linearmente conforme o tempo avança, até zero em t=31000.
        let clock = Arc::new(ManualClock::new(1_000));
        let b = Arc::new(MemoryBackend::new(clock.clone()));
        let window = Duration::from_secs(30);
        let limit = 2;

        let _ = b.check("k", limit, window).await.expect("ok");
        let _ = b.check("k", limit, window).await.expect("ok");

        let denied = b.check("k", limit, window).await.expect("ok");
        assert!(!denied.is_allowed());
        let first_wait = denied.wait_seconds();
        assert_eq!(first_wait, 30);

        clock.advance(Duration::from_secs(10));
        let denied_again = b.check("k", limit, window).await.expect("ok");
        assert!(
            denied_again.wait_seconds() < first_wait,
            "espera deveria cair de {first_wait}s para menos, veio {}",
            denied_again.wait_seconds()
        );
    }
}
