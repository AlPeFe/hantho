//! Servidor MCP (Model Context Protocol) por stdio: `hantho mcp`.
//!
//! Permite que un agente (Claude Code, Codex, Hermes, pi…) busque manga, consulte la biblioteca,
//! marque capítulos y abra el lector en un panel al lado del chat. JSON-RPC 2.0, un mensaje por
//! línea; stdout es solo protocolo (los mensajes de diagnóstico van a stderr).
//!
//! La biblioteca y el registro de leídos se releen del disco en cada llamada y se guardan al
//! momento, así que la TUI y el MCP pueden usarse a la vez.

use crate::anilist::{self, Credentials};
use crate::host::{Host, LoadedSource};
use crate::launch::{self, Placement};
use crate::library::Library;
use crate::model::Manga;
use crate::reads::{self, ReadLog};
use crate::search::{self, SearchEvent};
use crate::tui::ChapterTarget;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, OnceCell};

const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

fn instructions() -> &'static str {
    tr!(
        "Han Thỏ es un lector de manga para la terminal con extensiones de Mihon. \
Usa search_manga para encontrar un título en las fuentes instaladas, library para ver los favoritos del usuario \
con su progreso, chapters para listar capítulos, mark_read para marcar lo leído (actualiza AniList si el manga está \
vinculado) y open_reader para abrir el lector en un panel al lado del chat. Los identificadores de fuente (source_id) \
y las URLs (manga_url, chapter_url) salen de search_manga, library o chapters: no los inventes. \
El usuario usa la aplicación en español.",
        "Han Thỏ is a terminal manga reader with Mihon extensions. \
Use search_manga to find a title across the installed sources, library to see the user's favorites \
with their progress, chapters to list chapters, mark_read to mark what was read (updates AniList if the manga is \
linked) and open_reader to open the reader in a pane next to the chat. Source ids (source_id) \
and URLs (manga_url, chapter_url) come from search_manga, library or chapters: don't make them up."
    )
}

struct Server {
    host: OnceCell<(Arc<Host>, Vec<LoadedSource>)>,
    out: Mutex<tokio::io::Stdout>,
}

pub async fn serve() -> Result<()> {
    let server = Arc::new(Server { host: OnceCell::new(), out: Mutex::new(tokio::io::stdout()) });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let server = server.clone();
        // Cada pedido en su tarea: una búsqueda lenta no bloquea los demás
        tokio::spawn(async move {
            if let Some(response) = server.handle(&line).await {
                let mut out = server.out.lock().await;
                let _ = out.write_all(format!("{response}\n").as_bytes()).await;
                let _ = out.flush().await;
            }
        });
    }
    Ok(())
}

impl Server {
    async fn handle(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error(Value::Null, -32700, &trf!("JSON inválido: {e}", "invalid JSON: {e}"))),
        };
        // Notificaciones (sin id): no llevan respuesta
        let id = msg.get("id").cloned()?;
        let method = msg["method"].as_str().unwrap_or("");
        let params = &msg["params"];
        let result = match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(match self.call(name, &args).await {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true }),
                })
            }
            _ => Err((-32601, trf!("método desconocido: {method}", "unknown method: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, msg)) => error(id, code, &msg),
        })
    }

    /// El host de extensiones se arranca la primera vez que hace falta.
    async fn host(&self) -> Result<&(Arc<Host>, Vec<LoadedSource>)> {
        self.host
            .get_or_try_init(|| async {
                let host = Host::spawn().await?;
                let (sources, _) = host.load_all(&crate::ext::list()).await;
                if sources.is_empty() {
                    bail!(tr!(
                        "no hay extensiones instaladas: el usuario tiene que añadir un repositorio y elegirlas en «Añadir extensiones» (o `hantho ext repo add <url>`)",
                        "no extensions installed: the user has to add a repository and pick them in “Add extensions” (or `hantho ext repo add <url>`)"
                    ));
                }
                Ok((host, sources))
            })
            .await
    }

    async fn call(&self, name: &str, args: &Value) -> Result<String> {
        match name {
            "search_manga" => self.search(args).await,
            "library" => library(),
            "chapters" => self.chapters(args).await,
            "mark_read" => self.mark_read(args).await,
            "set_favorite" => set_favorite(args).await,
            "link_anilist" => link_anilist(args).await,
            "open_reader" => open_reader(args),
            "sync_anilist" => sync_anilist(args).await,
            _ => bail!(trf!("herramienta desconocida: {name}", "unknown tool: {name}")),
        }
    }

    async fn search(&self, args: &Value) -> Result<String> {
        let query = str_arg(args, "query")?;
        let lang = args["lang"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(15) as usize;
        let (host, sources) = self.host().await?;
        // Sin idioma explícito, los mismos que la búsqueda de la TUI
        let langs = match lang {
            Some(l) => vec![l.to_string()],
            None => crate::prefs::Prefs::load().search_langs,
        };
        let sources: Vec<_> = sources.iter().filter(|s| crate::prefs::lang_matches(&langs, &s.info.lang)).cloned().collect();
        let health = crate::health::Health::load();
        let (mut sources, skipped) = health.partition(sources, crate::library::now());
        // De más a menos fiables; sin `all_sources`, solo la primera tanda
        let mut remaining = 0;
        if !args["all_sources"].as_bool().unwrap_or(false) {
            let library = crate::library::Library::load();
            let favorites = library.favorites.iter().map(|f| f.source_id.as_str()).collect();
            let (first, rest) = health.first_batch(sources, &favorites);
            sources = first;
            remaining = rest.len();
        }

        let mut groups: Vec<(String, String, Vec<Value>)> = Vec::new(); // (clave, título, fuentes)
        let mut failed = 0;
        let mut rx = search::global(host.clone(), sources, query.to_string());
        while let Some(ev) = rx.recv().await {
            match ev {
                SearchEvent::Results { source, mangas } => {
                    for m in mangas {
                        let key = search::normalize(&m.title);
                        let entry = json!({ "source_id": source.info.id, "source": source.info.name, "lang": source.info.lang, "manga_url": m.url, "title": m.title });
                        match groups.iter_mut().find(|g| g.0 == key) {
                            Some(g) => g.2.push(entry),
                            None => groups.push((key, m.title.clone(), vec![entry])),
                        }
                    }
                }
                SearchEvent::Failed { .. } => failed += 1,
                SearchEvent::Done => break,
            }
        }
        // Igual que en la TUI: coincidencia exacta, luego por prefijo/contenido, luego por nº de fuentes
        let q = search::normalize(query);
        groups.sort_by_key(|(k, _, s)| (if *k == q { 0 } else if k.starts_with(&q) { 1 } else if k.contains(&q) { 2 } else { 3 }, std::cmp::Reverse(s.len())));
        let results: Vec<Value> = groups.into_iter().take(limit).map(|(_, title, sources)| json!({ "title": title, "sources": sources })).collect();
        Ok(pretty(&json!({ "query": query, "results": results, "failed_sources": failed, "skipped_sources": skipped.len(), "remaining_sources": remaining })))
    }

    async fn chapters(&self, args: &Value) -> Result<String> {
        let (source_id, manga_url) = (str_arg(args, "source_id")?, str_arg(args, "manga_url")?);
        let (host, sources) = self.host().await?;
        if !sources.iter().any(|s| s.info.id == source_id) {
            bail!(trf!("la fuente {source_id} no está instalada", "source {source_id} isn't installed"));
        }
        let manga = Manga { url: manga_url.to_string(), ..Default::default() };
        let details = host.details(source_id, &manga, args["refresh"].as_bool().unwrap_or(false)).await?;
        let log = ReadLog::load();
        let key = reads::key(source_id, manga_url);
        let unread_only = args["unread_only"].as_bool().unwrap_or(false);
        let (from, to) = (args["from"].as_f64(), args["to"].as_f64());
        let limit = args["limit"].as_u64().unwrap_or(60) as usize;

        let mut chapters: Vec<_> = details.chapters.iter().collect();
        chapters.sort_by(|a, b| a.chapter_number.total_cmp(&b.chapter_number));
        let read_count = chapters.iter().filter(|c| log.is_read(&key, &c.url)).count();
        let list: Vec<Value> = chapters
            .iter()
            .filter(|c| !unread_only || !log.is_read(&key, &c.url))
            .filter(|c| from.is_none_or(|f| c.chapter_number as f64 >= f) && to.is_none_or(|t| c.chapter_number as f64 <= t))
            .take(limit)
            .map(|c| json!({ "number": c.chapter_number, "name": c.name, "chapter_url": c.url, "read": log.is_read(&key, &c.url) }))
            .collect();
        Ok(pretty(&json!({
            "title": details.manga.title,
            "total": details.chapters.len(),
            "read": read_count,
            "shown": list.len(),
            "chapters": list,
            "note": tr!("ordenados del más viejo al más nuevo; usa from/to/unread_only/limit para ver otros", "sorted oldest to newest; use from/to/unread_only/limit to see others")
        })))
    }

    async fn mark_read(&self, args: &Value) -> Result<String> {
        let (source_id, manga_url) = (str_arg(args, "source_id")?, str_arg(args, "manga_url")?);
        let read = args["read"].as_bool().unwrap_or(true);
        let key = reads::key(source_id, manga_url);
        let mut log = ReadLog::load();
        let mut changed = 0;

        if let Some(up_to) = args["up_to_number"].as_f64() {
            // Hace falta la lista de capítulos para saber cuáles son "hasta el N"
            let (host, _) = self.host().await?;
            let details = host.details(source_id, &Manga { url: manga_url.to_string(), ..Default::default() }, false).await?;
            for c in details.chapters.iter().filter(|c| c.chapter_number >= 0.0 && c.chapter_number as f64 <= up_to) {
                changed += usize::from(if read { log.mark(&key, c) } else { log.unmark(&key, &c.url) });
            }
        } else if let Some(url) = args["chapter_url"].as_str() {
            let chapter = crate::model::Chapter {
                url: url.to_string(),
                name: args["chapter_name"].as_str().unwrap_or(url).to_string(),
                chapter_number: args["chapter_number"].as_f64().unwrap_or(-1.0) as f32,
                ..Default::default()
            };
            changed += usize::from(if read { log.mark(&key, &chapter) } else { log.unmark(&key, url) });
        } else {
            bail!(tr!("indica chapter_url o up_to_number", "pass chapter_url or up_to_number"));
        }
        log.save()?;

        // Favorito: progreso = mayor capítulo leído, y AniList si está vinculado
        let mut lib = Library::load();
        let mut anilist = String::from(tr!("no vinculado a AniList", "not linked to AniList"));
        if let Some(fav) = lib.get_mut(source_id, manga_url) {
            fav.progress = log.max_number(&key);
            let (progress, status, link) = (fav.progress, fav.status, fav.anilist.clone());
            lib.save()?;
            if let (Some(link), Some(creds)) = (link, Credentials::load()) {
                anilist = match anilist::sync(&creds, link.media_id, status.as_anilist(), progress).await {
                    Ok(r) => {
                        let mut lib = Library::load();
                        if let Some(fav) = lib.get_mut(source_id, manga_url) {
                            fav.progress = fav.progress.max(r.entry.progress.unwrap_or(0));
                            if let Some(a) = fav.anilist.as_mut() {
                                a.synced_progress = Some(fav.progress);
                                a.synced_status = Some(status);
                                a.synced_at = Some(crate::library::now());
                            }
                            lib.save()?;
                        }
                        trf!("AniList actualizado: capítulo {}", "AniList updated: chapter {}", r.entry.progress.unwrap_or(progress))
                    }
                    Err(e) => trf!("AniList no se ha podido actualizar: {e}", "AniList couldn't be updated: {e}"),
                };
            }
        }
        Ok(if read {
            trf!("{changed} capítulos marcados como leídos. {anilist}.", "{changed} chapters marked as read. {anilist}.")
        } else {
            trf!("{changed} capítulos desmarcados. {anilist}.", "{changed} chapters unmarked. {anilist}.")
        })
    }
}

fn library() -> Result<String> {
    let lib = Library::load();
    let log = ReadLog::load();
    let items: Vec<Value> = lib
        .favorites
        .iter()
        .map(|f| {
            json!({
                "title": f.manga.title,
                "source_id": f.source_id,
                "source": f.source_name,
                "manga_url": f.manga.url,
                "status": f.status.label(),
                "progress": f.progress,
                "chapters_read": log.chapters(&reads::key(&f.source_id, &f.manga.url)).map_or(0, |c| c.len()),
                "last_read": f.last_read.as_ref().map(|l| json!({ "chapter": l.chapter_name, "chapter_url": l.chapter_url, "page": l.page + 1 })),
                "anilist": f.anilist.as_ref().map(|a| json!({ "media_id": a.media_id, "title": a.title, "pending_sync": f.needs_sync() })),
            })
        })
        .collect();
    Ok(pretty(&json!({ "favorites": items, "anilist_user": Credentials::load().map(|c| c.user_name) })))
}

async fn set_favorite(args: &Value) -> Result<String> {
    let (source_id, manga_url) = (str_arg(args, "source_id")?, str_arg(args, "manga_url")?);
    let favorite = args["favorite"].as_bool().unwrap_or(true);
    let mut lib = Library::load();
    let exists = lib.find(source_id, manga_url).is_some();
    if exists == favorite {
        return Ok(if favorite { tr!("ya estaba en favoritos", "it was already in favorites").into() } else { tr!("no estaba en favoritos", "it wasn't in favorites").into() });
    }
    let manga = Manga { url: manga_url.to_string(), title: args["title"].as_str().unwrap_or(manga_url).to_string(), ..Default::default() };
    let source_name = args["source"].as_str().unwrap_or(source_id);
    let lang = args["lang"].as_str().unwrap_or("");
    lib.toggle(source_id, source_name, lang, &manga);
    lib.save()?;
    if !favorite {
        return Ok(tr!("quitado de favoritos", "removed from favorites").into());
    }
    // Vincular con AniList si hay una coincidencia exacta del título
    let linked = match anilist::search(&manga.title).await {
        Ok(candidates) => match anilist::best_match(&manga.title, &candidates) {
            Some(media) => link_and_sync(source_id, manga_url, media.id, &media.display_title()).await?,
            None => tr!("no se ha vinculado con AniList automáticamente (sin coincidencia exacta; usa link_anilist)", "not linked to AniList automatically (no exact match; use link_anilist)").into(),
        },
        Err(e) => trf!("no se ha podido consultar AniList: {e}", "couldn't query AniList: {e}"),
    };
    Ok(trf!("★ «{}» añadido a favoritos. {linked}", "★ “{}” added to favorites. {linked}", manga.title))
}

/// Vincula un favorito con una obra de AniList y, si hay sesión, envía el progreso.
async fn link_and_sync(source_id: &str, manga_url: &str, media_id: i64, title: &str) -> Result<String> {
    let mut lib = Library::load();
    let fav = lib.get_mut(source_id, manga_url).ok_or_else(|| anyhow!(tr!("ese manga no está en favoritos", "that manga isn't in favorites")))?;
    fav.anilist = Some(crate::library::AniListLink { media_id, title: title.to_string(), synced_progress: None, synced_status: None, synced_at: None });
    let (status, progress) = (fav.status, fav.progress);
    lib.save()?;
    let Some(creds) = Credentials::load() else {
        return Ok(trf!("vinculado con «{title}» en AniList (sin sesión: se sincronizará al conectar la cuenta)", "linked to “{title}” on AniList (not logged in: it will sync once the account is connected)"));
    };
    match anilist::sync(&creds, media_id, status.as_anilist(), progress).await {
        Ok(r) => {
            let remote = r.entry.progress.unwrap_or(progress);
            let mut lib = Library::load();
            if let Some(fav) = lib.get_mut(source_id, manga_url) {
                fav.progress = fav.progress.max(remote);
                if let Some(a) = fav.anilist.as_mut() {
                    a.synced_progress = Some(fav.progress);
                    a.synced_status = Some(status);
                    a.synced_at = Some(crate::library::now());
                }
                lib.save()?;
            }
            Ok(trf!("vinculado con «{title}» en AniList y sincronizado (capítulo {remote})", "linked to “{title}” on AniList and synced (chapter {remote})"))
        }
        Err(e) => Ok(trf!("vinculado con «{title}» en AniList, pero no se ha podido sincronizar: {e}", "linked to “{title}” on AniList, but syncing failed: {e}")),
    }
}

async fn link_anilist(args: &Value) -> Result<String> {
    let (source_id, manga_url) = (str_arg(args, "source_id")?, str_arg(args, "manga_url")?);
    let lib = Library::load();
    let fav = lib
        .favorites
        .iter()
        .find(|f| f.is(source_id, manga_url))
        .ok_or_else(|| anyhow!(tr!("ese manga no está en favoritos: añádelo primero con set_favorite", "that manga isn't in favorites: add it first with set_favorite")))?;
    let query = args["query"].as_str().unwrap_or(&fav.manga.title).to_string();
    let candidates = anilist::search(&query).await?;

    if let Some(id) = args["media_id"].as_i64() {
        let title = match candidates.iter().find(|m| m.id == id) {
            Some(m) => m.display_title(),
            None => anilist::media(id).await.map_err(|e| anyhow!(trf!("AniList no tiene ningún manga con id {id}: {e}", "AniList has no manga with id {id}: {e}")))?.display_title(),
        };
        return link_and_sync(source_id, manga_url, id, &title).await;
    }
    if let Some(media) = anilist::best_match(&fav.manga.title, &candidates) {
        return link_and_sync(source_id, manga_url, media.id, &media.display_title()).await;
    }
    // Sin coincidencia segura: devolver candidatas para que el usuario elija
    let list: Vec<Value> = candidates
        .iter()
        .take(8)
        .map(|m| {
            json!({
                "media_id": m.id,
                "title": m.display_title(),
                "english": m.title.english,
                "format": m.format,
                "year": m.start_date.as_ref().and_then(|d| d.year),
                "chapters": m.chapters,
            })
        })
        .collect();
    if list.is_empty() {
        bail!(trf!("AniList no ha encontrado «{query}»; prueba con otro título en query", "AniList didn't find “{query}”; try another title in query"));
    }
    Ok(trf!(
        "No hay una coincidencia exacta para «{}». Pregunta al usuario cuál es y vuelve a llamar a link_anilist con media_id:\n{}",
        "There's no exact match for “{}”. Ask the user which one it is and call link_anilist again with media_id:\n{}",
        fav.manga.title,
        pretty(&json!(list))
    ))
}

fn open_reader(args: &Value) -> Result<String> {
    let chapter = if let Some(url) = args["chapter_url"].as_str() {
        ChapterTarget::Url(url.to_string())
    } else if let Some(n) = args["chapter_number"].as_f64() {
        ChapterTarget::Number(n as f32)
    } else if args["continue"].as_bool().unwrap_or(true) {
        ChapterTarget::Continue
    } else {
        ChapterTarget::None
    };
    let lib = Library::load();
    let target = launch::resolve(&lib, args["title"].as_str(), args["source_id"].as_str(), args["manga_url"].as_str(), args["title"].as_str(), chapter)?;
    let placement = if args["placement"].as_str() == Some("window") { Placement::Window } else { Placement::Side };
    let where_ = launch::launch(&target, placement)?;
    Ok(trf!("Abierto «{}» en {where_}.", "Opened “{}” in {where_}.", target.title))
}

async fn sync_anilist(args: &Value) -> Result<String> {
    let (source_id, manga_url) = (str_arg(args, "source_id")?, str_arg(args, "manga_url")?);
    let creds = Credentials::load().ok_or_else(|| anyhow!(tr!("no hay sesión de AniList: conéctala desde la TUI (menú AniList, tecla l)", "not logged in to AniList: connect it from the TUI (AniList menu, key l)")))?;
    let lib = Library::load();
    let fav = lib.favorites.iter().find(|f| f.is(source_id, manga_url)).ok_or_else(|| anyhow!(tr!("ese manga no está en favoritos", "that manga isn't in favorites")))?;
    let link = fav.anilist.clone().ok_or_else(|| anyhow!(tr!("ese favorito no está vinculado a AniList: vincúlalo con link_anilist", "that favorite isn't linked to AniList: link it with link_anilist")))?;
    let r = anilist::sync(&creds, link.media_id, fav.status.as_anilist(), fav.progress).await?;
    Ok(trf!("AniList: {} · capítulo {}", "AniList: {} · chapter {}", r.entry.status.unwrap_or_default(), r.entry.progress.unwrap_or(0)))
}

fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSIONS[0]);
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == requested).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "hantho", "title": "Han Thỏ", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions(),
    })
}

fn tools() -> Value {
    let manga_ref = json!({
        "source_id": { "type": "string", "description": tr!("Id de la fuente (de search_manga, library o chapters)", "Source id (from search_manga, library or chapters)") },
        "manga_url": { "type": "string", "description": tr!("URL del manga en la fuente (de search_manga o library)", "Manga URL in the source (from search_manga or library)") },
    });
    let with = |extra: Value| {
        let mut props = manga_ref.as_object().unwrap().clone();
        props.extend(extra.as_object().unwrap().clone());
        Value::Object(props)
    };
    json!([
        {
            "name": "search_manga",
            "title": tr!("Buscar manga", "Search manga"),
            "description": tr!("Busca un manga por título en las fuentes instaladas. Con muchas, solo en una primera tanda de las más fiables: si no aparece y remaining_sources es mayor que 0, repite con all_sources. Devuelve los títulos agrupados con las fuentes donde aparecen.", "Search a manga by title across the installed sources. With many, only in a first batch of the most reliable: if it doesn't show up and remaining_sources is above 0, repeat with all_sources. Returns titles grouped with the sources they appear in."),
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": tr!("Título a buscar", "Title to search for") },
                "lang": { "type": "string", "description": tr!("Solo fuentes de este idioma (p. ej. es, en); si no se indica, los idiomas elegidos en la TUI", "Only sources in this language (e.g. en, es); if omitted, the languages chosen in the TUI") },
                "limit": { "type": "integer", "description": tr!("Máximo de títulos (por defecto 15)", "Maximum number of titles (default 15)") },
                "all_sources": { "type": "boolean", "description": tr!("Buscar en todas las fuentes, no solo en la primera tanda (hace muchas más peticiones)", "Search every source, not just the first batch (makes many more requests)") }
            }, "required": ["query"] },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        },
        {
            "name": "library",
            "title": tr!("Biblioteca", "Library"),
            "description": tr!("Lista los favoritos del usuario con estado, progreso, último capítulo leído y vínculo con AniList.", "Lists the user's favorites with status, progress, last chapter read and AniList link."),
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "chapters",
            "title": tr!("Capítulos", "Chapters"),
            "description": tr!("Lista los capítulos de un manga (del más viejo al más nuevo) indicando cuáles están leídos. Para mangas largos usa from/to o unread_only.", "Lists a manga's chapters (oldest to newest) showing which are read. For long manga use from/to or unread_only."),
            "inputSchema": { "type": "object", "properties": with(json!({
                "from": { "type": "number", "description": tr!("Desde el capítulo número", "From chapter number") },
                "to": { "type": "number", "description": tr!("Hasta el capítulo número", "Up to chapter number") },
                "unread_only": { "type": "boolean", "description": tr!("Solo los no leídos", "Only unread ones") },
                "limit": { "type": "integer", "description": tr!("Máximo de capítulos (por defecto 60)", "Maximum number of chapters (default 60)") },
                "refresh": { "type": "boolean", "description": tr!("Ignorar la caché y pedir la lista a la fuente", "Skip the cache and ask the source for the list") }
            })), "required": ["source_id", "manga_url"] },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        },
        {
            "name": "mark_read",
            "title": tr!("Marcar como leído", "Mark as read"),
            "description": tr!("Marca (o desmarca) un capítulo, o todos hasta un número, como leídos. Si el manga es favorito y está vinculado a AniList, actualiza AniList.", "Marks (or unmarks) a chapter, or every chapter up to a number, as read. If the manga is a favorite linked to AniList, updates AniList."),
            "inputSchema": { "type": "object", "properties": with(json!({
                "chapter_url": { "type": "string", "description": tr!("Un capítulo concreto (de chapters)", "A specific chapter (from chapters)") },
                "chapter_number": { "type": "number", "description": tr!("Número de ese capítulo (para el progreso)", "That chapter's number (for progress)") },
                "chapter_name": { "type": "string", "description": tr!("Nombre de ese capítulo", "That chapter's name") },
                "up_to_number": { "type": "number", "description": tr!("Marcar todos los capítulos hasta este número incluido", "Mark every chapter up to and including this number") },
                "read": { "type": "boolean", "description": tr!("false para desmarcar (por defecto true)", "false to unmark (default true)") }
            })), "required": ["source_id", "manga_url"] },
            "annotations": { "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "set_favorite",
            "title": tr!("Favorito", "Favorite"),
            "description": tr!("Añade o quita un manga de los favoritos del usuario. Al añadirlo intenta vincularlo con AniList si el título coincide exactamente.", "Adds or removes a manga from the user's favorites. When adding, tries to link it to AniList if the title matches exactly."),
            "inputSchema": { "type": "object", "properties": with(json!({
                "title": { "type": "string", "description": tr!("Título del manga", "Manga title") },
                "source": { "type": "string", "description": tr!("Nombre de la fuente", "Source name") },
                "lang": { "type": "string", "description": tr!("Idioma de la fuente", "Source language") },
                "favorite": { "type": "boolean", "description": tr!("false para quitarlo (por defecto true)", "false to remove it (default true)") }
            })), "required": ["source_id", "manga_url"] },
            "annotations": { "idempotentHint": true }
        },
        {
            "name": "open_reader",
            "title": tr!("Abrir lector", "Open reader"),
            "description": tr!("Abre el lector de Han Thỏ en un panel al lado del chat (Windows Terminal, tmux, WezTerm, zellij o kitty) o en una ventana nueva. Por defecto continúa donde se quedó el usuario.", "Opens the Han Thỏ reader in a pane next to the chat (Windows Terminal, tmux, WezTerm, zellij or kitty) or in a new window. By default continues where the user left off."),
            "inputSchema": { "type": "object", "properties": {
                "title": { "type": "string", "description": tr!("Título de un favorito (alternativa a source_id + manga_url)", "Title of a favorite (alternative to source_id + manga_url)") },
                "source_id": { "type": "string", "description": tr!("Id de la fuente", "Source id") },
                "manga_url": { "type": "string", "description": tr!("URL del manga", "Manga URL") },
                "chapter_number": { "type": "number", "description": tr!("Abrir este capítulo", "Open this chapter") },
                "chapter_url": { "type": "string", "description": tr!("Abrir este capítulo (de chapters)", "Open this chapter (from chapters)") },
                "continue": { "type": "boolean", "description": tr!("Seguir donde se quedó (por defecto true)", "Continue where they left off (default true)") },
                "placement": { "type": "string", "enum": ["side", "window"], "description": tr!("side = panel al lado (por defecto), window = ventana nueva", "side = side pane (default), window = new window") }
            } },
            "annotations": { "openWorldHint": false }
        },
        {
            "name": "link_anilist",
            "title": tr!("Vincular con AniList", "Link to AniList"),
            "description": tr!("Vincula un favorito con su obra en AniList (necesario para que el progreso se guarde en la cuenta del usuario) y sincroniza. Sin media_id, vincula solo si el título coincide exactamente; si hay dudas devuelve candidatas: pregunta al usuario y vuelve a llamar con media_id.", "Links a favorite to its AniList entry (needed so progress is saved to the user's account) and syncs. Without media_id it only links on an exact title match; otherwise it returns candidates: ask the user and call again with media_id."),
            "inputSchema": { "type": "object", "properties": with(json!({
                "media_id": { "type": "integer", "description": tr!("Id de la obra en AniList (de las candidatas)", "AniList entry id (from the candidates)") },
                "query": { "type": "string", "description": tr!("Buscar en AniList con otro título (por defecto, el del favorito)", "Search AniList with another title (default: the favorite's)") }
            })), "required": ["source_id", "manga_url"] },
            "annotations": { "openWorldHint": true, "idempotentHint": true }
        },
        {
            "name": "sync_anilist",
            "title": tr!("Sincronizar con AniList", "Sync with AniList"),
            "description": tr!("Envía a AniList el estado y el progreso de un favorito ya vinculado.", "Sends the status and progress of an already linked favorite to AniList."),
            "inputSchema": { "type": "object", "properties": manga_ref, "required": ["source_id", "manga_url"] },
            "annotations": { "openWorldHint": true }
        }
    ])
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!(trf!("falta el parámetro «{name}»", "missing parameter “{name}”")))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn protocol_basics() {
        let server = Server { host: OnceCell::new(), out: Mutex::new(tokio::io::stdout()) };
        let init = server.handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#).await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "hantho");

        assert!(server.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_none(), "las notificaciones no se responden");

        let list = server.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await.unwrap();
        let names: Vec<_> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["search_manga", "library", "chapters", "mark_read", "set_favorite", "open_reader", "link_anilist", "sync_anilist"]);
        for t in list["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }

        let bad = server.handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"chapters","arguments":{}}}"#).await.unwrap();
        assert_eq!(bad["result"]["isError"], true);
        assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("source_id"));

        let unknown = server.handle(r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#).await.unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(server.handle("{no es json").await.unwrap()["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn favorites_and_library_roundtrip() {
        // Escribe favoritos: solo con una carpeta de datos de prueba, nunca en la biblioteca real
        if std::env::var_os("HANTHO_HOME").is_none() {
            return;
        }
        let server = Server { host: OnceCell::new(), out: Mutex::new(tokio::io::stdout()) };
        let add = server.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_favorite","arguments":{"source_id":"42","manga_url":"/gb","title":"Grand Blue","source":"Manga TV","lang":"es"}}}"#).await.unwrap();
        assert!(add["result"]["content"][0]["text"].as_str().unwrap().contains(tr!("añadido", "added")));
        let mark = server.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mark_read","arguments":{"source_id":"42","manga_url":"/gb","chapter_url":"/gb/c7","chapter_number":7}}}"#).await.unwrap();
        assert_eq!(mark["result"]["isError"], false, "{mark}");
        let lib = server.handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"library","arguments":{}}}"#).await.unwrap();
        let text = lib["result"]["content"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        let gb = v["favorites"].as_array().unwrap().iter().find(|f| f["title"] == "Grand Blue").unwrap();
        assert_eq!(gb["progress"], 7);
        assert_eq!(gb["chapters_read"], 1);
    }
}
