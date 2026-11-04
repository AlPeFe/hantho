//! Búsqueda global: lanza la misma consulta en las fuentes, con un límite de peticiones
//! simultáneas, y entrega los resultados por un canal a medida que cada fuente responde.

use crate::cache;
use crate::health::{self, Kind, Outcome};
use crate::host::{Host, LoadedSource};
use crate::model::Manga;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const TIMEOUT: Duration = Duration::from_secs(25);
const SEARCH_TTL: Duration = Duration::from_secs(10 * 60);
/// Peticiones de búsqueda a la vez. Con pocas fuentes no se nota; con cientos evita lanzar todas
/// las conexiones de golpe desde una misma IP.
const MAX_CONCURRENT: usize = 8;

/// Límites de una búsqueda: en total, y de una en una por dominio (varias fuentes pueden ser
/// el mismo sitio, p. ej. una por idioma).
struct Limits {
    total: Arc<Semaphore>,
    domains: HashMap<String, Arc<Semaphore>>,
}

impl Limits {
    fn new<'a>(domains: impl Iterator<Item = &'a str>) -> Limits {
        Limits {
            total: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            domains: domains.map(|d| (d.to_string(), Arc::new(Semaphore::new(1)))).collect(),
        }
    }

    /// Primero el dominio: esperando a su turno no se ocupa un hueco del total.
    fn acquire(&self, domain: &str) -> impl Future<Output = Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>> + use<> {
        let (total, domain) = (self.total.clone(), self.domains.get(domain).cloned());
        async move {
            let domain = domain?.acquire_owned().await.ok()?;
            Some((domain, total.acquire_owned().await.ok()?))
        }
    }
}

/// Dominio de una fuente; sin URL, se la trata como un sitio aparte.
fn domain_of(source: &LoadedSource) -> String {
    let Some(url) = source.info.base_url.as_deref() else { return source.info.id.clone() };
    let host = url.split("://").last().unwrap_or(url).split(['/', '?', '#']).next().unwrap_or(url);
    host.strip_prefix("www.").unwrap_or(host).to_lowercase()
}

#[derive(Debug)]
pub enum SearchEvent {
    Results { source: LoadedSource, mangas: Vec<Manga> },
    Failed { source: LoadedSource, error: String },
    Done,
}

pub fn global(host: Arc<Host>, sources: Vec<LoadedSource>, query: String) -> mpsc::UnboundedReceiver<SearchEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Primero las que suelen responder antes, para ver resultados cuanto antes
        let mut sources = sources;
        let known = health::Health::load();
        sources.sort_by_key(|s| known.get(&s.info.id).and_then(|r| r.latency_ms).unwrap_or(u64::MAX));
        let domains: Vec<String> = sources.iter().map(domain_of).collect();
        let limits = Limits::new(domains.iter().map(String::as_str));
        let tasks = sources.into_iter().zip(&domains).map(|(source, domain)| {
            let (host, tx, query) = (host.clone(), tx.clone(), query.clone());
            let turn = limits.acquire(domain);
            tokio::spawn(async move {
                // Repetir la misma búsqueda en poco tiempo no vuelve a pegarle a los sitios
                let key = format!("{}|{}", source.info.id, query.trim().to_lowercase());
                if let Some(mangas) = cache::get::<Vec<Manga>>("search", &key, SEARCH_TTL) {
                    let _ = tx.send(SearchEvent::Results { source, mangas });
                    return None;
                }
                let _permits = turn.await;
                let source_id = source.info.id.clone();
                let started = Instant::now();
                let (event, result) = match tokio::time::timeout(TIMEOUT, host.search(&source_id, &query, 1)).await {
                    Ok(Ok(page)) => {
                        cache::put("search", &key, &page.mangas);
                        (SearchEvent::Results { source, mangas: page.mangas }, Ok(started.elapsed().as_millis() as u64))
                    }
                    Ok(Err(e)) => {
                        let (raw, error) = (e.to_string(), short_error(&e.to_string()));
                        (SearchEvent::Failed { source, error: error.clone() }, Err((Kind::classify(&raw), error)))
                    }
                    Err(_) => {
                        let error: String = tr!("tiempo de espera agotado", "timed out").into();
                        (SearchEvent::Failed { source, error: error.clone() }, Err((Kind::Timeout, error)))
                    }
                };
                let _ = tx.send(event);
                Some(Outcome { source_id, result })
            })
        });
        let outcomes = futures::future::join_all(tasks).await.into_iter().filter_map(|r| r.ok().flatten()).collect();
        // De una vez al final: así se distingue una fuente rota de haberse quedado sin conexión
        health::record_search(outcomes);
        let _ = tx.send(SearchEvent::Done);
    });
    rx
}

/// Los errores vienen con el nombre completo de la excepción Java; nos quedamos con el mensaje.
pub fn short_error(e: &str) -> String {
    // Fallos de red: decir qué web no responde (y si es de Cloudflare, que puede estar bloqueada)
    if let Some(rest) = e.split("Failed to connect to ").nth(1) {
        let target = rest.split_whitespace().next().unwrap_or(rest);
        let (host, ip) = target.split_once('/').unwrap_or((target, ""));
        let cloudflare = ["188.114.96.", "188.114.97.", "104.16.", "104.17.", "104.18.", "104.19.", "104.20.", "104.21.", "172.64.", "172.65.", "172.66.", "172.67."]
            .iter()
            .any(|p| ip.starts_with(p));
        return if cloudflare {
            trf!("{host} no responde (está en Cloudflare: puede que tu red la esté bloqueando)", "{host} isn't responding (it's behind Cloudflare: your network may be blocking it)")
        } else {
            trf!("{host} no responde (caída o bloqueada)", "{host} isn't responding (down or blocked)")
        };
    }
    if e.contains("SocketTimeoutException") || e.contains("timed out") || e.contains("timeout") {
        return tr!("la web tarda demasiado en responder", "the site took too long to respond").into();
    }
    if e.contains("UnknownHostException") {
        return tr!("no se encuentra la web (¿dominio cambiado o sin conexión?)", "site not found (domain changed, or no connection?)").into();
    }
    if let Some(code) = e.split("HTTP error ").nth(1).and_then(|c| c.get(..3)) {
        return trf!("la web ha respondido con un error {code}", "the site answered with error {code}");
    }
    if e.contains("NoClassDefFoundError") || e.contains("ClassNotFoundException") || e.contains("NoSuchMethodError") || e.contains("NoSuchFieldError") {
        return tr!("la extensión usa algo de Android que Han Thỏ aún no tiene (ver host.log)", "the extension needs an Android API Han Thỏ doesn't have yet (see host.log)").into();
    }
    let msg = e.rsplit_once("Exception: ").map(|(_, m)| m).unwrap_or(e);
    msg.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceInfo;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn source(id: &str, url: Option<&str>) -> LoadedSource {
        LoadedSource { info: SourceInfo { id: id.into(), name: id.into(), lang: "es".into(), base_url: url.map(String::from), supports_latest: false } }
    }

    #[test]
    fn domains() {
        assert_eq!(domain_of(&source("1", Some("https://www.Ejemplo.com/manga?x=1"))), "ejemplo.com");
        assert_eq!(domain_of(&source("2", Some("https://ejemplo.com"))), "ejemplo.com");
        assert_eq!(domain_of(&source("3", None)), "3");
    }

    /// Lanza una tarea por dominio de la lista y devuelve cuántas han corrido a la vez como máximo.
    async fn peak_concurrency(domains: Vec<String>) -> usize {
        let limits = Limits::new(domains.iter().map(String::as_str));
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let tasks = domains.iter().map(|d| {
            let (turn, running, peak) = (limits.acquire(d), running.clone(), peak.clone());
            tokio::spawn(async move {
                let _permits = turn.await.unwrap();
                peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
            })
        });
        futures::future::join_all(tasks).await;
        peak.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn never_more_than_the_limit_at_once() {
        let domains = (0..MAX_CONCURRENT * 3).map(|i| format!("sitio{i}.com")).collect();
        assert_eq!(peak_concurrency(domains).await, MAX_CONCURRENT);
    }

    #[tokio::test]
    async fn a_few_sources_all_run_at_once() {
        let domains = (0..4).map(|i| format!("sitio{i}.com")).collect();
        assert_eq!(peak_concurrency(domains).await, 4);
    }

    #[tokio::test]
    async fn one_at_a_time_per_domain() {
        assert_eq!(peak_concurrency(vec!["mismo.com".to_string(); 5]).await, 1);
    }

    #[test]
    fn friendly_errors() {
        let cf = short_error("java.net.ConnectException: Failed to connect to mangafire.to/188.114.96.5:443");
        assert!(cf.starts_with("mangafire.to") && cf.contains("Cloudflare"), "{cf}");
        let other = short_error("java.net.ConnectException: Failed to connect to es.mi2manga.com/80.97.124.233:443");
        assert!(other.starts_with("es.mi2manga.com") && !other.contains("Cloudflare"), "{other}");
        assert!(short_error("eu.kanade.tachiyomi.network.HttpException: HTTP error 404").contains("404"));
        assert!(short_error("java.lang.NoClassDefFoundError: android/webkit/CookieManager").contains("Android"));
        assert_eq!(short_error("java.io.IOException: Cloudflare pide verificación"), "Cloudflare pide verificación");
    }
}

/// Normaliza un título para agrupar el mismo manga visto en varias fuentes.
pub fn normalize(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}
