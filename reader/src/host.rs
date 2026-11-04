//! Cliente de ext-host: lanza la JVM y habla JSON (una línea por mensaje) por stdin/stdout.
//! Los pedidos se multiplexan por id, así que se pueden hacer muchos en paralelo.

use crate::cache;
use crate::ext::Installed;
use crate::health;
use crate::model::{Chapter, Manga, MangaDetails, MangasPage, Page, SourceInfo};
use crate::paths;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

const DETAILS_TTL: Duration = Duration::from_secs(30 * 60);
/// Cuánto se espera a una fuente al abrir un manga o capítulo.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Al recargar a propósito (`r`) se da más margen: puede ser un manga enorme en una fuente lenta.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(90);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT_ERROR: &str = "timed out";

pub struct Host {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Child,
}

/// Una fuente cargada en el host.
#[derive(Debug, Clone)]
pub struct LoadedSource {
    pub info: SourceInfo,
}

impl Host {
    pub async fn spawn() -> Result<Arc<Host>> {
        let lib = find_host_lib()?;
        std::fs::create_dir_all(paths::home())?;
        let log = std::fs::File::create(paths::host_log())?;
        let mut child = Command::new(find_java())
            // Arranque rápido: el host hace poco cómputo, el JIT completo no compensa
            .args(["-XX:+UseSerialGC", "-XX:TieredStopAtLevel=1", "-Xshare:auto", "-Dfile.encoding=UTF-8", "-cp"])
            .arg(lib.join("*"))
            .arg("hantho.host.MainKt")
            .env("HANTHO_HOME", paths::home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .context(tr!("no se ha podido lanzar java (¿JAVA_HOME?)", "couldn't start java (JAVA_HOME?)"))?;

        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Arc::default();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = msg["id"].as_u64() else { continue };
                let result = if msg["ok"].as_bool() == Some(true) {
                    Ok(msg["result"].clone())
                } else {
                    Err(anyhow!("{}", msg["error"].as_str().unwrap_or(tr!("error desconocido", "unknown error"))))
                };
                if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
            // El host murió: fallar todo lo pendiente en vez de colgarse
            for (_, tx) in reader_pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!(trf!("ext-host terminó (ver {})", "ext-host exited (see {})", paths::host_log().display()))));
            }
        });

        Ok(Arc::new(Host {
            stdin: tokio::sync::Mutex::new(child.stdin.take().unwrap()),
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        }))
    }

    pub async fn call<T: DeserializeOwned>(&self, op: &str, mut params: Value) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        params["id"] = json!(id);
        params["op"] = json!(op);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let mut line = serde_json::to_string(&params)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        let value = rx.await.map_err(|_| anyhow!(tr!("ext-host no respondió", "ext-host didn't respond")))??;
        Ok(serde_json::from_value(value)?)
    }

    /// Carga todas las extensiones instaladas en paralelo. Devuelve las fuentes y los errores por extensión.
    pub async fn load_all(self: &Arc<Self>, exts: &[Installed]) -> (Vec<LoadedSource>, Vec<(String, String)>) {
        let tasks = exts.iter().cloned().map(|ext| {
            let host = self.clone();
            async move {
                let res: Result<Value> =
                    host.call("load", json!({ "jar": ext.jar_path().to_string_lossy() })).await;
                (ext, res)
            }
        });
        let mut sources = Vec::new();
        let mut errors = Vec::new();
        for (ext, res) in futures::future::join_all(tasks).await {
            match res.and_then(|v| Ok(serde_json::from_value::<Vec<SourceInfo>>(v["sources"].clone())?)) {
                Ok(list) => sources.extend(list.into_iter().map(|info| LoadedSource { info })),
                Err(e) => errors.push((ext.name.clone(), e.to_string())),
            }
        }
        (sources, errors)
    }

    pub async fn search(&self, source: &str, query: &str, page: u32) -> Result<MangasPage> {
        self.call("search", json!({ "source": source, "query": query, "page": page })).await
    }

    /// Detalles y capítulos. Cacheados 30 min para no repetir pedidos al volver a un manga;
    /// `refresh` fuerza ir a la fuente (para ver capítulos nuevos).
    ///
    /// No espera más de `REQUEST_TIMEOUT` (`REFRESH_TIMEOUT` con `refresh`). La petición sigue en
    /// segundo plano y se cachea si acaba llegando: los mangas con muchos capítulos en fuentes
    /// lentas se abren al reintentar, y una respuesta tardía demuestra que la fuente funciona.
    pub async fn details(self: &Arc<Self>, source: &str, manga: &Manga, refresh: bool) -> Result<MangaDetails> {
        let key = format!("{source}|{}", manga.url);
        let cached = if refresh { None } else { cache::get::<MangaDetails>("details", &key, DETAILS_TTL) };
        let mut details = match cached {
            Some(hit) => hit,
            None => {
                let abandoned = Arc::new(AtomicBool::new(false));
                let started = Instant::now();
                let request = {
                    let (host, source, manga, abandoned) = (self.clone(), source.to_string(), manga.clone(), abandoned.clone());
                    tokio::spawn(async move {
                        let res: Result<MangaDetails> = host.call("manga", json!({ "source": source, "manga": manga })).await;
                        match &res {
                            Ok(fresh) => {
                                cache::put("details", &key, fresh);
                                health::record_read_ok(&source, started.elapsed().as_millis() as u64);
                            }
                            // Si ya se contó como timeout, no contarlo dos veces
                            Err(e) if !abandoned.load(Ordering::Relaxed) => health::record_read_error(&source, &e.to_string()),
                            Err(_) => {}
                        }
                        res
                    })
                };
                let limit = if refresh { REFRESH_TIMEOUT } else { REQUEST_TIMEOUT };
                match tokio::time::timeout(limit, request).await {
                    Ok(joined) => joined??,
                    Err(_) => {
                        abandoned.store(true, Ordering::Relaxed);
                        health::record_read_error(source, TIMEOUT_ERROR);
                        bail!(TIMEOUT_ERROR);
                    }
                }
            }
        };
        // Fuentes que no informan el número: deducirlo del nombre ("Capítulo 104")
        for c in details.chapters.iter_mut().filter(|c| c.chapter_number < 0.0) {
            if let Some(n) = crate::model::chapter_number_from_name(&c.name) {
                c.chapter_number = n;
            }
        }
        Ok(details)
    }

    pub async fn pages(&self, source: &str, chapter: &Chapter) -> Result<Vec<Page>> {
        let started = Instant::now();
        let request = self.call("pages", json!({ "source": source, "chapter": chapter }));
        let res = tokio::time::timeout(REQUEST_TIMEOUT, request).await.unwrap_or_else(|_| Err(anyhow!(TIMEOUT_ERROR)));
        match &res {
            Ok(_) => health::record_read_ok(source, started.elapsed().as_millis() as u64),
            Err(e) => health::record_read_error(source, &e.to_string()),
        }
        res
    }

    /// Descarga la imagen de una página (a través de la extensión) al archivo `out`.
    /// Un fallo aquí suele ser de esa página, así que no cuenta para la salud de la fuente.
    pub async fn image(&self, source: &str, page: &Page, out: &Path) -> Result<()> {
        let request = self.call::<Value>("image", json!({ "source": source, "page": page, "out": out }));
        tokio::time::timeout(IMAGE_TIMEOUT, request).await.map_err(|_| anyhow!(TIMEOUT_ERROR))??;
        Ok(())
    }
}

fn find_java() -> PathBuf {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let java = PathBuf::from(home).join("bin").join(if cfg!(windows) { "java.exe" } else { "java" });
        if java.exists() {
            return java;
        }
    }
    PathBuf::from("java")
}

/// Directorio `lib` de ext-host: `$HANTHO_HOST_LIB`, o `ext-host/build/install/ext-host/lib`
/// buscando hacia arriba desde el ejecutable (cubre `cargo run` dentro del repo).
fn find_host_lib() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HANTHO_HOST_LIB") {
        return Ok(PathBuf::from(dir));
    }
    let exe = std::env::current_exe()?;
    for dir in exe.ancestors() {
        for candidate in [dir.join("ext-host").join("lib"), dir.join("ext-host/build/install/ext-host/lib")] {
            if candidate.join("ext-host.jar").exists() {
                return Ok(candidate);
            }
        }
    }
    bail!(tr!(
        "no se ha encontrado ext-host; compílalo con `gradlew installDist` o define HANTHO_HOST_LIB",
        "ext-host not found; build it with `gradlew installDist` or set HANTHO_HOST_LIB"
    ))
}
