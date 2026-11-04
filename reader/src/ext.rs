//! Extensiones instaladas: JARs en `~/.hantho/extensions` + `installed.json` con sus metadatos.

use crate::catalog::Extension;
use crate::paths;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub pkg: String,
    pub name: String,
    pub version_code: i64,
    pub version_name: String,
    pub content_warning: i32,
    pub langs: Vec<String>,
}

impl Installed {
    pub fn jar_path(&self) -> PathBuf {
        paths::extensions_dir().join(format!("{}.jar", self.pkg))
    }
}

pub fn list() -> Vec<Installed> {
    std::fs::read(paths::installed_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(list: &[Installed]) -> Result<()> {
    paths::write_atomic(&paths::installed_file(), &serde_json::to_vec_pretty(list)?)?;
    Ok(())
}

pub async fn install(ext: &Extension) -> Result<Installed> {
    let Some(url) = ext.jar_url() else { bail!(trf!("{} no publica un .jar", "{} doesn't publish a .jar", ext.name)) };
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let item = Installed {
        pkg: ext.package_name.clone(),
        name: ext.name.clone(),
        version_code: ext.version_code,
        version_name: ext.version_name.clone(),
        content_warning: ext.content_warning,
        langs: ext.langs().into_iter().map(String::from).collect(),
    };
    paths::write_atomic(&item.jar_path(), &bytes)?;

    let mut all = list();
    all.retain(|i| i.pkg != item.pkg);
    all.push(item.clone());
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    save(&all)?;
    // Una versión nueva puede arreglar lo que fallaba: darle otra oportunidad a sus fuentes
    crate::health::Health::update(|h| ext.sources.iter().for_each(|s| h.forgive(&s.id.to_string())));
    Ok(item)
}

/// Instala la versión del catálogo de cada extensión (de cuatro en cuatro).
pub async fn upgrade_all(exts: &[Extension]) -> Vec<(Extension, Result<Installed>)> {
    use futures::stream::{self, StreamExt};
    stream::iter(exts.iter().cloned())
        .map(|e| async move {
            let res = install(&e).await;
            (e, res)
        })
        .buffer_unordered(4)
        .collect()
        .await
}

pub fn uninstall(pkg: &str) -> Result<bool> {
    let mut all = list();
    let Some(pos) = all.iter().position(|i| i.pkg == pkg) else { return Ok(false) };
    let item = all.remove(pos);
    let _ = std::fs::remove_file(item.jar_path());
    save(&all)?;
    Ok(true)
}

/// Busca en el catálogo por nombre, paquete o id de fuente (sin distinguir mayúsculas).
pub fn find<'a>(catalog: &'a [Extension], query: &str) -> Vec<&'a Extension> {
    let q = query.to_lowercase();
    if let Some(exact) = catalog.iter().find(|e| e.package_name == query) {
        return vec![exact];
    }
    catalog
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&q) || e.package_name.to_lowercase().ends_with(&format!(".{q}")))
        .collect()
}
