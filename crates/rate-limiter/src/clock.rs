//! Relógio injetável.
//!
//! O tempo é a variável mais difícil de testar num rate limiter. Com relógio de
//! sistema, testar "a janela expira" significa dormir um segundo por caso — a
//! suíte fica lenta e, pior, fica flaky em carga. Com relógio manual, o teste
//! avança o tempo instantaneamente e o comportamento é determinístico.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Fonte de tempo em milissegundos.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> u64;
}

/// Relógio do sistema.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Relógio controlado por teste.
///
/// Compartilhável entre o backend e o próprio teste, que avança o tempo para
/// simular o passar das janelas.
#[derive(Debug)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    pub fn new(start_millis: u64) -> Self {
        Self {
            millis: AtomicU64::new(start_millis),
        }
    }

    /// Avança o relógio.
    pub fn advance(&self, by: std::time::Duration) {
        let delta = u64::try_from(by.as_millis()).unwrap_or(0);
        self.millis.fetch_add(delta, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }
}

/// Relógio compartilhável entre componentes.
pub type SharedClock = Arc<dyn Clock>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn relogio_manual_avanca_somente_quando_pedido() {
        let clock = ManualClock::new(1_000);
        assert_eq!(clock.now_millis(), 1_000);
        assert_eq!(clock.now_millis(), 1_000, "sem avanço, o tempo é estável");
        clock.advance(Duration::from_millis(250));
        assert_eq!(clock.now_millis(), 1_250);
    }

    #[test]
    fn relogio_manual_acumula_avancos() {
        let clock = ManualClock::default();
        clock.advance(Duration::from_secs(5));
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now_millis(), 10_000);
    }

    #[test]
    fn relogio_do_sistema_avanca_no_tempo_real() {
        let clock = SystemClock;
        let first = clock.now_millis();
        assert!(
            first > 1_600_000_000_000,
            "deveria ser epoch em milissegundos"
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(clock.now_millis() >= first, "o tempo real não retrocede");
    }

    #[test]
    fn relogio_manual_compartilhado_por_varias_threads() {
        let clock = Arc::new(ManualClock::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = clock.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    c.advance(Duration::from_millis(1));
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        assert_eq!(clock.now_millis(), 800, "8 threads × 100 × 1ms");
    }
}
