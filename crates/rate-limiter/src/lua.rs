//! Script Lua do sliding window log.
//!
//! # Por que Lua e não uma sequência de comandos
//!
//! Verificar e registrar precisam ser atômicos. Com comandos separados
//! (`ZCARD` e depois `ZADD`), N requisições concorrentes leem o mesmo `ZCARD`
//! antes de qualquer uma escrever, e todas passam. Com o script, o Redis
//! executa tudo como uma única unidade — sem trava, sem condição de corrida.
//!
//! # Por que sorted set e não contador
//!
//! Contador com `INCR` + `EXPIRE` é janela fixa disfarçada: `INCR` zera no
//! limite e a janela recomeça, permitindo o dobro do limite na fronteira. O
//! sorted set guarda o instante de cada requisição, o que permite de fato
//! deslizar a janela.

/// Contrato do script, documentado junto do código que depende dele.
///
/// `KEYS[1]` — chave do orçamento.
///
/// `ARGV[1]` — instante atual em milissegundos.
/// `ARGV[2]` — tamanho da janela em milissegundos.
/// `ARGV[3]` — limite de requisições na janela.
/// `ARGV[4]` — membro único desta requisição (ver nota abaixo).
///
/// Retorna `{ permitido, restante, espera_ms }`.
///
/// # Por que o membro precisa ser único
///
/// `ZADD` com um membro já existente apenas atualiza o score — não soma. Se o
/// membro fosse só o timestamp em milissegundos, um cliente que disparasse duas
/// requisições no mesmo milissegundo seria contado uma vez só, e o limite seria
/// furado em silêncio, exatamente nos picos de tráfego — o cenário que o rate
/// limiter existe para conter.
pub const SLIDING_WINDOW_SCRIPT: &str = r#"
local key    = KEYS[1]
local now    = tonumber(ARGV[1])
local window = tonumber(ARGV[2])
local limit  = tonumber(ARGV[3])
local member = ARGV[4]

-- 1. Descarta requisições que já saíram da janela deslizante.
redis.call('ZREMRANGEBYSCORE', key, '-inf', now - window)

-- 2. Quantas requisições ainda estão dentro da janela.
local count = redis.call('ZCARD', key)

local oldest = redis.call('ZRANGE', key, 0, 0, 'WITHSCORES')
local wait = 0
if oldest[2] then
  wait = tonumber(oldest[2]) + window - now
  if wait < 0 then wait = 0 end
end

-- 3. Dentro do orçamento: registra e libera.
if count < limit then
  redis.call('ZADD', key, now, member)
  redis.call('PEXPIRE', key, window)
  return { 1, limit - count - 1, wait }
end

-- 4. Esgotado: informa quanto tempo falta.
return { 0, 0, wait }
"#;

/// Script de limpeza explícita de uma chave.
pub const RESET_SCRIPT: &str = r#"
return redis.call('DEL', KEYS[1])
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_declara_todos_os_argumentos() {
        for i in 1..=4 {
            assert!(
                SLIDING_WINDOW_SCRIPT.contains(&format!("ARGV[{i}]")),
                "ARGV[{i}] deve ser lido pelo script"
            );
        }
        assert!(SLIDING_WINDOW_SCRIPT.contains("KEYS[1]"));
    }

    #[test]
    fn script_remove_eventos_fora_da_janela() {
        assert!(SLIDING_WINDOW_SCRIPT.contains("ZREMRANGEBYSCORE"));
    }

    #[test]
    fn script_compara_contagem_com_limite() {
        assert!(SLIDING_WINDOW_SCRIPT.contains("count < limit"));
    }

    #[test]
    fn script_devolve_tres_campos() {
        assert!(SLIDING_WINDOW_SCRIPT.contains("return { 1,"));
        assert!(SLIDING_WINDOW_SCRIPT.contains("return { 0,"));
    }

    #[test]
    fn script_nao_aceita_espera_negativa() {
        // Um valor negativo viraria Retry-After negativo, que o cliente leria
        // como "pode tentar imediatamente" — o oposto do desejado.
        assert!(SLIDING_WINDOW_SCRIPT.contains("if wait < 0 then wait = 0 end"));
    }

    #[test]
    fn script_expira_a_chave_com_a_janela() {
        assert!(
            SLIDING_WINDOW_SCRIPT.contains("PEXPIRE"),
            "sem TTL, chaves de clientes abandonados vazariam memória no Redis"
        );
    }

    #[test]
    fn script_nao_pode_ler_mais_de_uma_chave() {
        assert!(!SLIDING_WINDOW_SCRIPT.contains("KEYS[2]"));
    }

    #[test]
    fn reset_script_apaga_a_chave() {
        assert!(RESET_SCRIPT.contains("DEL"));
    }

    #[test]
    fn scripts_sao_balanceados() {
        for (nome, src) in [
            ("sliding_window", SLIDING_WINDOW_SCRIPT),
            ("reset", RESET_SCRIPT),
        ] {
            let abertos = src.matches('{').count();
            let fechados = src.matches('}').count();
            assert_eq!(abertos, fechados, "{nome}: chaves desbalanceadas");
            let parenteses_abertos = src.matches('(').count();
            let parenteses_fechados = src.matches(')').count();
            assert_eq!(
                parenteses_abertos, parenteses_fechados,
                "{nome}: parênteses desbalanceados"
            );
        }
    }
}
