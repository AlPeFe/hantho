//! Descarga de capítulos a una carpeta local (también unidades de red montadas) o remota (con rclone).
//!
//! Cada capítulo queda en `<destino>/<manga>/<capítulo>.cbz`, o como carpeta de imágenes. Un capítulo
//! con errores no se guarda a medias, salvo imágenes sueltas que ya no existen en la web (se avisa).
//! Descarga despacio para no castigar a las fuentes.

use crate::health::Kind;
use crate::host::Host;
use crate::model::{Chapter, Page};
use crate::paths;
use crate::search;
use anyhow::{anyhow, bail, Context, Result};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Páginas a la vez de un mismo capítulo. Los capítulos van de uno en uno.
const PAGE_CONCURRENCY: usize = 2;
const PAUSE_BETWEEN_CHAPTERS: Duration = Duration::from_secs(1);
const PAGE_RETRY_WAIT: Duration = Duration::from_secs(2);
/// Fallos seguidos tras los que se deja de insistir a esa fuente.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const CANCELLED: &str = "cancelled";

/// Si faltan demasiadas imágenes para guardar el capítulo. Algunas webs meten en cada capítulo una
/// página que siempre da 404 (MangaTV, p. ej.): por una suelta no se pierde el capítulo.
fn too_many_missing(missing: usize, pages: usize) -> bool {
    missing == pages || missing > (pages / 10).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Un archivo por capítulo, compatible con casi cualquier lector de cómics.
    #[default]
    Cbz,
    /// Una carpeta de imágenes por capítulo.
    Folder,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Cbz => "CBZ",
            Format::Folder => tr!("carpeta", "folder"),
        }
    }

    pub fn toggled(self) -> Format {
        match self {
            Format::Cbz => Format::Folder,
            Format::Folder => Format::Cbz,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    Local(PathBuf),
    /// Remoto de rclone, p. ej. `nas:manga`.
    Rclone(String),
}

impl Dest {
    /// `rclone:<remoto>:<ruta>` es un remoto; cualquier otra cosa, una carpeta (`\\nas\manga`, `/mnt/nas`…).
    pub fn parse(text: &str) -> Result<Dest> {
        let text = text.trim();
        if text.is_empty() {
            bail!(tr!("indica una carpeta de destino", "give a destination folder"));
        }
        match text.strip_prefix("rclone:") {
            Some(remote) if remote.starts_with('-') || !remote.contains(':') => {
                bail!(tr!("el destino remoto se escribe rclone:<remoto>:<ruta>, p. ej. rclone:nas:manga", "a remote destination is written rclone:<remote>:<path>, e.g. rclone:nas:manga"))
            }
            Some(remote) => Ok(Dest::Rclone(remote.to_string())),
            None => Ok(Dest::Local(PathBuf::from(text))),
        }
    }
}

/// Carpeta por defecto: `Han Tho` dentro de Descargas.
pub fn default_dest() -> String {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("Han Tho").to_string_lossy().into_owned()
}

/// Convierte un título de la web en un nombre de archivo válido en Linux, Windows y remotos.
/// Los títulos los decide la fuente: no pueden salirse de la carpeta de destino.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned: String = cleaned.trim_matches(|c: char| c == '.' || c == ' ').chars().take(120).collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return "_".into();
    }
    let stem = cleaned.split('.').next().unwrap_or(cleaned).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{cleaned}") } else { cleaned.to_string() }
}

fn short_hash(text: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:06x}", h.finish() & 0xff_ffff)
}

/// Nombre de archivo de cada capítulo, sin repetidos (el mismo capítulo de varios scanlators).
pub fn chapter_stems(chapters: &[Chapter]) -> Vec<String> {
    fn count(names: &[String]) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for n in names {
            *counts.entry(n.to_lowercase()).or_default() += 1;
        }
        counts
    }
    let mut stems: Vec<String> = chapters.iter().map(|c| sanitize(&c.name)).collect();
    let counts = count(&stems);
    for (stem, c) in stems.iter_mut().zip(chapters) {
        if counts[&stem.to_lowercase()] > 1
            && let Some(scanlator) = c.scanlator.as_ref().filter(|s| !s.is_empty())
        {
            *stem = sanitize(&format!("{} [{scanlator}]", c.name));
        }
    }
    let counts = count(&stems);
    for (stem, c) in stems.iter_mut().zip(chapters) {
        if counts[&stem.to_lowercase()] > 1 {
            *stem = format!("{stem} {}", short_hash(&c.url));
        }
    }
    stems
}

fn remote_join(remote: &str, parts: &[&str]) -> String {
    let mut out = remote.trim_end_matches('/').to_string();
    for part in parts {
        if !out.ends_with(':') {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}

/// Qué capítulos están descargados y dónde (`~/.hantho/downloads.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Manifest {
    /// "fuente|url del manga" → (url del capítulo → destino)
    mangas: BTreeMap<String, BTreeMap<String, String>>,
}

impl Manifest {
    pub fn load() -> Manifest {
        std::fs::read(paths::downloads_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(paths::home())?;
        let tmp = paths::downloads_file().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, paths::downloads_file())?;
        Ok(())
    }

    pub fn contains(&self, manga_key: &str, chapter_url: &str) -> bool {
        self.mangas.get(manga_key).is_some_and(|m| m.contains_key(chapter_url))
    }

    /// Si ese capítulo ya está en ese destino. Lo local se comprueba; lo remoto, no se puede sin red.
    fn has(&self, manga_key: &str, chapter_url: &str, place: &str) -> bool {
        let known = self.mangas.get(manga_key).and_then(|m| m.get(chapter_url)).is_some_and(|p| p == place);
        known && (place.starts_with("rclone:") || Path::new(place).exists())
    }

    fn insert(&mut self, manga_key: &str, chapter_url: &str, place: String) {
        self.mangas.entry(manga_key.to_string()).or_default().insert(chapter_url.to_string(), place);
    }
}

pub struct Job {
    pub source_id: String,
    pub manga_title: String,
    pub manga_key: String,
    /// Capítulo y nombre de archivo, del primero al último.
    pub chapters: Vec<(Chapter, String)>,
    pub dest: Dest,
    pub format: Format,
    pub force: bool,
    pub cancel: Arc<AtomicBool>,
}

impl Job {
    /// `selected`: índices en `all`, la lista completa de capítulos del manga.
    pub fn new(source_id: &str, manga_url: &str, manga_title: &str, all: &[Chapter], selected: &[usize], dest: Dest, format: Format) -> Job {
        let stems = chapter_stems(all);
        let mut chapters: Vec<(Chapter, String)> = selected.iter().map(|&i| (all[i].clone(), stems[i].clone())).collect();
        chapters.sort_by(|a, b| a.0.chapter_number.total_cmp(&b.0.chapter_number));
        Job {
            source_id: source_id.to_string(),
            manga_title: sanitize(manga_title),
            manga_key: crate::reads::key(source_id, manga_url),
            chapters,
            dest,
            format,
            force: false,
            cancel: Arc::default(),
        }
    }

    fn file_name(&self, stem: &str) -> String {
        match self.format {
            Format::Cbz => format!("{stem}.cbz"),
            Format::Folder => stem.to_string(),
        }
    }

    /// Dónde queda el capítulo, tal como se guarda en el manifiesto.
    fn place(&self, stem: &str) -> String {
        let name = self.file_name(stem);
        match &self.dest {
            Dest::Local(dir) => dir.join(&self.manga_title).join(name).to_string_lossy().into_owned(),
            Dest::Rclone(remote) => format!("rclone:{}", remote_join(remote, &[&self.manga_title, &name])),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub saved: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Imágenes que no existen en la web, en capítulos que sí se han guardado.
    pub missing_pages: usize,
    pub cancelled: bool,
    /// Por qué se ha dejado de descargar antes de tiempo.
    pub stopped: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    Progress { chapter: usize, chapters: usize, name: String, page: usize, pages: usize },
    /// `missing`: números de las páginas que no existen en la web.
    Saved { place: String, missing: Vec<usize> },
    Failed { name: String, error: String },
    Finished(Summary),
}

pub fn start(host: Arc<Host>, job: Job) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(host, job, tx));
    rx
}

async fn run(host: Arc<Host>, job: Job, tx: mpsc::UnboundedSender<Event>) {
    let mut summary = Summary::default();
    if let Err(e) = check_dest(&job).await {
        summary.stopped = Some(e.to_string());
        let _ = tx.send(Event::Finished(summary));
        return;
    }
    let mut consecutive = 0;
    let total = job.chapters.len();
    for (index, (chapter, stem)) in job.chapters.iter().enumerate() {
        if job.cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        let place = job.place(stem);
        if !job.force && Manifest::load().has(&job.manga_key, &chapter.url, &place) {
            summary.skipped += 1;
            continue;
        }
        if summary.saved + summary.failed > 0 {
            tokio::time::sleep(PAUSE_BETWEEN_CHAPTERS).await;
        }
        match download_chapter(&host, &job, chapter, stem, index, total, &tx).await {
            Ok(missing) => {
                let mut manifest = Manifest::load();
                manifest.insert(&job.manga_key, &chapter.url, place.clone());
                if let Err(e) = manifest.save() {
                    summary.last_error = Some(e.to_string());
                }
                consecutive = 0;
                summary.saved += 1;
                summary.missing_pages += missing.len();
                let _ = tx.send(Event::Saved { place, missing });
            }
            Err(_) if job.cancel.load(Ordering::Relaxed) => {
                summary.cancelled = true;
                break;
            }
            Err(e) => {
                let raw = format!("{e:#}");
                // Decir qué página ha fallado: un capítulo puede tener una sola imagen rota en la web
                let error = match e.downcast_ref::<PageFailed>() {
                    Some(failed) => trf!("página {}: {}", "page {}: {}", failed.page, search::short_error(&failed.error)),
                    None => search::short_error(&raw),
                };
                consecutive += 1;
                summary.failed += 1;
                summary.last_error = Some(error.clone());
                let _ = tx.send(Event::Failed { name: chapter.name.clone(), error });
                if Kind::classify(&raw) == Kind::Http(429) {
                    summary.stopped = Some(tr!("la fuente pide frenar (429): prueba más tarde", "the source asks to slow down (429): try again later").into());
                    break;
                }
                if consecutive >= MAX_CONSECUTIVE_FAILURES {
                    summary.stopped = Some(trf!("{consecutive} capítulos seguidos han fallado", "{consecutive} chapters in a row failed"));
                    break;
                }
            }
        }
    }
    let _ = tx.send(Event::Finished(summary));
}

fn rclone_program() -> std::ffi::OsString {
    std::env::var_os("HANTHO_RCLONE").unwrap_or_else(|| "rclone".into())
}

async fn check_dest(job: &Job) -> Result<()> {
    match &job.dest {
        Dest::Local(dir) => {
            let dir = dir.join(&job.manga_title);
            tokio::fs::create_dir_all(&dir).await.with_context(|| trf!("no se puede escribir en {}", "can't write to {}", dir.display()))
        }
        Dest::Rclone(_) => match Command::new(rclone_program()).arg("version").output().await {
            Ok(out) if out.status.success() => Ok(()),
            _ => bail!(tr!("para guardar en un remoto hace falta rclone (https://rclone.org/install)", "saving to a remote needs rclone (https://rclone.org/install)")),
        },
    }
}

async fn download_chapter(host: &Host, job: &Job, chapter: &Chapter, stem: &str, index: usize, total: usize, tx: &mpsc::UnboundedSender<Event>) -> Result<Vec<usize>> {
    let progress = |page, pages| {
        let _ = tx.send(Event::Progress { chapter: index + 1, chapters: total, name: chapter.name.clone(), page, pages });
    };
    progress(0, 0);
    let pages = host.pages(&job.source_id, chapter).await?;
    if pages.is_empty() {
        bail!(tr!("el capítulo no tiene páginas", "this chapter has no pages"));
    }
    let work = paths::home().join("cache").join("downloads").join(short_hash(&format!("{}|{}", job.source_id, chapter.url)));
    let _ = tokio::fs::remove_dir_all(&work).await;
    tokio::fs::create_dir_all(&work).await?;

    let mut fetches = stream::iter(pages.iter().cloned().enumerate())
        .map(|(i, page)| fetch_page(host, job, chapter, page, i, &work))
        .buffer_unordered(PAGE_CONCURRENCY);
    let mut files = Vec::with_capacity(pages.len());
    let mut missing = Vec::new();
    while let Some(fetched) = fetches.next().await {
        if job.cancel.load(Ordering::Relaxed) {
            bail!(CANCELLED);
        }
        match fetched? {
            (_, Some(file)) => files.push(file),
            (page, None) => missing.push(page),
        }
        progress(files.len() + missing.len(), pages.len());
    }
    drop(fetches);
    if too_many_missing(missing.len(), pages.len()) {
        let _ = tokio::fs::remove_dir_all(&work).await;
        bail!(trf!("faltan {} de {} páginas en la web", "{} of {} pages are missing on the site", missing.len(), pages.len()));
    }
    files.sort();
    missing.sort();

    let result = store(job, stem, files, &work).await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result.map(|()| missing)
}

#[derive(Debug)]
struct PageFailed {
    page: usize,
    error: String,
}

impl std::fmt::Display for PageFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page {}: {}", self.page, self.error)
    }
}

impl std::error::Error for PageFailed {}

async fn fetch_page(host: &Host, job: &Job, chapter: &Chapter, page: Page, index: usize, work: &Path) -> Result<(usize, Option<PathBuf>)> {
    match try_fetch_page(host, job, chapter, page, index, work).await {
        Ok(file) => Ok((index + 1, file)),
        Err(e) => Err(PageFailed { page: index + 1, error: e.to_string() }.into()),
    }
}

/// Baja una página (o la toma de la caché del lector) y comprueba que sea una imagen.
/// `None` si esa imagen no existe en la web: reintentar no serviría de nada.
async fn try_fetch_page(host: &Host, job: &Job, chapter: &Chapter, page: Page, index: usize, work: &Path) -> Result<Option<PathBuf>> {
    if job.cancel.load(Ordering::Relaxed) {
        bail!(CANCELLED);
    }
    let raw = work.join(format!("{index:04}.raw"));
    let cached = crate::tui::page_cache_path(&job.source_id, &chapter.url, index);
    if cached.exists() {
        tokio::fs::copy(&cached, &raw).await?;
    } else {
        // Un archivo por intento: tras un timeout, el host puede seguir escribiendo en el anterior
        let first = work.join(format!("{index:04}.0.part"));
        match host.image(&job.source_id, &page, &first).await {
            Ok(()) => tokio::fs::rename(&first, &raw).await?,
            Err(e) if matches!(Kind::classify(&e.to_string()), Kind::Http(404) | Kind::Http(410)) => return Ok(None),
            Err(e) if Kind::classify(&e.to_string()) == Kind::Http(429) => return Err(e),
            Err(_) => {
                tokio::time::sleep(PAGE_RETRY_WAIT).await;
                let second = work.join(format!("{index:04}.1.part"));
                host.image(&job.source_id, &page, &second).await?;
                tokio::fs::rename(&second, &raw).await?;
            }
        }
    }
    let bytes = tokio::fs::read(&raw).await?;
    let format = image::guess_format(&bytes).map_err(|_| anyhow!(trf!("la página {} no es una imagen", "page {} isn't an image", index + 1)))?;
    let extension = format.extensions_str().first().copied().unwrap_or("img");
    let file = work.join(format!("{:04}.{extension}", index + 1));
    tokio::fs::rename(&raw, &file).await?;
    Ok(Some(file))
}

async fn store(job: &Job, stem: &str, files: Vec<PathBuf>, work: &Path) -> Result<()> {
    let name = job.file_name(stem);
    match (&job.dest, job.format) {
        (Dest::Local(dir), Format::Cbz) => {
            let dir = dir.join(&job.manga_title);
            let (part, target) = (dir.join(format!("{name}.part")), dir.join(&name));
            let path = part.clone();
            tokio::task::spawn_blocking(move || write_cbz(&files, &path)).await??;
            // Renombrar al final: nunca queda un .cbz a medias con su nombre definitivo
            tokio::fs::rename(&part, &target).await?;
        }
        (Dest::Local(dir), Format::Folder) => {
            let target = dir.join(&job.manga_title).join(&name);
            tokio::fs::create_dir_all(&target).await?;
            for file in &files {
                let Some(file_name) = file.file_name() else { continue };
                tokio::fs::copy(file, target.join(file_name)).await?;
            }
        }
        (Dest::Rclone(remote), Format::Cbz) => {
            let path = work.join(&name);
            let cbz = path.clone();
            tokio::task::spawn_blocking(move || write_cbz(&files, &cbz)).await??;
            rclone("copyto", &path, &remote_join(remote, &[&job.manga_title, &name])).await?;
        }
        (Dest::Rclone(remote), Format::Folder) => {
            let folder = work.join(&name);
            tokio::fs::create_dir_all(&folder).await?;
            for file in &files {
                let Some(file_name) = file.file_name() else { continue };
                tokio::fs::rename(file, folder.join(file_name)).await?;
            }
            rclone("copy", &folder, &remote_join(remote, &[&job.manga_title, &name])).await?;
        }
    }
    Ok(())
}

async fn rclone(command: &str, from: &Path, to: &str) -> Result<()> {
    let out = Command::new(rclone_program())
        .args([command, "--"])
        .arg(from)
        .arg(to)
        .output()
        .await
        .context(tr!("no se ha podido lanzar rclone", "couldn't start rclone"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("rclone: {}", stderr.lines().last().unwrap_or("error"));
    }
    Ok(())
}

/// Las imágenes ya vienen comprimidas: se guardan tal cual, sin volver a comprimir.
fn write_cbz(files: &[PathBuf], path: &Path) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path)?);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for file in files {
        let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("nombre de página inválido"))?;
        zip.start_file(name, options)?;
        std::io::copy(&mut std::fs::File::open(file)?, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(name: &str, url: &str, scanlator: Option<&str>) -> Chapter {
        Chapter { url: url.into(), name: name.into(), scanlator: scanlator.map(String::from), ..Default::default() }
    }

    #[test]
    fn a_loose_missing_image_does_not_lose_the_chapter() {
        assert!(!too_many_missing(0, 30));
        assert!(!too_many_missing(1, 33), "la página fantasma de MangaTV");
        assert!(!too_many_missing(1, 3));
        assert!(!too_many_missing(3, 30));
        assert!(too_many_missing(4, 30));
        assert!(too_many_missing(2, 3));
        assert!(too_many_missing(1, 1), "sin ninguna imagen no hay capítulo");
    }

    #[test]
    fn parses_destinations() {
        assert_eq!(Dest::parse("  D:\\Manga ").unwrap(), Dest::Local(PathBuf::from("D:\\Manga")));
        assert_eq!(Dest::parse("/mnt/nas/manga").unwrap(), Dest::Local(PathBuf::from("/mnt/nas/manga")));
        assert_eq!(Dest::parse(r"\\nas\manga").unwrap(), Dest::Local(PathBuf::from(r"\\nas\manga")));
        assert_eq!(Dest::parse("rclone:nas:manga/es").unwrap(), Dest::Rclone("nas:manga/es".into()));
        assert!(Dest::parse("").is_err());
        assert!(Dest::parse("rclone:nas").is_err(), "falta la ruta del remoto");
        assert!(Dest::parse("rclone:--config=x:y").is_err(), "no puede parecer una opción");
    }

    #[test]
    fn titles_cannot_escape_the_destination() {
        assert_eq!(sanitize("../../etc/passwd"), "etc passwd");
        assert_eq!(sanitize(r"..\..\Windows\win.ini"), "Windows win.ini");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("   "), "_");
        assert_eq!(sanitize("C:\\x"), "C x");
        assert!(!sanitize("a/b\\c").contains(['/', '\\']));
    }

    #[test]
    fn names_are_valid_on_every_system() {
        assert_eq!(sanitize("¿Qué pasa? Cap. 1: \"inicio\""), "¿Qué pasa Cap. 1 inicio");
        assert_eq!(sanitize("final."), "final");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("com1.txt"), "_com1.txt");
        assert_eq!(sanitize("Console"), "Console");
        assert_eq!(sanitize("a\nb\tc"), "a b c");
        assert_eq!(sanitize(&"x".repeat(300)).chars().count(), 120);
    }

    #[test]
    fn repeated_chapters_get_different_names() {
        let stems = chapter_stems(&[
            chapter("Cap 1", "/a", Some("Scan A")),
            chapter("Cap 1", "/b", Some("Scan B")),
            chapter("Cap 2", "/c", None),
            chapter("Cap 3", "/d", None),
            chapter("cap 3", "/e", None),
        ]);
        assert_eq!(stems[..3], ["Cap 1 [Scan A]", "Cap 1 [Scan B]", "Cap 2"]);
        assert_ne!(stems[3].to_lowercase(), stems[4].to_lowercase(), "también en sistemas sin mayúsculas");
    }

    #[test]
    fn remote_paths() {
        assert_eq!(remote_join("nas:", &["Grand Blue", "Cap 1.cbz"]), "nas:Grand Blue/Cap 1.cbz");
        assert_eq!(remote_join("nas:manga/", &["Grand Blue"]), "nas:manga/Grand Blue");
    }

    #[test]
    fn chapters_go_from_first_to_last() {
        let mut all = vec![chapter("Cap 3", "/3", None), chapter("Cap 1", "/1", None), chapter("Cap 2", "/2", None)];
        (all[0].chapter_number, all[1].chapter_number, all[2].chapter_number) = (3.0, 1.0, 2.0);
        let job = Job::new("1", "/m", "Manga: uno", &all, &[0, 1], Dest::Local("d".into()), Format::Cbz);
        assert_eq!(job.chapters.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["Cap 1", "Cap 3"]);
        assert_eq!(job.manga_title, "Manga uno");
        assert_eq!(job.place("Cap 1"), Path::new("d").join("Manga uno").join("Cap 1.cbz").to_string_lossy());

        let remote = Job::new("1", "/m", "Manga", &all, &[0], Dest::Rclone("nas:x".into()), Format::Folder);
        assert_eq!(remote.place("Cap 3"), "rclone:nas:x/Manga/Cap 3");
    }

    #[test]
    fn manifest_knows_what_is_in_each_place() {
        let dir = std::env::temp_dir().join(format!("hantho-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.cbz");
        std::fs::write(&file, b"x").unwrap();
        let local = file.to_string_lossy().into_owned();

        let mut m = Manifest::default();
        m.insert("k", "/1", local.clone());
        assert!(m.contains("k", "/1") && !m.contains("k", "/2") && !m.contains("otro", "/1"));
        assert!(m.has("k", "/1", &local));
        assert!(!m.has("k", "/1", "otra/carpeta/c.cbz"), "en otro destino hay que descargarlo");

        std::fs::remove_file(&file).unwrap();
        assert!(!m.has("k", "/1", &local), "si lo has borrado, se vuelve a descargar");

        m.insert("k", "/2", "rclone:nas:m/c.cbz".into());
        assert!(m.has("k", "/2", "rclone:nas:m/c.cbz"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn cbz_has_the_pages_in_order() {
        let dir = std::env::temp_dir().join(format!("hantho-cbz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pages: Vec<PathBuf> = ["0001.png", "0002.jpg"].map(|n| dir.join(n)).into();
        std::fs::write(&pages[0], b"uno").unwrap();
        std::fs::write(&pages[1], b"dos").unwrap();
        let cbz = dir.join("c.cbz");
        write_cbz(&pages, &cbz).unwrap();

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&cbz).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        assert_eq!(archive.by_index(0).unwrap().name().unwrap(), "0001.png");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut archive.by_index(1).unwrap(), &mut text).unwrap();
        assert_eq!(text, "dos");
        drop(archive);
        for f in pages.iter().chain([&cbz]) {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_dir(&dir);
    }
}
