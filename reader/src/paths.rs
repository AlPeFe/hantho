//! Rutas de datos. Todo vive en `~/.hantho` (o `$HANTHO_HOME`), compartido con ext-host.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HANTHO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().expect("sin directorio home").join(".hantho"))
}

/// El proyecto antes se llamaba "mangaterm": si existe `~/.mangaterm` y todavía no hay
/// `~/.hantho`, se mueve tal cual (extensiones, biblioteca, sesión de AniList y caché).
pub fn migrate_legacy() {
    if std::env::var_os("HANTHO_HOME").is_some() {
        return;
    }
    let Some(user_home) = dirs::home_dir() else { return };
    let (old, new) = (user_home.join(".mangaterm"), home());
    if old.is_dir() && !new.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => eprintln!("{}", trf!("✿ datos movidos de {} a {}", "✿ data moved from {} to {}", old.display(), new.display())),
            Err(e) => eprintln!("{}", trf!("no se han podido mover los datos de {}: {e}", "couldn't move the data from {}: {e}", old.display())),
        }
    }
}

/// Repositorios de extensiones que ha añadido el usuario.
pub fn repos_file() -> PathBuf {
    home().join("repos.json")
}

/// Índices descargados de cada repositorio.
pub fn repos_dir() -> PathBuf {
    home().join("repos")
}

pub fn extensions_dir() -> PathBuf {
    home().join("extensions")
}

pub fn installed_file() -> PathBuf {
    extensions_dir().join("installed.json")
}

pub fn pages_cache() -> PathBuf {
    home().join("cache").join("pages")
}

pub fn host_log() -> PathBuf {
    home().join("host.log")
}

pub fn library_file() -> PathBuf {
    home().join("library.json")
}

pub fn read_file() -> PathBuf {
    home().join("read.json")
}

/// Credenciales de AniList (token OAuth). Es un secreto: no compartir este archivo.
pub fn anilist_file() -> PathBuf {
    home().join("anilist.json")
}

/// Client ID de AniList configurado localmente (si el binario no trae uno).
pub fn anilist_client_file() -> PathBuf {
    home().join("anilist-client-id")
}

/// Caché en disco de respuestas (AniList, detalles de manga).
pub fn http_cache() -> PathBuf {
    home().join("cache").join("api")
}

/// Escribe a un temporal y renombra: un corte a mitad no deja el archivo corrupto.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Capítulos por leer de los favoritos, según la última comprobación.
pub fn news_file() -> PathBuf {
    home().join("new_chapters.json")
}

/// Qué capítulos se han descargado y dónde.
pub fn downloads_file() -> PathBuf {
    home().join("downloads.json")
}

/// Qué fuentes funcionan y cuáles fallan (ver `health.rs`).
pub fn source_health_file() -> PathBuf {
    home().join("source_health.json")
}

/// Preferencias de la TUI (ajuste del lector, idiomas de búsqueda).
pub fn prefs_file() -> PathBuf {
    home().join("prefs.json")
}
