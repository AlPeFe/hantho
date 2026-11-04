//! Salud de las fuentes: qué fuentes funcionan, cuáles fallan y por qué.
//! Se guarda en `~/.hantho/source_health.json` para no volver a esperar a las que están rotas.
//!
//! Tras varios fallos seguidos, una fuente se omite en la búsqueda durante un tiempo que depende
//! del tipo de error y crece si sigue fallando. Al vencer se vuelve a probar sola.

use crate::host::LoadedSource;
use crate::paths;
use crate::search;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;
/// Fuentes por tanda de búsqueda. Con estas o menos se consultan todas.
pub const BATCH_SIZE: usize = 20;

/// De qué tipo es un fallo. Decide cuánto tiempo se omite la fuente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Desafío de Cloudflare (sin WebView no se puede resolver).
    Cloudflare,
    /// No se puede conectar: web caída o bloqueada por la red. Suele ser temporal.
    Unreachable,
    /// El dominio no existe (o no hay conexión).
    DeadDomain,
    Timeout,
    Http(u16),
    /// La extensión usa algo de Android que el host todavía no tiene.
    MissingApi,
    /// Fallo del propio ext-host: la fuente no tiene la culpa.
    Host,
    Other,
}

impl Kind {
    /// Clasifica el error tal como llega del host (`excepción: mensaje`).
    pub fn classify(error: &str) -> Kind {
        let has = |patterns: &[&str]| patterns.iter().any(|p| error.contains(p));
        if has(&["ext-host", "no cargada"]) {
            Kind::Host
        } else if has(&["Cloudflare pide"]) {
            Kind::Cloudflare
        } else if has(&["UnknownHostException"]) {
            Kind::DeadDomain
        } else if has(&["Failed to connect to ", "ConnectException", "SSLException", "SSLHandshakeException", "Connection reset"]) {
            Kind::Unreachable
        } else if has(&["SocketTimeoutException", "timed out", "timeout"]) {
            Kind::Timeout
        } else if has(&["NoClassDefFoundError", "ClassNotFoundException", "NoSuchMethodError", "NoSuchFieldError", "UnsupportedOperationException", "NotImplementedError"]) {
            Kind::MissingApi
        } else if let Some(code) = error.split("HTTP error ").nth(1).and_then(|c| c.get(..3)).and_then(|c| c.parse().ok()) {
            Kind::Http(code)
        } else {
            Kind::Other
        }
    }

    /// Sin conexión fallan todas las fuentes a la vez con estos errores: no hay que culparlas.
    fn is_network(self) -> bool {
        matches!(self, Kind::Unreachable | Kind::DeadDomain | Kind::Timeout)
    }

    /// (fallos seguidos para omitirla, tiempo inicial, tiempo máximo), en segundos.
    fn backoff(self) -> (u32, u64, u64) {
        match self {
            // Puede ser un bloqueo temporal de la red: se vuelve a probar pronto
            Kind::Unreachable | Kind::Timeout => (3, HOUR / 2, 6 * HOUR),
            Kind::Cloudflare | Kind::DeadDomain => (3, 6 * HOUR, 3 * DAY),
            // Nos han pedido frenar: a la primera, y no insistir hasta dentro de un rato
            Kind::Http(429) => (1, HOUR, DAY),
            Kind::Http(_) | Kind::Other | Kind::Host => (3, HOUR, DAY),
            // No se arregla solo: hasta que se actualice la extensión o Han Thỏ
            Kind::MissingApi => (2, 30 * DAY, 30 * DAY),
        }
    }
}

/// Dónde ha ocurrido: buscar, o abrir un manga o capítulo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Search,
    Read,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// Fallos seguidos al buscar.
    pub fails: u32,
    /// Fallos seguidos al abrir mangas o capítulos. Van aparte: una fuente puede buscar bien y no
    /// dejar leer nada.
    pub read_fails: u32,
    pub kind: Option<Kind>,
    /// Último error, ya en texto para el usuario.
    pub error: Option<String>,
    pub last_ok: u64,
    pub last_fail: u64,
    /// Latencia típica (media móvil) de las peticiones correctas.
    pub latency_ms: Option<u64>,
    pub disabled: bool,
    /// Versión de Han Thỏ del último fallo: una versión nueva puede traer lo que faltaba.
    pub app_version: String,
}

impl Record {
    pub fn has_problem(&self) -> bool {
        self.fails > 0 || self.read_fails > 0
    }

    /// Hasta cuándo se omite por sus fallos (`None` si no hay que omitirla).
    pub fn skip_until(&self) -> Option<u64> {
        let kind = self.kind?;
        if kind == Kind::MissingApi && self.app_version != env!("CARGO_PKG_VERSION") {
            return None;
        }
        let (threshold, base, cap) = kind.backoff();
        let fails = self.fails.max(self.read_fails);
        if fails < threshold {
            return None;
        }
        let secs = base.saturating_mul(1u64 << (fails - threshold).min(16)).min(cap);
        Some(self.last_fail + secs)
    }

    fn skipped_at(&self, now: u64) -> bool {
        self.disabled || self.skip_until().is_some_and(|until| now < until)
    }

    fn on_ok(&mut self, stage: Stage, latency_ms: u64, now: u64) {
        match stage {
            Stage::Search => self.fails = 0,
            Stage::Read => self.read_fails = 0,
        }
        if !self.has_problem() {
            self.kind = None;
            self.error = None;
        }
        self.last_ok = now;
        self.latency_ms = Some(self.latency_ms.map_or(latency_ms, |old| (old * 3 + latency_ms) / 4));
    }

    fn on_fail(&mut self, stage: Stage, kind: Kind, error: String, now: u64) {
        match stage {
            Stage::Search => self.fails = self.fails.saturating_add(1),
            Stage::Read => self.read_fails = self.read_fails.saturating_add(1),
        }
        self.kind = Some(kind);
        self.error = Some(error);
        self.last_fail = now;
        self.app_version = env!("CARGO_PKG_VERSION").into();
    }
}

/// Resultado de consultar de verdad una fuente en una búsqueda (los aciertos de caché no cuentan).
#[derive(Debug)]
pub struct Outcome {
    pub source_id: String,
    pub result: Result<u64, (Kind, String)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Health {
    sources: HashMap<String, Record>,
}

impl Health {
    pub fn load() -> Health {
        std::fs::read(paths::source_health_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let path = paths::source_health_file();
        let Ok(bytes) = serde_json::to_vec_pretty(self) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Temporal + rename: la TUI y el servidor MCP comparten este archivo
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Lee, modifica y guarda.
    pub fn update(change: impl FnOnce(&mut Health)) {
        let mut health = Health::load();
        change(&mut health);
        health.save();
    }

    pub fn get(&self, source_id: &str) -> Option<&Record> {
        self.sources.get(source_id)
    }

    /// Olvida los fallos (no el estado de desactivada ni la latencia).
    pub fn forgive(&mut self, source_id: &str) {
        if let Some(r) = self.sources.get_mut(source_id) {
            (r.fails, r.read_fails, r.last_fail) = (0, 0, 0);
            (r.kind, r.error) = (None, None);
        }
    }

    pub fn set_disabled(&mut self, source_id: &str, disabled: bool) {
        self.sources.entry(source_id.to_string()).or_default().disabled = disabled;
    }

    /// Separa las fuentes en las que hay que consultar y las que se omiten. Si no queda ninguna
    /// activa se prueban las omitidas por fallos: mejor esperar que no buscar en ningún sitio.
    pub fn partition(&self, sources: Vec<LoadedSource>, now: u64) -> (Vec<LoadedSource>, Vec<LoadedSource>) {
        let skipped_now = |s: &LoadedSource| self.get(&s.info.id).is_some_and(|r| r.skipped_at(now));
        let (active, skipped): (Vec<_>, Vec<_>) = sources.into_iter().partition(|s| !skipped_now(s));
        if !active.is_empty() {
            return (active, skipped);
        }
        skipped.into_iter().partition(|s| !self.get(&s.info.id).is_some_and(|r| r.disabled))
    }

    /// Con muchas fuentes, elige en cuáles buscar primero: las de tus favoritos, luego las que no
    /// dan problemas y, entre ellas, las más rápidas. Devuelve esa tanda y el resto, en orden.
    pub fn first_batch(&self, mut sources: Vec<LoadedSource>, favorites: &HashSet<&str>) -> (Vec<LoadedSource>, Vec<LoadedSource>) {
        if sources.len() <= BATCH_SIZE {
            return (sources, Vec::new());
        }
        sources.sort_by_key(|s| {
            let record = self.get(&s.info.id);
            (!favorites.contains(s.info.id.as_str()), record.is_some_and(Record::has_problem), record.and_then(|r| r.latency_ms).unwrap_or(u64::MAX))
        });
        let rest = sources.split_off(BATCH_SIZE);
        (sources, rest)
    }

    /// Aplica los resultados de una búsqueda. Si ninguna fuente ha respondido, lo más probable es
    /// que el problema sea la conexión, así que los fallos de red no cuentan.
    fn apply_search(&mut self, outcomes: Vec<Outcome>, now: u64) {
        let any_ok = outcomes.iter().any(|o| o.result.is_ok());
        for outcome in outcomes {
            match outcome.result {
                Ok(latency) => self.sources.entry(outcome.source_id).or_default().on_ok(Stage::Search, latency, now),
                Err((kind, error)) => {
                    if kind != Kind::Host && (any_ok || !kind.is_network()) {
                        self.sources.entry(outcome.source_id).or_default().on_fail(Stage::Search, kind, error, now);
                    }
                }
            }
        }
    }

    fn apply_read_error(&mut self, source_id: &str, kind: Kind, error: String, now: u64) {
        // Un 404 suele ser de ese manga o capítulo, no de la fuente
        if !matches!(kind, Kind::Host | Kind::Http(404) | Kind::Http(410)) {
            self.sources.entry(source_id.to_string()).or_default().on_fail(Stage::Read, kind, error, now);
        }
    }
}

/// Guarda de una vez los resultados de una búsqueda global.
pub fn record_search(outcomes: Vec<Outcome>) {
    if !outcomes.is_empty() {
        Health::update(|h| h.apply_search(outcomes, crate::library::now()));
    }
}

/// Se ha abierto un manga o capítulo sin problemas.
pub fn record_read_ok(source_id: &str, latency_ms: u64) {
    Health::update(|h| h.sources.entry(source_id.to_string()).or_default().on_ok(Stage::Read, latency_ms, crate::library::now()));
}

/// Ha fallado abrir un manga o capítulo. `error` es el texto original del host.
pub fn record_read_error(source_id: &str, error: &str) {
    let (kind, text) = (Kind::classify(error), search::short_error(error));
    Health::update(|h| h.apply_read_error(source_id, kind, text, crate::library::now()));
}

/// «30 min», «6 h», «3 días».
pub fn format_wait(secs: u64) -> String {
    if secs < HOUR {
        format!("{} min", (secs / 60).max(1))
    } else if secs < 2 * DAY {
        format!("{} h", secs.div_ceil(HOUR))
    } else {
        trf!("{} días", "{} days", secs.div_ceil(DAY))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceInfo;

    const NOW: u64 = 1_000_000;

    fn source(id: &str) -> LoadedSource {
        LoadedSource { info: SourceInfo { id: id.into(), name: id.into(), lang: "es".into(), base_url: None, supports_latest: false } }
    }

    fn fail(id: &str, kind: Kind) -> Outcome {
        Outcome { source_id: id.into(), result: Err((kind, "error".into())) }
    }

    fn ok(id: &str) -> Outcome {
        Outcome { source_id: id.into(), result: Ok(100) }
    }

    fn ids(sources: &[LoadedSource]) -> Vec<&str> {
        sources.iter().map(|s| s.info.id.as_str()).collect()
    }

    #[test]
    fn classifies_host_errors() {
        assert_eq!(Kind::classify("java.io.IOException: Cloudflare pide verificación (sin WebView no se puede resolver)"), Kind::Cloudflare);
        assert_eq!(Kind::classify("java.net.ConnectException: Failed to connect to a.com/1.2.3.4:443"), Kind::Unreachable);
        assert_eq!(Kind::classify("java.net.UnknownHostException: a.com"), Kind::DeadDomain);
        assert_eq!(Kind::classify("java.net.SocketTimeoutException: timeout"), Kind::Timeout);
        assert_eq!(Kind::classify("eu.kanade.tachiyomi.network.HttpException: HTTP error 503"), Kind::Http(503));
        assert_eq!(Kind::classify("java.lang.NoClassDefFoundError: android/webkit/Foo"), Kind::MissingApi);
        assert_eq!(Kind::classify("ext-host terminó (ver host.log)"), Kind::Host);
        assert_eq!(Kind::classify("java.lang.IllegalStateException: Fuente 12 no cargada"), Kind::Host);
        assert_eq!(Kind::classify("kotlin.Exception: otra cosa"), Kind::Other);
    }

    #[test]
    fn skips_after_repeated_failures_and_retries_when_it_expires() {
        let mut h = Health::default();
        for _ in 0..2 {
            h.apply_search(vec![ok("bien"), fail("mal", Kind::Unreachable)], NOW);
        }
        assert_eq!(ids(&h.partition(vec![source("bien"), source("mal")], NOW).1), Vec::<&str>::new(), "dos fallos aún no bastan");

        h.apply_search(vec![ok("bien"), fail("mal", Kind::Unreachable)], NOW);
        let (active, skipped) = h.partition(vec![source("bien"), source("mal")], NOW);
        assert_eq!((ids(&active), ids(&skipped)), (vec!["bien"], vec!["mal"]));

        let (active, _) = h.partition(vec![source("bien"), source("mal")], NOW + HOUR / 2);
        assert_eq!(ids(&active), vec!["bien", "mal"], "al vencer se vuelve a probar");
    }

    #[test]
    fn wait_grows_up_to_the_cap() {
        let mut h = Health::default();
        for _ in 0..3 {
            h.apply_search(vec![ok("bien"), fail("mal", Kind::Unreachable)], NOW);
        }
        assert_eq!(h.get("mal").unwrap().skip_until(), Some(NOW + HOUR / 2));
        h.apply_search(vec![ok("bien"), fail("mal", Kind::Unreachable)], NOW);
        assert_eq!(h.get("mal").unwrap().skip_until(), Some(NOW + HOUR));
        for _ in 0..40 {
            h.apply_search(vec![ok("bien"), fail("mal", Kind::Unreachable)], NOW);
        }
        assert_eq!(h.get("mal").unwrap().skip_until(), Some(NOW + 6 * HOUR));
    }

    #[test]
    fn few_sources_are_all_searched() {
        let sources: Vec<_> = (0..BATCH_SIZE).map(|i| source(&format!("s{i}"))).collect();
        let (first, rest) = Health::default().first_batch(sources, &HashSet::new());
        assert_eq!((first.len(), rest.len()), (BATCH_SIZE, 0));
    }

    #[test]
    fn with_many_sources_the_first_batch_has_the_most_reliable() {
        let mut h = Health::default();
        h.sources.entry("rapida".into()).or_default().on_ok(Stage::Search, 100, NOW);
        h.sources.entry("lenta".into()).or_default().on_ok(Stage::Search, 5000, NOW);
        // Tan rápida como la que más, pero ahora mismo falla
        h.sources.entry("rota".into()).or_default().on_ok(Stage::Search, 50, NOW);
        h.apply_read_error("rota", Kind::Http(500), "e".into(), NOW);

        let mut sources: Vec<_> = (0..BATCH_SIZE).map(|i| source(&format!("nueva{i}"))).collect();
        sources.extend(["rota", "lenta", "rapida", "favorita"].map(source));
        let favorites = HashSet::from(["favorita"]);
        let (first, rest) = h.first_batch(sources, &favorites);

        assert_eq!(ids(&first)[..3], ["favorita", "rapida", "lenta"]);
        assert_eq!(first.len(), BATCH_SIZE);
        assert_eq!(rest.len(), 4);
        assert_eq!(ids(&rest).last(), Some(&"rota"), "las que dan problemas, al final");
    }

    #[test]
    fn being_rate_limited_backs_off_at_the_first_answer() {
        let mut h = Health::default();
        h.apply_search(vec![fail("a", Kind::Http(429)), fail("b", Kind::Http(500))], NOW);
        assert_eq!(h.get("a").unwrap().skip_until(), Some(NOW + HOUR));
        assert_eq!(h.get("b").unwrap().skip_until(), None);
        assert_eq!(Kind::classify("eu.kanade.tachiyomi.network.HttpException: HTTP error 429"), Kind::Http(429));
    }

    #[test]
    fn being_offline_does_not_blame_the_sources() {
        let mut h = Health::default();
        for _ in 0..5 {
            h.apply_search(vec![fail("a", Kind::DeadDomain), fail("b", Kind::Unreachable), fail("c", Kind::Timeout)], NOW);
        }
        assert!(h.get("a").is_none() && h.get("b").is_none() && h.get("c").is_none());

        // Lo que no es de red sí cuenta, aunque nadie haya respondido
        h.apply_search(vec![fail("a", Kind::DeadDomain), fail("d", Kind::MissingApi)], NOW);
        assert!(h.get("a").is_none());
        assert_eq!(h.get("d").unwrap().fails, 1);
    }

    #[test]
    fn host_crash_is_not_the_sources_fault() {
        let mut h = Health::default();
        h.apply_search(vec![ok("bien"), fail("a", Kind::Host)], NOW);
        h.apply_read_error("b", Kind::Host, "e".into(), NOW);
        assert!(h.get("a").is_none() && h.get("b").is_none());
    }

    #[test]
    fn a_missing_chapter_does_not_count_against_the_source() {
        let mut h = Health::default();
        h.apply_read_error("a", Kind::Http(404), "e".into(), NOW);
        assert!(h.get("a").is_none());
        h.apply_read_error("a", Kind::Http(500), "e".into(), NOW);
        assert_eq!(h.get("a").unwrap().read_fails, 1);
    }

    #[test]
    fn searching_fine_does_not_hide_that_nothing_can_be_read() {
        let mut h = Health::default();
        for _ in 0..3 {
            h.apply_read_error("a", Kind::Cloudflare, "e".into(), NOW);
            h.apply_search(vec![ok("a")], NOW);
        }
        let record = h.get("a").unwrap();
        assert_eq!((record.fails, record.read_fails), (0, 3));
        assert_eq!(record.kind, Some(Kind::Cloudflare));
        assert!(h.partition(vec![source("a"), source("b")], NOW).1.iter().any(|s| s.info.id == "a"));

        h.sources.get_mut("a").unwrap().on_ok(Stage::Read, 100, NOW);
        assert!(!h.get("a").unwrap().has_problem());
        assert_eq!(h.get("a").unwrap().kind, None);
    }

    #[test]
    fn missing_api_is_retried_with_a_new_version() {
        let mut h = Health::default();
        for _ in 0..2 {
            h.apply_read_error("a", Kind::MissingApi, "e".into(), NOW);
        }
        assert!(h.get("a").unwrap().skip_until().is_some());
        h.sources.get_mut("a").unwrap().app_version = "0.0.0-vieja".into();
        assert_eq!(h.get("a").unwrap().skip_until(), None);
    }

    #[test]
    fn if_every_source_is_skipped_they_are_tried_anyway_except_disabled_ones() {
        let mut h = Health::default();
        for _ in 0..3 {
            h.apply_read_error("a", Kind::Cloudflare, "e".into(), NOW);
        }
        h.set_disabled("b", true);
        let (active, skipped) = h.partition(vec![source("a"), source("b")], NOW);
        assert_eq!((ids(&active), ids(&skipped)), (vec!["a"], vec!["b"]));
    }

    #[test]
    fn forgiving_keeps_disabled_and_latency() {
        let mut h = Health::default();
        h.sources.entry("a".into()).or_default().on_ok(Stage::Read, 200, NOW);
        for _ in 0..3 {
            h.apply_read_error("a", Kind::Other, "e".into(), NOW);
        }
        h.set_disabled("a", true);
        h.forgive("a");
        let r = h.get("a").unwrap();
        assert!(!r.has_problem() && r.disabled && r.skip_until().is_none());
        assert_eq!(r.latency_ms, Some(200));
    }

    #[test]
    fn latency_is_smoothed() {
        let mut r = Record::default();
        r.on_ok(Stage::Search, 1000, NOW);
        assert_eq!(r.latency_ms, Some(1000));
        r.on_ok(Stage::Search, 200, NOW);
        assert_eq!(r.latency_ms, Some(800));
    }

    #[test]
    fn formats_waits() {
        assert_eq!(format_wait(30), "1 min");
        assert_eq!(format_wait(30 * 60), "30 min");
        assert_eq!(format_wait(6 * HOUR), "6 h");
    }
}
