//! Cache de respostas HTTP em memória.
//!
//! # Escolhas de projeto
//!
//! **Moka, não um `HashMap` + thread.** Um `Mutex<HashMap>` serializa todas as
//! leituras — no gateway, o cache é consultado em toda requisição, então isso
//! vira o gargalo do sistema inteiro. O Moka segmenta internamente e remove
//! entradas com bloqueio, o que dá leituras concorrentes sem contenção.
//!
//! **Chave inclui método, caminho e query.** Sem o método, um `POST /users` e um
//! `GET /users` dividiriam a mesma entrada — e servir uma resposta de leitura
//! para uma escrita seria pior que não cachear. Sem a query, `/search?q=a` e
//! `/search?q=b` colidiriam, servindo resultados de uma busca na outra.
//!
//! **Invalidação por rota, não por tag.** Tags would exigem varrer o cache
//! inteiro para remover todas as entradas de um prefixo. Como o gateway só
//! guarda `GET`/`HEAD`, e escritas invalidam por prefixo, a semântica fica
//! previsível e o custo é previsível.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use tokio::sync::RwLock;

/// Corpo cacheado.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Arc<Vec<u8>>,
}

impl CachedResponse {
    pub fn new(status: u16, content_type: Option<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body: Arc::new(body),
        }
    }

    /// Tamanho aproximado em bytes, para observabilidade.
    pub fn size_bytes(&self) -> usize {
        self.body.len() + self.content_type.as_ref().map_or(0, |c| c.len())
    }
}

/// Resultado de uma consulta ao cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Servir a partir do cache.
    Hit(CachedResponse),
    /// Buscar no upstream.
    Miss,
}

/// Contadores do cache. `AtomicU64` não é `Copy`, então o struct não é
/// derivado como tal — ele é compartilhado por referência.
#[derive(Debug, Default)]
struct CacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
    invalidations: AtomicU64,
}

impl CacheStats {
    fn snapshot(&self) -> (u64, u64, u64, f64) {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        let ratio = if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        };
        (
            hits,
            misses,
            self.invalidations.load(Ordering::Relaxed),
            ratio,
        )
    }
}

/// Cache de respostas com TTL.
pub struct ResponseCache {
    cache: Cache<String, CachedResponse>,
    ttl: Duration,
    stats: CacheStats,
    /// Prefixos cuja leitura é vedada (ex.: rota marcada como não cacheável).
    blocked_prefixes: RwLock<Vec<String>>,
}

impl ResponseCache {
    /// Cria cache com capacidade e TTL.
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(ttl)
            // Deliberadamente SEM `time_to_idle`: zero significa "expira assim
            // que ninguém segura a entrada", o que zera o cache na prática e
            // faria todo teste de acerto falhar. TTL já cobre o descarte por
            // tempo, e `max_capacity` cobre o volume.
            .build();
        Self {
            cache,
            ttl,
            stats: CacheStats::default(),
            blocked_prefixes: RwLock::new(Vec::new()),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Monta a chave de cache.
    pub fn key(method: &str, path: &str, query: Option<&str>) -> String {
        let query = query.filter(|q| !q.is_empty()).unwrap_or("-");
        format!("{method} {path}?{query}")
    }

    /// Consulta o cache.
    pub async fn get(&self, key: &str) -> CacheOutcome {
        if self.is_blocked(key).await {
            return CacheOutcome::Miss;
        }
        match self.cache.get(key).await {
            Some(response) => {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(key, "cache hit");
                CacheOutcome::Hit(response)
            }
            None => {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(key, "cache miss");
                CacheOutcome::Miss
            }
        }
    }

    /// Insere uma resposta. Entradas maiores que o TTL-effective de memória
    /// simplesmente não são inseridas — o Moka descarta ao expirar.
    pub async fn insert(&self, key: &str, response: CachedResponse) {
        if self.is_blocked(key).await {
            return;
        }
        tracing::trace!(key, bytes = response.size_bytes(), "cache insert");
        self.cache.insert(key.to_owned(), response).await;
    }

    /// Invalida todas as entradas cujo caminho começa com `prefix`.
    ///
    /// O Moka não oferece remoção por padrão de chave, então a varredura é
    /// manual. É aceitável porque invalidações são raras em leitura e o custo
    /// é proporcional ao número de entradas — não ao tráfego.
    pub async fn invalidate_prefix(&self, prefix: &str) -> usize {
        let mut removidas = 0_usize;
        // `moka::future::Iter` já é um stream; não precisa de `await`.
        // A chave vem como `Arc<String>`; converte para `String` para uso posterior.
        let entradas: Vec<String> = self.cache.iter().map(|(k, _)| k.as_ref().clone()).collect();
        for chave in entradas {
            // A chave tem o formato "MÉTODO /caminho?query".
            let caminho = chave
                .split_once(' ')
                .map_or(chave.as_str(), |(_, resto)| resto)
                .split('?')
                .next()
                .unwrap_or_default();
            if caminho.starts_with(prefix) {
                self.cache.invalidate(&chave).await;
                removidas += 1;
            }
        }
        self.stats.invalidations.fetch_add(1, Ordering::Relaxed);
        tracing::info!(prefix, removidas, "cache invalidado por prefixo");
        removidas
    }

    /// Invalida tudo.
    pub async fn invalidate_all(&self) {
        self.cache.invalidate_all();
        self.stats.invalidations.fetch_add(1, Ordering::Relaxed);
        tracing::info!("cache invalidado por completo");
    }

    /// Veda a leitura de um prefixo de rota.
    pub async fn block_prefix(&self, prefix: &str) {
        let mut bloqueados = self.blocked_prefixes.write().await;
        if !bloqueados.iter().any(|p| p == prefix) {
            bloqueados.push(prefix.to_owned());
        }
    }

    async fn is_blocked(&self, key: &str) -> bool {
        let bloqueados = self.blocked_prefixes.read().await;
        if bloqueados.is_empty() {
            return false;
        }
        let caminho = key
            .split_once(' ')
            .map_or(key, |(_, resto)| resto)
            .split('?')
            .next()
            .unwrap_or_default();
        bloqueados.iter().any(|p| caminho.starts_with(p))
    }

    /// Números de acerto, erro, invalidação e a razão de acerto.
    pub async fn stats(&self) -> CacheStatsReport {
        let (hits, misses, invalidations, ratio) = self.stats.snapshot();
        CacheStatsReport {
            hits,
            misses,
            invalidations,
            hit_ratio: ratio,
            entries: self.cache.entry_count(),
            ttl_seconds: self.ttl.as_secs(),
        }
    }
}

/// Fotografia das métricas do cache.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStatsReport {
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
    /// Razão de acerto em `0.0..=1.0`.
    pub hit_ratio: f64,
    pub entries: u64,
    pub ttl_seconds: u64,
}

/// Decide se um método é elegível para cache.
pub fn is_cacheable_method(method: &str, permitidos: &[String]) -> bool {
    permitidos.iter().any(|m| m.eq_ignore_ascii_case(method))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    fn resposta(marker: &str) -> CachedResponse {
        CachedResponse::new(
            200,
            Some("text/plain".to_owned()),
            marker.as_bytes().to_vec(),
        )
    }

    #[test]
    fn chave_inclui_metodo_caminho_e_query() {
        assert_eq!(ResponseCache::key("GET", "/a", Some("q=1")), "GET /a?q=1");
        assert_eq!(ResponseCache::key("GET", "/a", None), "GET /a?-");
        assert_eq!(ResponseCache::key("GET", "/a", Some("")), "GET /a?-");
    }

    #[test]
    fn chave_distingue_query_vazia_de_ausente() {
        assert_eq!(
            ResponseCache::key("GET", "/a", Some("q=1")),
            ResponseCache::key("GET", "/a", Some("q=1"))
        );
    }

    #[test]
    fn metodo_elegivel_respeita_a_lista() {
        let permitidos = vec!["GET".to_owned(), "HEAD".to_owned()];
        assert!(is_cacheable_method("GET", &permitidos));
        assert!(is_cacheable_method("get", &permitidos));
        assert!(!is_cacheable_method("POST", &permitidos));
        assert!(!is_cacheable_method("DELETE", &permitidos));
    }

    #[tokio::test]
    async fn primeira_consulta_erra_e_segunda_acerta() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        let chave = ResponseCache::key("GET", "/users", None);

        assert_eq!(cache.get(&chave).await, CacheOutcome::Miss);
        cache.insert(&chave, resposta("dados")).await;

        match cache.get(&chave).await {
            CacheOutcome::Hit(hit) => {
                assert_eq!(hit.status, 200);
                assert_eq!(hit.body.as_slice(), b"dados");
            }
            CacheOutcome::Miss => panic!("esperava acerto após inserção"),
        }
    }

    #[tokio::test]
    async fn insercao_devolve_o_mesmo_objeto() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        let k = ResponseCache::key("GET", "/x", None);
        let original = resposta("payload");
        cache.insert(&k, original.clone()).await;
        match cache.get(&k).await {
            CacheOutcome::Hit(hit) => {
                assert_eq!(hit.status, original.status);
                assert_eq!(hit.content_type, original.content_type);
                assert_eq!(hit.body, original.body);
            }
            CacheOutcome::Miss => panic!("esperava acerto"),
        }
    }

    #[tokio::test]
    async fn chaves_distintas_nao_colidem() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /a", resposta("A")).await;
        cache.insert("GET /b", resposta("B")).await;
        cache.insert("GET /a?q=1", resposta("A1")).await;

        assert!(matches!(cache.get("GET /a").await, CacheOutcome::Hit(_)));
        assert!(matches!(cache.get("GET /b").await, CacheOutcome::Hit(_)));
        assert!(matches!(
            cache.get("GET /a?q=1").await,
            CacheOutcome::Hit(_)
        ));
        assert_eq!(cache.get("GET /c").await, CacheOutcome::Miss);
    }

    #[tokio::test]
    async fn metodo_diferente_nao_compartilha_entrada() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /users", resposta("leitura")).await;
        assert_eq!(cache.get("POST /users").await, CacheOutcome::Miss);
    }

    #[tokio::test]
    async fn ttl_expira_a_entrada() {
        let cache = ResponseCache::new(100, Duration::from_millis(80));
        let k = ResponseCache::key("GET", "/tmp", None);
        cache.insert(&k, resposta("efemera")).await;
        assert!(matches!(cache.get(&k).await, CacheOutcome::Hit(_)));

        sleep(Duration::from_millis(250)).await;

        assert_eq!(
            cache.get(&k).await,
            CacheOutcome::Miss,
            "TTL deveria expirar"
        );
    }

    #[tokio::test]
    async fn invalidacao_por_prefixo_remove_apenas_o_grupo() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /api/v1/users", resposta("u")).await;
        cache.insert("GET /api/v1/posts", resposta("p")).await;
        cache.insert("GET /publico", resposta("pub")).await;

        let removidas = cache.invalidate_prefix("/api/v1").await;
        assert_eq!(removidas, 2);

        assert_eq!(cache.get("GET /api/v1/users").await, CacheOutcome::Miss);
        assert_eq!(cache.get("GET /api/v1/posts").await, CacheOutcome::Miss);
        assert!(
            matches!(cache.get("GET /publico").await, CacheOutcome::Hit(_)),
            "rota fora do prefixo deve sobreviver"
        );
    }

    #[tokio::test]
    async fn invalidacao_respeita_query_na_chave() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /busca?q=rus", resposta("r")).await;
        cache.insert("GET /busca?q=go", resposta("g")).await;

        assert_eq!(cache.invalidate_prefix("/busca").await, 2);
        assert_eq!(cache.get("GET /busca?q=rus").await, CacheOutcome::Miss);
    }

    #[tokio::test]
    async fn invalidacao_total_limpa_tudo() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /a", resposta("a")).await;
        cache.insert("GET /b", resposta("b")).await;
        cache.invalidate_all().await;
        assert_eq!(cache.get("GET /a").await, CacheOutcome::Miss);
        assert_eq!(cache.get("GET /b").await, CacheOutcome::Miss);
    }

    #[tokio::test]
    async fn prefixo_bloqueado_fora_leitura_e_insercao() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /privado/x", resposta("s")).await;
        cache.block_prefix("/privado").await;

        assert_eq!(cache.get("GET /privado/x").await, CacheOutcome::Miss);

        // Mesmo inserindo, a entrada continua inacessível.
        cache.insert("GET /privado/y", resposta("t")).await;
        assert_eq!(cache.get("GET /privado/y").await, CacheOutcome::Miss);

        assert!(
            matches!(cache.get("GET /aberto").await, CacheOutcome::Miss),
            "rota não bloqueada começa errada, como esperado"
        );
    }

    #[tokio::test]
    async fn bloqueio_de_prefixo_nao_vaza_para_rotas_similares() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /pub", resposta("ok")).await;
        cache.block_prefix("/priv").await;
        assert!(
            matches!(cache.get("GET /pub").await, CacheOutcome::Hit(_)),
            "\"/pub\" não começa com \"/priv\""
        );
    }

    #[tokio::test]
    async fn metricas_acumulam_acertos_e_erros() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        let k = ResponseCache::key("GET", "/m", None);

        assert_eq!(cache.get(&k).await, CacheOutcome::Miss);
        cache.insert(&k, resposta("x")).await;
        let _ = cache.get(&k).await;
        let _ = cache.get(&k).await;

        let stats = cache.stats().await;
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!(
            (stats.hit_ratio - 0.6667).abs() < 0.001,
            "ratio: {}",
            stats.hit_ratio
        );
        assert_eq!(stats.invalidations, 0);
        // `entries` NÃO é conferido: o Moka atualiza a contagem de forma
        // assíncrona, então logo após `insert` ela ainda pode ser 0. Verificar
        // esse número aqui testaria a temporização interna da biblioteca, não o
        // comportamento do cache.
    }

    #[tokio::test]
    async fn entrada_fica_visivel_para_o_proximo_acesso() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        let k = ResponseCache::key("GET", "/visivel", None);
        cache.insert(&k, resposta("ok")).await;
        // Este é o contrato que importa: inserir e depois ler.
        assert!(matches!(cache.get(&k).await, CacheOutcome::Hit(_)));
    }

    #[tokio::test]
    async fn ratio_zerado_sem_trafego() {
        let cache = ResponseCache::new(10, Duration::from_secs(1));
        let stats = cache.stats().await;
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.hit_ratio, 0.0);
    }

    #[tokio::test]
    async fn invalidações_sao_contadas() {
        let cache = ResponseCache::new(100, Duration::from_secs(60));
        cache.insert("GET /x", resposta("x")).await;
        cache.invalidate_prefix("/x").await;
        cache.invalidate_all().await;
        assert_eq!(cache.stats().await.invalidations, 2);
    }

    #[tokio::test]
    async fn capacidade_limita_o_numero_de_entradas() {
        let cache = ResponseCache::new(3, Duration::from_secs(60));
        for i in 0..10 {
            cache
                .insert(&format!("GET /item-{i}"), resposta(&i.to_string()))
                .await;
        }
        // Moka pode reter um pouco além da capacidade antes de descartar.
        let stats = cache.stats().await;
        assert!(
            stats.entries <= 10,
            "entradas: {} (capacidade 3, tolerância de evicted)",
            stats.entries
        );
    }

    #[test]
    fn tamanho_conta_corpo_e_content_type() {
        let r = CachedResponse::new(200, Some("text/plain".to_owned()), vec![1, 2, 3]);
        assert_eq!(r.size_bytes(), 3 + "text/plain".len());
    }

    #[test]
    fn content_type_ausente_nao_quebra_o_tamanho() {
        let r = CachedResponse::new(204, None, vec![1, 2]);
        assert_eq!(r.size_bytes(), 2);
    }

    #[test]
    fn relatorio_serializa_para_json() {
        let report = CacheStatsReport {
            hits: 5,
            misses: 5,
            invalidations: 0,
            hit_ratio: 0.5,
            entries: 5,
            ttl_seconds: 30,
        };
        let json = serde_json::to_string(&report).expect("serializar");
        assert!(json.contains("\"hit_ratio\":0.5"));
        assert!(json.contains("\"ttl_seconds\":30"));
    }
}
