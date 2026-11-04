//! Capítulos nuevos de los favoritos que estás leyendo. Solo se comprueba cuando se pide
//! (`u` en la biblioteca, `hantho updates`), para no hacer peticiones a las fuentes porque sí.
//! El resultado se guarda en `~/.hantho/new_chapters.json`.

use crate::health::Health;
use crate::host::Host;
use crate::library::{Favorite, Status};
use crate::model::Chapter;
use crate::paths;
use crate::reads::{self, ReadLog};
use crate::search;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Favoritos comprobados a la vez.
const CONCURRENCY: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub url: String,
    pub number: f32,
}

/// "fuente|url del manga" → capítulos por leer en la última comprobación.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct News {
    mangas: HashMap<String, Vec<Pending>>,
}

impl News {
    pub fn load() -> News {
        std::fs::read(paths::news_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = paths::write_atomic(&paths::news_file(), &bytes);
        }
    }

    /// Cuántos siguen pendientes: lo que has leído desde la comprobación ya no cuenta.
    pub fn count(&self, fav: &Favorite, log: &ReadLog) -> usize {
        let key = reads::key(&fav.source_id, &fav.manga.url);
        let read = fav.progress.max(log.max_number(&key)) as f32;
        self.mangas.get(&key).map_or(0, |list| list.iter().filter(|p| p.number > read && !log.is_read(&key, &p.url)).count())
    }

    pub fn set(&mut self, key: String, pending: Vec<Pending>) {
        self.mangas.insert(key, pending);
    }
}

/// Capítulos posteriores al último leído, sin contar dos veces el mismo de varios scanlators.
pub fn pending(chapters: &[Chapter], read_up_to: f32, is_read: impl Fn(&str) -> bool) -> Vec<Pending> {
    let mut seen = HashSet::new();
    chapters
        .iter()
        .filter(|c| c.chapter_number > read_up_to && !is_read(&c.url))
        .filter(|c| seen.insert((c.chapter_number * 100.0) as i64))
        .map(|c| Pending { url: c.url.clone(), number: c.chapter_number })
        .collect()
}

#[derive(Debug)]
pub enum Event {
    Progress { done: usize, total: usize },
    /// Un favorito con capítulos por leer.
    Found { title: String, new: usize },
    Failed { title: String, error: String },
    Done { checked: usize, failed: usize, skipped: usize },
}

/// Los que se están leyendo, con la fuente cargada y sin omitir por sus fallos.
fn to_check(favorites: Vec<Favorite>, loaded: &HashSet<String>) -> (Vec<Favorite>, usize) {
    let health = Health::load();
    let now = crate::library::now();
    let reading: Vec<Favorite> = favorites.into_iter().filter(|f| matches!(f.status, Status::Current | Status::Repeating)).collect();
    let total = reading.len();
    let usable: Vec<Favorite> = reading
        .into_iter()
        .filter(|f| loaded.contains(&f.source_id) && !health.get(&f.source_id).is_some_and(|r| r.disabled || r.skip_until().is_some_and(|until| now < until)))
        .collect();
    let skipped = total - usable.len();
    (usable, skipped)
}

pub fn check(host: Arc<Host>, favorites: Vec<Favorite>, loaded: HashSet<String>) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (favorites, skipped) = to_check(favorites, &loaded);
        let total = favorites.len();
        let log = ReadLog::load();
        let mut news = News::load();
        let (mut checked, mut failed) = (0, 0);
        let mut requests = stream::iter(favorites)
            .map(|fav| {
                let host = host.clone();
                async move {
                    let res = host.details(&fav.source_id, &fav.manga, true).await;
                    (fav, res)
                }
            })
            .buffer_unordered(CONCURRENCY);
        while let Some((fav, res)) = requests.next().await {
            match res {
                Ok(details) => {
                    let key = reads::key(&fav.source_id, &fav.manga.url);
                    let read = fav.progress.max(log.max_number(&key)) as f32;
                    let list = pending(&details.chapters, read, |url| log.is_read(&key, url));
                    if !list.is_empty() {
                        let _ = tx.send(Event::Found { title: fav.manga.title.clone(), new: list.len() });
                    }
                    news.set(key, list);
                    news.save();
                    checked += 1;
                }
                Err(e) => {
                    failed += 1;
                    let _ = tx.send(Event::Failed { title: fav.manga.title.clone(), error: search::short_error(&e.to_string()) });
                }
            }
            let _ = tx.send(Event::Progress { done: checked + failed, total });
        }
        let _ = tx.send(Event::Done { checked, failed, skipped });
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Manga;

    fn chapter(url: &str, number: f32) -> Chapter {
        Chapter { url: url.into(), name: url.into(), chapter_number: number, ..Default::default() }
    }

    fn favorite(progress: i32) -> Favorite {
        Favorite {
            source_id: "1".into(),
            source_name: "s".into(),
            lang: "es".into(),
            manga: Manga { url: "/m".into(), title: "M".into(), ..Default::default() },
            added_at: 0,
            status: Status::Current,
            progress,
            last_read: None,
            anilist: None,
        }
    }

    #[test]
    fn only_counts_chapters_after_the_last_read_once() {
        let chapters = [chapter("/12-a", 12.0), chapter("/12-b", 12.0), chapter("/11", 11.0), chapter("/10", 10.0), chapter("/10.5", 10.5), chapter("/raro", -1.0)];
        let list = pending(&chapters, 10.0, |url| url == "/11");
        let urls: Vec<_> = list.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["/12-a", "/10.5"]);
    }

    #[test]
    fn reading_lowers_the_count_without_checking_again() {
        let mut news = News::default();
        news.set(reads::key("1", "/m"), vec![Pending { url: "/11".into(), number: 11.0 }, Pending { url: "/12".into(), number: 12.0 }]);
        let log = ReadLog::default();
        assert_eq!(news.count(&favorite(10), &log), 2);
        assert_eq!(news.count(&favorite(11), &log), 1);
        assert_eq!(news.count(&favorite(12), &log), 0);

        let mut log = ReadLog::default();
        log.mark(&reads::key("1", "/m"), &chapter("/11", 11.0));
        assert_eq!(news.count(&favorite(0), &log), 1, "lo leído también cuenta si no hay progreso");
    }

    #[test]
    fn only_checks_what_you_are_reading_from_usable_sources() {
        let mut planned = favorite(0);
        planned.status = Status::Planning;
        let mut other_source = favorite(0);
        other_source.source_id = "no-instalada-news".into();
        let (list, skipped) = to_check(vec![favorite(0), planned, other_source], &HashSet::from(["1".to_string()]));
        assert_eq!((list.len(), skipped), (1, 1), "lo planeado no se comprueba ni cuenta");
    }
}
