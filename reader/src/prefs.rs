//! Preferencias que se recuerdan entre sesiones (`~/.hantho/prefs.json`): el ajuste del lector y
//! los idiomas en los que buscar. Las comparten la TUI y el servidor MCP.

use crate::download;
use crate::paths;
use crate::render::Fit;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct Prefs {
    /// Último ajuste elegido en el lector, y el de cada manga.
    #[serde(default)]
    pub fit: Fit,
    #[serde(default)]
    pub manga_fit: HashMap<String, Fit>,
    /// Idiomas de las fuentes en las que buscar (vacío = todos).
    #[serde(default = "default_langs")]
    pub search_langs: Vec<String>,
    /// Dónde guardar las descargas: una carpeta o `rclone:<remoto>:<ruta>`.
    #[serde(default)]
    pub download_dest: Option<String>,
    #[serde(default)]
    pub download_format: download::Format,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { fit: Fit::default(), manga_fit: HashMap::new(), search_langs: default_langs(), download_dest: None, download_format: download::Format::default() }
    }
}

/// Por defecto se busca en el idioma del sistema y en inglés: buscar en todos supone cientos de
/// fuentes (cada extensión multiidioma cuenta una vez por idioma) y la mayoría no sirven.
fn default_langs() -> Vec<String> {
    let system = tr!("es", "en");
    if system == "en" {
        vec!["en".into()]
    } else {
        vec![system.into(), "en".into()]
    }
}

impl Prefs {
    pub fn load() -> Self {
        std::fs::read(paths::prefs_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = paths::write_atomic(&paths::prefs_file(), &json);
        }
    }

    pub fn fit_for(&self, manga_key: &str) -> Fit {
        self.manga_fit.get(manga_key).copied().unwrap_or(self.fit)
    }

    pub fn set_fit(&mut self, manga_key: String, fit: Fit) {
        self.fit = fit;
        self.manga_fit.insert(manga_key, fit);
        self.save();
    }

    pub fn download_dest(&self) -> String {
        self.download_dest.clone().unwrap_or_else(download::default_dest)
    }

    /// Texto para mostrar el filtro de idiomas ("es + en", o "todos").
    pub fn langs_label(&self) -> String {
        if self.search_langs.is_empty() {
            tr!("todos los idiomas", "all languages").into()
        } else {
            self.search_langs.join(" + ")
        }
    }
}

/// Si una fuente de idioma `lang` entra en el filtro `langs` (vacío = todas). Las fuentes "all"
/// (multiidioma) entran siempre, y "es" incluye variantes como "es-419".
pub fn lang_matches(langs: &[String], lang: &str) -> bool {
    langs.is_empty() || lang == "all" || langs.iter().any(|l| lang == l || lang.split('-').next() == Some(l.as_str()))
}

/// Opciones que recorre la tecla de idioma: todos, el del sistema, el del sistema + inglés,
/// inglés, y después el resto de idiomas de las fuentes instaladas (de más a menos fuentes).
pub fn lang_options<'a>(source_langs: impl Iterator<Item = &'a str>) -> Vec<Vec<String>> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for lang in source_langs.filter(|l| *l != "all") {
        *counts.entry(lang.split('-').next().unwrap_or(lang).to_string()).or_default() += 1;
    }
    let mut others: Vec<(String, usize)> = counts.into_iter().collect();
    others.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let system = tr!("es", "en").to_string();
    let mut options = vec![vec![], vec![system.clone()]];
    if system != "en" {
        options.push(vec![system.clone(), "en".into()]);
        options.push(vec!["en".into()]);
    }
    for (lang, _) in others {
        let option = vec![lang];
        if !options.contains(&option) {
            options.push(option);
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        let es = vec!["es".to_string()];
        assert!(lang_matches(&es, "es") && lang_matches(&es, "es-419") && lang_matches(&es, "all"));
        assert!(!lang_matches(&es, "en") && !lang_matches(&es, "esp"));
        assert!(lang_matches(&[], "ja"));

        let options = lang_options(["es", "es-419", "en", "fr", "fr", "fr", "all"].into_iter());
        assert_eq!(options[0], Vec::<String>::new());
        assert_eq!(options[1], vec![tr!("es", "en").to_string()]);
        // El resto, sin repetir, con francés (3 fuentes) antes que los demás
        assert!(options.contains(&vec!["fr".to_string()]));
        let flat: Vec<_> = options.iter().filter(|o| o.len() == 1).collect();
        assert_eq!(flat.len(), flat.iter().collect::<std::collections::HashSet<_>>().len());
    }
}
