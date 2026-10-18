//! Backend de rate limiting em memória.
//!
//! # Para que serve
//!
//! Três usos legítimos:
//!
//! 1. **Testes** — exercita o comportamento de janela deslizante sem subir
//!    Redis, o que mantém a suíte rápida e determinística.
//! 2. **Desenvolvimento** — permite rodar o gateway sem infraestruturaextra.
//! 3. **Fallback controlado** — quando o gateway roda em processo único e o
//!    Redis é opcional.
//!
//! # O que ele não faz
//!
//! Não é compartilhado entre instâncias. Dois processos com backends em memória
//! dão orçamento dobrado ao mesmo cliente. Para produção distribuída, use
//! [`crate::redis_backend::RedisBackend`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gateway_common::{GatewayError, RateLimitDecision};
use tokio::sync::Mutex;

use crate::clock::Clock;
use crate::RateLimiterBackend;

/// Registro de instantes de requisição, por chave.
#[derive(Debug, Default)]
struct Bucket {
    /// Instantes em milissegundos, mantidos em ordem crescente.
    events: Vec<u64>,
}

/// Rate limiter local, protected by a single lock.
pub struct MemoryBackend {
    buckets: Mutex<HashMap<String, Bucket>>,
    clock: Arc<dyn Clock>,
}

impl MemoryBackend {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Quantas chaves têm orçamento registrado.
    pub async fn tracked_keys(&self) -> usize {
        self.buckets.lock().await.len()
    }
}

#[async_trait]
impl RateLimiterBackend for MemoryBackend {
    async fn check(
        &self,
        key: &str,
        limit: u32,
        window: Duration,
    ) -> Result<RateLimitDecision, GatewayError> {
        if limit == 0 {
            // Janela vazia nunca autoriza. Decidir aqui evita entrar no
            // bookkeeping com um limite sem sentido.
            return Ok(RateLimitDecision::from_parts(
                false,
                0,
                0,
                Duration::from_secs(0),
            ));
        }

        let now = self.clock.now_millis();
        let window_millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now.saturating_sub(window_millis);

        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry(key.to_owned()).or_default();

        // Descarta o que saiu da janela. `retain` preserva a ordem, que é
        // invariante do algoritmo de janela deslizante.
        //
        // `>=` e não `>`: o instante exato da borda ainda está dentro da janela
        // — a janela é [now - window, now]. Com `>`, um evento no instante 0
        // seria descartado por `0 > 0`, e o orçamento nunca acumular, deixando o
        // limiter permanentemente aberto para quem o relógio estiver em zero.
        bucket.events.retain(|event| *event >= cutoff);

        let count = bucket.events.len() as u32;

        // Tempo até o evento mais antigo deixar a janela — é quando uma
        // unidade de orçamento volta a existir.
        let wait = bucket
            .events
            .first()
            .map(|oldest| {
                let libera_em = oldest.saturating_add(window_millis);
                libera_em.saturating_sub(now).max(1)
            })
            .unwrap_or(0);

        if count < limit {
            bucket.events.push(now);
            tracing::trace!(key, count, limit, "orçamento consumido");
            Ok(RateLimitDecision::from_parts(
                true,
                limit,
                limit - count - 1,
                Duration::from_millis(wait),
            ))
        } else {
            tracing::debug!(key, count, limit, "orçamento esgotado");
            Ok(RateLimitDecision::from_parts(
                false,
                limit,
                0,
                Duration::from_millis(wait),
            ))
        }
    }

    async fn reset(&self, key: &str) -> Result<(), GatewayError> {
        self.buckets.lock().await.remove(key);
        Ok(())
    }

    async fn health(&self) -> Result<(), GatewayError> {
        // Backend local não tem dependência externa: falha só se o lock
        // estiver envenenado, o que significaria panic em outra thread.
        let buckets = self.buckets.lock().await;
        if buckets.is_empty() {
            tracing::trace!("nenhum orçamento registrado ainda");
        }
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "memoria"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn backend() -> (Arc<MemoryBackend>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(10_000));
        (Arc::new(MemoryBackend::new(clock.clone())), clock)
    }

    #[tokio::test]
    async fn contabiliza_exatamente_o_limite() {
        let (b, _c) = backend();
        for esperado in 0..5_u32 {
            let d = b.check("k", 5, Duration::from_secs(60)).await.expect("ok");
            assert!(d.is_allowed(), "requisição {esperado} deveria passar");
            assert_eq!(d.remaining(), 4 - esperado);
        }
        assert!(!b
            .check("k", 5, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
    }

    #[tokio::test]
    async fn janela_desliza_e_libera_gradualmente() {
        // Eventos em t=10000; janela de 10s.
        // Só saem quando now - 10000 > 10000, isto é, t > 20000.
        let (b, c) = backend();
        let window = Duration::from_secs(10);
        for _ in 0..2 {
            assert!(b.check("k", 2, window).await.expect("ok").is_allowed());
        }
        assert!(!b.check("k", 2, window).await.expect("ok").is_allowed());

        // 6s depois os eventos têm 6s de idade: ainda dentro da janela.
        c.advance(Duration::from_secs(6));
        assert!(
            !b.check("k", 2, window).await.expect("ok").is_allowed(),
            "6s após o evento, a janela de 10s ainda o contém"
        );

        // Cruza a fronteira: ambos expiram e o orçamento volta a 2.
        c.advance(Duration::from_secs(5));
        assert!(b.check("k", 2, window).await.expect("ok").is_allowed());
        assert!(b.check("k", 2, window).await.expect("ok").is_allowed());
        assert!(!b.check("k", 2, window).await.expect("ok").is_allowed());
    }

    #[tokio::test]
    async fn evento_unico_expira_na_janela() {
        let (b, c) = backend();
        let window = Duration::from_secs(10);
        assert!(b.check("k", 1, window).await.expect("ok").is_allowed());
        assert!(!b.check("k", 1, window).await.expect("ok").is_allowed());

        // t=20001: o evento de t=10000 tem 10001ms e sai da janela.
        c.advance(Duration::from_secs(10) + Duration::from_millis(1));
        assert!(b.check("k", 1, window).await.expect("ok").is_allowed());
    }

    #[tokio::test]
    async fn janela_fixa_permitiria_o_dobro_esta_implementacao_nao() {
        // Cenário clássico da vulnerabilidade de janela fixa: 2 requisições no
        // fim de uma janela e mais 2 no começo da seguinte. Aqui a segunda
        // rajada é barrada, porque o orçamento é genuinely temporal.
        let (b, c) = backend();
        let window = Duration::from_secs(10);
        for _ in 0..3 {
            assert!(b.check("k", 3, window).await.expect("ok").is_allowed());
        }
        c.advance(Duration::from_millis(9_000));
        for _ in 0..3 {
            assert!(
                !b.check("k", 3, window).await.expect("ok").is_allowed(),
                "janela deslizante não pode liberar a rajada seguinte"
            );
        }
    }

    #[tokio::test]
    async fn limite_zero_nunca_autoriza() {
        let (b, _c) = backend();
        for _ in 0..5 {
            assert!(!b
                .check("k", 0, Duration::from_secs(60))
                .await
                .expect("ok")
                .is_allowed());
        }
    }

    #[tokio::test]
    async fn reset_remove_a_chave() {
        let (b, _c) = backend();
        let _ = b.check("k", 1, Duration::from_secs(60)).await.expect("ok");
        assert_eq!(b.tracked_keys().await, 1);
        b.reset("k").await.expect("ok");
        assert_eq!(b.tracked_keys().await, 0);
        assert!(b
            .check("k", 1, Duration::from_secs(60))
            .await
            .expect("ok")
            .is_allowed());
    }

    #[tokio::test]
    async fn chaves_distintas_ocupam_registros_distintos() {
        let (b, _c) = backend();
        let _ = b.check("a", 1, Duration::from_secs(60)).await.expect("ok");
        let _ = b.check("b", 1, Duration::from_secs(60)).await.expect("ok");
        assert_eq!(b.tracked_keys().await, 2);
    }

    #[tokio::test]
    async fn eventos_expirados_liberam_memoria_do_registro() {
        let (b, c) = backend();
        let _ = b.check("k", 10, Duration::from_secs(5)).await.expect("ok");
        c.advance(Duration::from_secs(30));
        // A próxima chamada poda os eventos antigos.
        let _ = b.check("k", 10, Duration::from_secs(5)).await.expect("ok");
        // Um evento só deve restar: o de agora.
        let deny = b.check("k", 1, Duration::from_secs(5)).await.expect("ok");
        assert!(!deny.is_allowed());
    }

    #[tokio::test]
    async fn concorrencia_nao_fura_o_limite() {
        // N tarefas disputando um orçamento de 50. Se a implementação não for
        // atômica, mais de 50 passam; aqui exatamente 50 devem passar.
        let (b, _c) = backend();
        let limite = 50_u32;
        let tarefas: Vec<_> = (0..200)
            .map(|_| {
                let b = b.clone();
                tokio::spawn(async move {
                    b.check("hot", limite, Duration::from_secs(60))
                        .await
                        .map(|d| d.is_allowed())
                        .unwrap_or(false)
                })
            })
            .collect();

        let mut passou = 0_u32;
        for t in tarefas {
            if t.await.expect("task") {
                passou += 1;
            }
        }
        assert_eq!(passou, limite, "exatamente {limite} deveriam passar");
    }

    #[tokio::test]
    async fn espera_aponta_para_o_evento_mais_antigo() {
        let (b, _c) = backend();
        // Janela de 30s, evento em t=10_000: libera em t=40_000.
        let d = b.check("k", 1, Duration::from_secs(30)).await.expect("ok");
        let negado = b.check("k", 1, Duration::from_secs(30)).await.expect("ok");
        assert!(d.is_allowed());
        assert!(!negado.is_allowed());
        assert_eq!(negado.wait_seconds(), 30);
    }

    #[tokio::test]
    async fn espera_nunca_e_zero_quando_negado() {
        // Retry-After: 0 mandaria o cliente repetir imediatamente, em loop.
        // Com janela de 10s e evento em t=10000, a espera é
        // 20000 - now, que só chega a zero quando o evento expira — e nesse
        // instante a requisição passa, então nunca há "negado com espera 0".
        let (b, c) = backend();
        let window = Duration::from_secs(10);

        assert!(b.check("k", 1, window).await.expect("ok").is_allowed());
        let negado = b.check("k", 1, window).await.expect("ok");
        assert!(!negado.is_allowed());
        assert_eq!(negado.wait_seconds(), 10);

        // No meio da janela a espera diminui, mas continua positiva.
        c.advance(Duration::from_secs(4));
        let meio = b.check("k", 1, window).await.expect("ok");
        assert!(!meio.is_allowed());
        assert_eq!(meio.wait_seconds(), 6);
        assert!(meio.wait_seconds() > 0);

        // Na borda, 1ms antes de expirar.
        c.advance(Duration::from_millis(5_999));
        let borda = b.check("k", 1, window).await.expect("ok");
        assert!(!borda.is_allowed());
        assert_eq!(
            borda.wait_seconds(),
            1,
            "nunca reporta espera arredondada a zero"
        );
    }

    #[tokio::test]
    async fn health_e_sempre_ok_sem_dependencia_externa() {
        let (b, _c) = backend();
        assert!(b.health().await.is_ok());
        let _ = b.check("k", 1, Duration::from_secs(1)).await.expect("ok");
        assert!(b.health().await.is_ok());
    }
}
