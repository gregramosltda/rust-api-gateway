//! Benchmark de throughput do pipeline.
//!
//! Mede as operações que dominam a latência do gateway, isolando o custo de
//! cada camada. Criterion salva o histórico entre execuções, então dá para
//! comparar uma mudança com a linha de base.
//!
//! Os números são de microbenchmark: medem a estrutura de dados, não o
//! gateway completo com rede. O teste de integração mede o caminho real. Os
//! dois se complementam — este diz onde está o custo, aquele diz se o sistema
//! funciona.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use gateway_cache::{is_cacheable_method, ResponseCache};
use gateway_common::{budget_key, client_identity, RateLimitDecision};
use gateway_rate_limiter::{ManualClock, MemoryBackend, RateLimiterBackend};

// ---------------------------------------------------------------------------
// Configuração
// ---------------------------------------------------------------------------

fn chave(caminho: &str) -> String {
    budget_key("10.0.0.1", caminho, "bench")
}

// ---------------------------------------------------------------------------
// Rate limiter
// ---------------------------------------------------------------------------

fn bench_rate_limiter(c: &mut Criterion) {
    let mut grupo = c.benchmark_group("rate_limiter");

    grupo.bench_function("janela_60s_limite_1000", |b| {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let limiter = MemoryBackend::new(clock);
        // O runtime é criado uma vez, fora do laço: construí-lo a cada
        // iteração mediria o custo de subir threads, e não o do rate limiter.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        b.iter(|| {
            rt.block_on(limiter.check(&chave("/api/v1/busca"), 1_000, Duration::from_secs(60)))
        })
    });

    //Limite curto: o pior caso, em que o orçamento está quase no fim.
    grupo.bench_function("janela_60s_limite_10", |b| {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let limiter = MemoryBackend::new(clock);
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        b.iter(|| {
            rt.block_on(limiter.check(&chave("/api/v1/critico"), 10, Duration::from_secs(60)))
        })
    });

    // Throughput bruto: o limiter aceita, não barra.
    grupo.bench_function("janela_60s_limite_1000_multiplas_chaves", |b| {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let limiter = MemoryBackend::new(clock);
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut i = 0_u64;
        b.iter(|| {
            i += 1;
            rt.block_on(limiter.check(
                &chave(&format!("/api/v1/recurso-{i}")),
                1_000,
                Duration::from_secs(60),
            ))
        })
    });

    grupo.finish();
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

fn bench_cache(c: &mut Criterion) {
    let mut grupo = c.benchmark_group("cache");

    grupo.bench_function("insert", |b| {
        let cache = ResponseCache::new(10_000, Duration::from_secs(60));
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let resposta = gateway_cache::CachedResponse::new(
            200,
            Some("application/json".to_owned()),
            vec![b'x'; 2_048],
        );
        let mut i = 0_u64;
        b.iter(|| {
            i += 1;
            rt.block_on(cache.insert(&format!("GET /recurso-{i}"), resposta.clone()))
        })
    });

    grupo.bench_function("get_hit", |b| {
        let cache = ResponseCache::new(10_000, Duration::from_secs(60));
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let k = "GET /recurso-quente";
        let resposta = gateway_cache::CachedResponse::new(
            200,
            Some("application/json".to_owned()),
            vec![b'x'; 2_048],
        );
        rt.block_on(cache.insert(k, resposta));
        b.iter(|| rt.block_on(cache.get(k)))
    });

    grupo.bench_function("get_miss", |b| {
        let cache = ResponseCache::new(10_000, Duration::from_secs(60));
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        b.iter(|| rt.block_on(cache.get("GET /inexistente")))
    });

    grupo.bench_function("montagem_de_chave", |b| {
        b.iter(|| ResponseCache::key("GET", "/api/v1/usuarios", Some("pagina=2")))
    });

    grupo.finish();
}

// ---------------------------------------------------------------------------
// Caminho crítico (rate limit → chave → decisão)
// ---------------------------------------------------------------------------

fn bench_pipeline_critico(c: &mut Criterion) {
    let mut grupo = c.benchmark_group("pipeline");

    grupo.bench_function("chave_de_orcamento", |b| b.iter(|| chave("/api/v1/busca")));

    grupo.bench_function("chave_com_query", |b| {
        b.iter(|| ResponseCache::key("GET", "/api/v1/busca", Some("q=rust&pagina=2")))
    });

    grupo.bench_function("extrair_headers_de_resposta", |b| {
        let decisao = RateLimitDecision::from_parts(true, 100, 42, Duration::from_secs(30));
        b.iter(|| decisao.headers())
    });

    grupo.bench_function("metodo_elegivel_para_cache", |b| {
        let permitidos = vec!["GET".to_owned(), "HEAD".to_owned()];
        b.iter(|| is_cacheable_method("GET", &permitidos))
    });

    grupo.bench_function("identidade_de_cliente", |b| {
        b.iter(|| client_identity("192.168.15.42:54321"))
    });

    grupo.finish();
}

criterion_group!(
    benches,
    bench_rate_limiter,
    bench_cache,
    bench_pipeline_critico
);
criterion_main!(benches);
