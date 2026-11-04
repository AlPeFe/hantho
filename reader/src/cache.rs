//! Caché en disco con vencimiento, y limitador de pedidos (token bucket).
//! Sirven para no castigar a AniList ni a las fuentes repitiendo los mismos pedidos.

use crate::paths;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    saved_at: u64,
    value: T,
}

fn entry_path(namespace: &str, key: &str) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    paths::http_cache().join(namespace).join(format!("{:016x}.json", h.finish()))
}

/// Devuelve el valor guardado si existe y tiene menos de `ttl`.
pub fn get<T: DeserializeOwned>(namespace: &str, key: &str, ttl: Duration) -> Option<T> {
    let bytes = std::fs::read(entry_path(namespace, key)).ok()?;
    let entry: Entry<T> = serde_json::from_slice(&bytes).ok()?;
    (crate::library::now().saturating_sub(entry.saved_at) < ttl.as_secs()).then_some(entry.value)
}

pub fn put<T: Serialize>(namespace: &str, key: &str, value: &T) {
    let path = entry_path(namespace, key);
    let entry = Entry { saved_at: crate::library::now(), value };
    if let (Some(dir), Ok(bytes)) = (path.parent(), serde_json::to_vec(&entry)) {
        let _ = std::fs::create_dir_all(dir);
        // Temporal único + rename: otro proceso (TUI, MCP) nunca lee un archivo a medio escribir
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Lo que lleve este tiempo sin tocarse se borra al abrir la TUI: si no, las cachés crecen sin fin.
const SWEEP_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Borra las páginas, respuestas y descargas a medias antiguas.
pub fn sweep() {
    let cache = paths::home().join("cache");
    remove_older_than(&paths::pages_cache(), SWEEP_AFTER);
    remove_older_than(&cache.join("downloads"), SWEEP_AFTER);
    let namespaces = std::fs::read_dir(paths::http_cache()).into_iter().flatten().flatten();
    for namespace in namespaces.filter(|e| e.path().is_dir()) {
        remove_older_than(&namespace.path(), SWEEP_AFTER);
    }
}

fn remove_older_than(dir: &std::path::Path, age: Duration) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|elapsed| elapsed >= age);
        if old {
            let path = entry.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

/// Token bucket: permite ráfagas de hasta `burst` pedidos y después uno cada `interval`.
pub struct RateLimiter {
    state: tokio::sync::Mutex<(f64, Instant)>,
    burst: f64,
    interval: Duration,
}

impl RateLimiter {
    pub fn new(burst: u32, interval: Duration) -> Self {
        RateLimiter { state: tokio::sync::Mutex::new((burst as f64, Instant::now())), burst: burst as f64, interval }
    }

    /// Espera hasta que haya cupo para un pedido.
    pub async fn acquire(&self) {
        let mut state = self.state.lock().await;
        let (tokens, last) = &mut *state;
        let refill = last.elapsed().as_secs_f64() / self.interval.as_secs_f64();
        *tokens = (*tokens + refill).min(self.burst);
        *last = Instant::now();
        if *tokens < 1.0 {
            let wait = self.interval.mul_f64(1.0 - *tokens);
            tokio::time::sleep(wait).await;
            *tokens = 1.0;
            *last = Instant::now();
        }
        *tokens -= 1.0;
    }

    /// Vacía el cupo y bloquea durante `pause` (cuando el servidor pide frenar).
    pub async fn back_off(&self, pause: Duration) {
        let mut state = self.state.lock().await;
        tokio::time::sleep(pause).await;
        *state = (0.0, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_expiry() {
        put("test", "clave", &vec![1, 2, 3]);
        assert_eq!(get::<Vec<i32>>("test", "clave", Duration::from_secs(60)), Some(vec![1, 2, 3]));
        assert_eq!(get::<Vec<i32>>("test", "clave", Duration::ZERO), None);
        assert_eq!(get::<Vec<i32>>("test", "otra", Duration::from_secs(60)), None);
    }

    #[test]
    fn sweep_removes_only_what_is_old() {
        let dir = std::env::temp_dir().join(format!("hantho-sweep-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("vieja")).unwrap();
        std::fs::write(dir.join("nuevo.json"), b"x").unwrap();
        std::fs::write(dir.join("vieja/0001"), b"x").unwrap();
        let viejo = dir.join("viejo.json");
        std::fs::write(&viejo, b"x").unwrap();
        let hace_un_ano = std::time::SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60);
        std::fs::File::options().write(true).open(&viejo).unwrap().set_modified(hace_un_ano).unwrap();

        remove_older_than(&dir, SWEEP_AFTER);
        assert!(!viejo.exists() && dir.join("nuevo.json").exists() && dir.join("vieja/0001").exists());

        remove_older_than(&dir, Duration::ZERO);
        assert!(!dir.join("nuevo.json").exists() && !dir.join("vieja").exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn limiter_allows_burst_then_spaces_requests() {
        let limiter = RateLimiter::new(3, Duration::from_millis(100));
        let start = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "la ráfaga no debería esperar");
        limiter.acquire().await;
        limiter.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(180), "pasada la ráfaga, uno cada 100 ms");
    }
}
