//! Catálogo de extensiones: los repositorios que añade el usuario (`index.pb`: protobuf
//! comprimido con gzip, el formato de Keiyoushi, que publica cada extensión también como .jar).
//! Han Thỏ no trae ningún repositorio: como en Mihon, cada usuario añade los suyos.
//! Schema oficial en `proto/index.proto`; aquí se replica a mano con prost.

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use prost::Message;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Message)]
pub struct Index {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub badge_label: String,
    #[prost(string, tag = "3")]
    pub signing_key: String,
    // En el .proto es un oneof (101 lista / 102 url); en la práctica siempre viene la lista
    #[prost(message, optional, tag = "101")]
    pub extension_list: Option<ExtensionList>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExtensionList {
    #[prost(message, repeated, tag = "1")]
    pub extensions: Vec<Extension>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Extension {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub package_name: String,
    #[prost(message, optional, tag = "3")]
    pub resources: Option<Resources>,
    #[prost(string, tag = "4")]
    pub extension_lib: String,
    #[prost(int64, tag = "5")]
    pub version_code: i64,
    #[prost(string, tag = "6")]
    pub version_name: String,
    #[prost(int32, tag = "7")]
    pub content_warning: i32,
    #[prost(message, repeated, tag = "8")]
    pub sources: Vec<Source>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resources {
    #[prost(string, tag = "1")]
    pub apk_url: String,
    #[prost(string, tag = "2")]
    pub icon_url: String,
    #[prost(string, tag = "501")]
    pub jar_url: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Source {
    #[prost(int64, tag = "1")]
    pub id: i64,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub language: String,
    #[prost(string, tag = "4")]
    pub home_url: String,
}

impl Extension {
    pub fn jar_url(&self) -> Option<&str> {
        self.resources.as_ref().map(|r| r.jar_url.as_str()).filter(|u| !u.is_empty())
    }

    pub fn langs(&self) -> Vec<&str> {
        let mut langs: Vec<&str> = self.sources.iter().map(|s| s.language.as_str()).collect();
        langs.dedup();
        langs
    }

    pub fn warning_label(&self) -> &'static str {
        match self.content_warning {
            1 => "safe",
            2 => "mixed",
            3 => "nsfw",
            _ => "?",
        }
    }
}

/// Repositorios añadidos por el usuario (URLs de `index.pb`).
pub fn repos() -> Vec<String> {
    std::fs::read(crate::paths::repos_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_repos(repos: &[String]) -> Result<()> {
    crate::paths::write_atomic(&crate::paths::repos_file(), &serde_json::to_vec_pretty(repos)?)?;
    Ok(())
}

/// Acepta lo que el usuario suele tener a mano y lo convierte en la URL del `index.pb`:
/// la URL del índice, la de `index.min.json` (Mihon), la carpeta del repo, un enlace
/// `tachiyomi://add-repo?url=…` o `https://github.com/<dueño>/<repo>` (rama `repo`).
pub fn normalize_repo(input: &str) -> Result<String> {
    let mut url = input.trim().trim_matches(['"', '\'', '<', '>']).to_string();
    if let Some(i) = url.find("url=").filter(|_| url.contains("://add-repo")) {
        url = url[i + 4..].replace("%3A", ":").replace("%2F", "/");
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!(trf!("no parece una URL: {input}", "that doesn't look like a URL: {input}"));
    }
    let url = url.trim_end_matches('/');
    if url.ends_with(".pb") {
        return Ok(url.to_string());
    }
    if let Some(base) = url.strip_suffix("index.min.json").or_else(|| url.strip_suffix("index.json")) {
        return Ok(format!("{base}index.pb"));
    }
    if let Some(path) = url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() == 2 {
            return Ok(format!("https://raw.githubusercontent.com/{}/{}/repo/index.pb", parts[0], parts[1]));
        }
    }
    Ok(format!("{url}/index.pb"))
}

fn cache_path(url: &str) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    crate::paths::repos_dir().join(format!("{:016x}.pb", h.finish()))
}

async fn fetch(url: &str) -> Result<Vec<Extension>> {
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let exts = decode(&bytes).with_context(|| trf!("{url} no es un índice de extensiones válido", "{url} isn't a valid extension index"))?;
    if !exts.iter().any(|e| e.jar_url().is_some()) {
        bail!(trf!(
            "{url} no publica extensiones en .jar (Han Thỏ no puede cargar .apk)",
            "{url} doesn't publish extensions as .jar (Han Thỏ can't load .apk)"
        ));
    }
    crate::paths::write_atomic(&cache_path(url), &bytes)?;
    Ok(exts)
}

/// Añade un repositorio: lo descarga para comprobar que vale. Devuelve la URL y cuántas extensiones trae.
pub async fn add_repo(input: &str) -> Result<(String, usize)> {
    let url = normalize_repo(input)?;
    let n = fetch(&url).await?.len();
    let mut all = repos();
    if !all.contains(&url) {
        all.push(url.clone());
        save_repos(&all)?;
    }
    Ok((url, n))
}

pub fn remove_repo(input: &str) -> Result<bool> {
    let url = normalize_repo(input).unwrap_or_else(|_| input.to_string());
    let mut all = repos();
    let before = all.len();
    all.retain(|r| r != &url && r != input);
    if all.len() == before {
        return Ok(false);
    }
    let _ = std::fs::remove_file(cache_path(&url));
    save_repos(&all)?;
    Ok(true)
}

fn no_repos() -> &'static str {
    tr!(
        "no hay repositorios de extensiones: añade uno en «Añadir extensiones» o con `hantho ext repo add <url>`",
        "no extension repositories yet: add one in “Add extensions” or with `hantho ext repo add <url>`"
    )
}

/// Descarga de nuevo el índice de todos los repositorios.
pub async fn update() -> Result<Vec<Extension>> {
    let repos = repos();
    if repos.is_empty() {
        bail!(no_repos());
    }
    let mut lists = Vec::new();
    for url in &repos {
        lists.push(fetch(url).await?);
    }
    Ok(merge(lists))
}

/// Hace cuánto se descargó el índice más viejo de los repositorios (None si falta alguno).
pub fn age() -> Option<std::time::Duration> {
    let repos = repos();
    let mut oldest = std::time::Duration::ZERO;
    for url in &repos {
        let modified = std::fs::metadata(cache_path(url)).and_then(|m| m.modified()).ok()?;
        oldest = oldest.max(modified.elapsed().unwrap_or_default());
    }
    (!repos.is_empty()).then_some(oldest)
}

/// Las extensiones instaladas que tienen una versión más nueva en el catálogo (la del catálogo).
pub fn outdated(catalog: &[Extension], installed: &[crate::ext::Installed]) -> Vec<Extension> {
    catalog
        .iter()
        .filter(|e| installed.iter().any(|i| i.pkg == e.package_name && i.version_code < e.version_code))
        .cloned()
        .collect()
}

/// Junta los índices locales de todos los repositorios; descarga los que falten.
pub async fn load() -> Result<Vec<Extension>> {
    let repos = repos();
    if repos.is_empty() {
        bail!(no_repos());
    }
    let mut lists = Vec::new();
    for url in &repos {
        let cached = std::fs::read(cache_path(url)).ok().and_then(|b| decode(&b).ok());
        lists.push(match cached {
            Some(exts) => exts,
            None => fetch(url).await?,
        });
    }
    Ok(merge(lists))
}

/// Si dos repositorios traen el mismo paquete, gana la versión más nueva.
fn merge(lists: Vec<Vec<Extension>>) -> Vec<Extension> {
    let mut by_pkg: std::collections::HashMap<String, Extension> = std::collections::HashMap::new();
    for e in lists.into_iter().flatten() {
        match by_pkg.get(&e.package_name) {
            Some(old) if old.version_code >= e.version_code => {}
            _ => {
                by_pkg.insert(e.package_name.clone(), e);
            }
        }
    }
    let mut all: Vec<Extension> = by_pkg.into_values().collect();
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    all
}

fn decode(gz: &[u8]) -> Result<Vec<Extension>> {
    let mut raw = Vec::new();
    GzDecoder::new(gz).read_to_end(&mut raw).context(tr!("index.pb no es gzip válido", "index.pb isn't valid gzip"))?;
    let index = Index::decode(raw.as_slice()).context(tr!("index.pb no es un protobuf válido", "index.pb isn't valid protobuf"))?;
    Ok(index.extension_list.map(|l| l.extensions).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::{normalize_repo, outdated, Extension};
    use crate::ext::Installed;

    #[test]
    fn outdated_extensions() {
        let ext = |pkg: &str, version_code| Extension { package_name: pkg.into(), version_code, ..Default::default() };
        let inst = |pkg: &str, version_code| Installed {
            pkg: pkg.into(),
            name: pkg.into(),
            version_code,
            version_name: String::new(),
            content_warning: 0,
            langs: Vec::new(),
        };
        let catalog = [ext("a", 5), ext("b", 3), ext("c", 9)];
        // a desactualizada, b al día, c sin instalar, d instalada pero ya no está en el catálogo
        let installed = [inst("a", 4), inst("b", 3), inst("d", 1)];
        let found: Vec<_> = outdated(&catalog, &installed).into_iter().map(|e| (e.package_name, e.version_code)).collect();
        assert_eq!(found, vec![("a".to_string(), 5)]);
    }

    #[test]
    fn repo_urls() {
        let pb = "https://raw.githubusercontent.com/dueño/ext/repo/index.pb";
        assert_eq!(normalize_repo(pb).unwrap(), pb);
        assert_eq!(normalize_repo("https://raw.githubusercontent.com/dueño/ext/repo/index.min.json").unwrap(), pb);
        assert_eq!(normalize_repo("https://raw.githubusercontent.com/dueño/ext/repo/").unwrap(), pb);
        assert_eq!(normalize_repo("https://github.com/dueño/ext").unwrap(), pb);
        assert_eq!(
            normalize_repo("tachiyomi://add-repo?url=https%3A%2F%2Fraw.githubusercontent.com%2Fdueño%2Fext%2Frepo%2Findex.min.json").unwrap(),
            pb
        );
        assert!(normalize_repo("no es una url").is_err());
    }
}
