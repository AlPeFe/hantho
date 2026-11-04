#[macro_use]
mod i18n;
mod anilist;
mod anilist_auth;
mod backup;
mod cache;
mod catalog;
mod download;
mod ext;
mod health;
mod home;
mod host;
mod launch;
mod library;
mod mcp;
mod model;
mod paths;
mod prefs;
mod reads;
mod render;
mod search;
mod theme;
mod tui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use host::Host;
use search::SearchEvent;

#[derive(Parser)]
#[command(name = "hantho", version, about = tr!("Han Thỏ: lector de manga para la terminal, con extensiones de Mihon (formato Keiyoushi) y sync con AniList.", "Han Thỏ: a terminal manga reader with Mihon extensions (Keiyoushi format) and AniList sync."))]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(about = tr!("Busca en todas las fuentes instaladas (sin TUI)", "Search every installed source (no TUI)"))]
    Search {
        query: String,
        #[arg(help = tr!("Solo fuentes de este idioma (p. ej. es)", "Only sources in this language (e.g. en)"))]
        #[arg(long)]
        lang: Option<String>,
        #[arg(help = tr!("Consultar todas las fuentes: sin tandas y también las omitidas por sus fallos o desactivadas", "Query every source: no batches, and also those skipped for failing or disabled"))]
        #[arg(long)]
        all: bool,
    },
    #[command(about = tr!("Descarga capítulos a una carpeta local o remota", "Download chapters to a local or remote folder"))]
    Download {
        #[arg(help = tr!("Título de un favorito (o usa --source y --url)", "Title of a favorite (or use --source and --url)"))]
        query: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(help = tr!("Número de capítulo", "Chapter number"))]
        #[arg(long)]
        chapter: Option<f32>,
        #[arg(help = tr!("Desde el capítulo número", "From chapter number"))]
        #[arg(long)]
        from: Option<f32>,
        #[arg(help = tr!("Hasta el capítulo número", "Up to chapter number"))]
        #[arg(long)]
        until: Option<f32>,
        #[arg(help = tr!("Solo los capítulos sin leer", "Only unread chapters"))]
        #[arg(long)]
        unread: bool,
        #[arg(help = tr!("Todos los capítulos", "Every chapter"))]
        #[arg(long)]
        all: bool,
        #[arg(help = tr!("Carpeta de destino, o rclone:<remoto>:<ruta> (se recuerda)", "Destination folder, or rclone:<remote>:<path> (remembered)"))]
        #[arg(long)]
        dest: Option<String>,
        #[arg(help = tr!("Formato: un .cbz por capítulo o una carpeta de imágenes (se recuerda)", "Format: one .cbz per chapter or a folder of images (remembered)"))]
        #[arg(long, value_enum)]
        format: Option<download::Format>,
        #[arg(help = tr!("Volver a descargar lo que ya esté descargado", "Download again what's already downloaded"))]
        #[arg(long)]
        force: bool,
    },
    #[command(about = tr!("Gestiona extensiones", "Manage extensions"))]
    #[command(subcommand)]
    Ext(ExtCmd),
    #[command(about = tr!("Cuenta de AniList", "AniList account"))]
    #[command(subcommand)]
    Anilist(AnilistCmd),
    #[command(about = tr!("Backups en formato de Mihon (.tachibk)", "Backups in Mihon format (.tachibk)"))]
    #[command(subcommand)]
    Backup(BackupCmd),
    #[command(about = tr!("Abre directamente un manga (o un capítulo) de tu biblioteca", "Open a manga (or a chapter) from your library directly"))]
    Open {
        #[arg(help = tr!("Título de un favorito (o usa --source y --url)", "Title of a favorite (or use --source and --url)"))]
        query: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(help = tr!("Número de capítulo", "Chapter number"))]
        #[arg(long)]
        chapter: Option<f32>,
        #[arg(long)]
        chapter_url: Option<String>,
        #[arg(help = tr!("Seguir donde te quedaste", "Continue where you left off"))]
        #[arg(long = "continue")]
        cont: bool,
        #[arg(help = tr!("Abrirlo en un panel al lado (Windows Terminal, tmux, WezTerm, zellij, kitty) en vez de aquí", "Open it in a side pane (Windows Terminal, tmux, WezTerm, zellij, kitty) instead of here"))]
        #[arg(long)]
        side: bool,
    },
    #[command(about = tr!("Servidor MCP por stdio, para usar Han Thỏ desde Claude Code, Codex y otros agentes", "MCP server over stdio, to use Han Thỏ from Claude Code, Codex and other agents"))]
    Mcp,
}

#[derive(Subcommand)]
enum BackupCmd {
    #[command(about = tr!("Importa favoritos, capítulos leídos y vínculos con AniList; instala las extensiones que falten", "Import favorites, read chapters and AniList links; installs missing extensions"))]
    Import { file: std::path::PathBuf },
    #[command(about = tr!("Exporta la biblioteca a un .tachibk que Mihon puede restaurar", "Export the library to a .tachibk that Mihon can restore"))]
    Export {
        #[arg(help = tr!("Archivo de salida (por defecto, hantho_<fecha>.tachibk en Descargas)", "Output file (default: hantho_<date>.tachibk in Downloads)"))]
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum AnilistCmd {
    #[command(about = tr!("Inicia sesión: abre el navegador, apruebas y listo (también desde la TUI con `l`)", "Log in: opens the browser, you approve and that's it (also from the TUI with `l`)"))]
    Login {
        #[arg(help = tr!("Client ID de AniList, si este binario no trae uno (se guarda para la próxima)", "AniList client ID, if this binary doesn't ship one (saved for next time)"))]
        #[arg(long)]
        client_id: Option<String>,
    },
    #[command(about = tr!("Muestra la sesión actual", "Show the current session"))]
    Status,
    #[command(about = tr!("Borra el token guardado", "Delete the saved token"))]
    Logout,
}

#[derive(Subcommand)]
enum ExtCmd {
    #[command(about = tr!("Gestiona los repositorios de extensiones (Han Thỏ no trae ninguno)", "Manage extension repositories (Han Thỏ ships none)"))]
    #[command(subcommand)]
    Repo(RepoCmd),
    #[command(about = tr!("Descarga la última versión del catálogo de tus repositorios", "Download the latest catalog from your repositories"))]
    Update,
    #[command(about = tr!("Actualiza las extensiones instaladas que tengan versión nueva", "Update the installed extensions that have a new version"))]
    Upgrade,
    #[command(about = tr!("Busca extensiones en el catálogo", "Search the extension catalog"))]
    Find {
        query: Option<String>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(help = tr!("Excluir extensiones NSFW", "Exclude NSFW extensions"))]
        #[arg(long)]
        sfw: bool,
    },
    #[command(about = tr!("Instala extensiones (por nombre o paquete); con --lang instala todas las de ese idioma", "Install extensions (by name or package); with --lang installs every one in that language"))]
    Install {
        names: Vec<String>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        sfw: bool,
    },
    #[command(about = tr!("Desinstala una extensión por paquete", "Uninstall an extension by package"))]
    Remove { pkg: String },
    #[command(about = tr!("Lista las extensiones instaladas", "List installed extensions"))]
    List,
}

#[derive(Subcommand)]
enum RepoCmd {
    #[command(about = tr!("Añade un repositorio (URL de su index.pb, de index.min.json o del repo en GitHub)", "Add a repository (URL of its index.pb, index.min.json or the GitHub repo)"))]
    Add { url: String },
    #[command(about = tr!("Quita un repositorio", "Remove a repository"))]
    Remove { url: String },
    #[command(about = tr!("Lista los repositorios añadidos", "List added repositories"))]
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    paths::migrate_legacy();
    match cli.cmd {
        None => tui::run(None).await,
        Some(Cmd::Search { query, lang, all }) => cli_search(query, lang, all).await,
        Some(Cmd::Download { query, source, url, title, chapter, from, until, unread, all, dest, format, force }) => {
            let lib = library::Library::load();
            let target = launch::resolve(&lib, query.as_deref(), source.as_deref(), url.as_deref(), title.as_deref(), tui::ChapterTarget::None)?;
            cli_download(target, ChapterSelection { chapter, from, until, unread, all }, dest, format, force).await
        }
        Some(Cmd::Ext(cmd)) => ext_cmd(cmd).await,
        Some(Cmd::Anilist(cmd)) => anilist_cmd(cmd).await,
        Some(Cmd::Backup(cmd)) => backup_cmd(cmd).await,
        Some(Cmd::Open { query, source, url, title, chapter, chapter_url, cont, side }) => {
            let target_chapter = match (chapter, chapter_url) {
                (_, Some(u)) => tui::ChapterTarget::Url(u),
                (Some(n), None) => tui::ChapterTarget::Number(n),
                (None, None) if cont => tui::ChapterTarget::Continue,
                _ => tui::ChapterTarget::None,
            };
            let lib = library::Library::load();
            let target = launch::resolve(&lib, query.as_deref(), source.as_deref(), url.as_deref(), title.as_deref(), target_chapter)?;
            if side {
                println!("{}", trf!("✿ abierto en {}", "✿ opened in {}", launch::launch(&target, launch::Placement::Side)?));
                Ok(())
            } else {
                tui::run(Some(target)).await
            }
        }
        Some(Cmd::Mcp) => mcp::serve().await,
    }
}

async fn backup_cmd(cmd: BackupCmd) -> Result<()> {
    let mut lib = library::Library::load();
    let mut log = reads::ReadLog::load();
    match cmd {
        BackupCmd::Import { file } => {
            let data = backup::read_file(&file)?;
            // Sin repositorios se importa igual; solo no se pueden instalar las extensiones que falten
            let catalog = catalog::load().await.unwrap_or_else(|e| {
                eprintln!("! {e}");
                Vec::new()
            });
            let installed: Vec<String> = ext::list().into_iter().map(|i| i.pkg).collect();
            let s = backup::import(&data, &catalog, &installed, &mut lib, &mut log);
            lib.save()?;
            log.save()?;
            println!("{}", trf!("✓ {} favoritos nuevos · {} ya estaban", "✓ {} new favorites · {} already there", s.added, s.merged));
            println!(
                "{}",
                trf!("✓ {} capítulos marcados como leídos · {} vinculados con AniList", "✓ {} chapters marked as read · {} linked to AniList", s.chapters_read, s.anilist_links)
            );
            if !s.unknown_sources.is_empty() {
                println!("{}", trf!("! sin extensión en tus repositorios: {}", "! no extension in your repositories: {}", s.unknown_sources.join(", ")));
            }
            for e in &s.needed_extensions {
                match ext::install(e).await {
                    Ok(i) => println!("{}", trf!("✓ extensión instalada: {}", "✓ extension installed: {}", i.name)),
                    Err(err) => eprintln!("✕ {}: {err}", e.name),
                }
            }
        }
        BackupCmd::Export { out } => {
            let path = out.unwrap_or_else(backup::default_export_path);
            let data = backup::export(&lib, &log);
            backup::write_file(&data, &path)?;
            let chapters: usize = data.manga.iter().map(|m| m.chapters.len()).sum();
            println!(
                "{}",
                trf!("✓ {} favoritos y {chapters} capítulos leídos → {}", "✓ {} favorites and {chapters} read chapters → {}", data.manga.len(), path.display())
            );
        }
    }
    Ok(())
}

async fn anilist_cmd(cmd: AnilistCmd) -> Result<()> {
    match cmd {
        AnilistCmd::Login { client_id } => {
            if let Some(id) = client_id {
                anilist_auth::save_client_id(&id)?;
            }
            let Some(id) = anilist_auth::client_id() else {
                bail!(trf!(
                    "falta registrar Han Thỏ en AniList (una sola vez):\n  \
                     1. {} → Create New Client\n  \
                     2. Name: Han Thỏ · Redirect URL: {}\n  \
                     3. hantho anilist login --client-id <ID>",
                    "Han Thỏ needs to be registered on AniList (once):\n  \
                     1. {} → Create New Client\n  \
                     2. Name: Han Thỏ · Redirect URL: {}\n  \
                     3. hantho anilist login --client-id <ID>",
                    anilist_auth::DEVELOPER_URL,
                    anilist_auth::REDIRECT
                ));
            };
            println!("{}", tr!("✿ se ha abierto el navegador: aprueba el acceso en AniList y vuelve aquí…", "✿ your browser is open: approve access on AniList and come back here…"));
            let creds = anilist_auth::login(&id).await?;
            println!("{}", trf!("✓ conectado como @{}", "✓ logged in as @{}", creds.user_name));
        }
        AnilistCmd::Status => match anilist::Credentials::load() {
            Some(c) => match anilist::viewer(&c.token).await {
                Ok((_, name)) => println!("{}", trf!("✓ conectado como {name}", "✓ logged in as {name}")),
                Err(e) => println!("{}", trf!("✗ {} (token guardado, pero {e})", "✗ {} (token saved, but {e})", c.user_name)),
            },
            None => println!("{}", tr!("sin sesión; usa `hantho anilist login`", "not logged in; use `hantho anilist login`")),
        },
        AnilistCmd::Logout => {
            anilist::Credentials::delete();
            println!("{}", tr!("sesión cerrada", "logged out"));
        }
    }
    Ok(())
}

/// Qué capítulos descargar. Hay que pedirlo: nada de bajarse una serie entera por descuido.
struct ChapterSelection {
    chapter: Option<f32>,
    from: Option<f32>,
    until: Option<f32>,
    unread: bool,
    all: bool,
}

impl ChapterSelection {
    fn is_empty(&self) -> bool {
        self.chapter.is_none() && self.from.is_none() && self.until.is_none() && !self.unread && !self.all
    }

    fn matches(&self, chapter: &model::Chapter, read: bool) -> bool {
        let n = chapter.chapter_number;
        self.chapter.is_none_or(|c| n == c) && self.from.is_none_or(|f| n >= f) && self.until.is_none_or(|u| n <= u) && !(self.unread && read)
    }
}

async fn cli_download(target: tui::OpenTarget, selection: ChapterSelection, dest: Option<String>, format: Option<download::Format>, force: bool) -> Result<()> {
    if selection.is_empty() {
        bail!(tr!("indica qué capítulos: --chapter, --from, --until, --unread o --all", "say which chapters: --chapter, --from, --until, --unread or --all"));
    }
    let mut prefs = prefs::Prefs::load();
    let dest_text = dest.unwrap_or_else(|| prefs.download_dest());
    let parsed = download::Dest::parse(&dest_text)?;
    let format = format.unwrap_or(prefs.download_format);

    let host = Host::spawn().await?;
    let (sources, _) = host.load_all(&ext::list()).await;
    if !sources.iter().any(|s| s.info.id == target.source_id) {
        bail!(trf!("la fuente {} no está instalada", "source {} isn't installed", target.source_name));
    }
    let manga = model::Manga { url: target.manga_url.clone(), title: target.title.clone(), ..Default::default() };
    let details = host.details(&target.source_id, &manga, false).await.map_err(|e| anyhow::anyhow!(search::short_error(&e.to_string())))?;
    let log = reads::ReadLog::load();
    let key = reads::key(&target.source_id, &target.manga_url);
    let selected: Vec<usize> = (0..details.chapters.len())
        .filter(|&i| selection.matches(&details.chapters[i], log.is_read(&key, &details.chapters[i].url)))
        .collect();
    if selected.is_empty() {
        bail!(tr!("ningún capítulo coincide", "no chapters match"));
    }
    prefs.download_dest = Some(dest_text.clone());
    prefs.download_format = format;
    prefs.save();

    eprintln!("{}", trf!("descargando {} capítulos de {} en {dest_text}…", "downloading {} chapters of {} to {dest_text}…", selected.len(), target.title));
    let mut job = download::Job::new(&target.source_id, &target.manga_url, &target.title, &details.chapters, &selected, parsed, format);
    job.force = force;
    let mut rx = download::start(host, job);
    let mut summary = download::Summary::default();
    while let Some(event) = rx.recv().await {
        match event {
            download::Event::Progress { .. } => {}
            download::Event::Saved { place, missing } if missing.is_empty() => println!("✓ {place}"),
            download::Event::Saved { place, missing } => {
                println!("{}", trf!("✓ {place} (la web no tiene estas páginas: {missing:?})", "✓ {place} (the site doesn't have these pages: {missing:?})"))
            }
            download::Event::Failed { name, error } => eprintln!("✗ {name}: {error}"),
            download::Event::Finished(s) => summary = s,
        }
    }
    eprintln!("{}", trf!("{} guardados · {} ya estaban · {} fallidos", "{} saved · {} already there · {} failed", summary.saved, summary.skipped, summary.failed));
    if let Some(reason) = summary.stopped {
        bail!(reason);
    }
    if summary.failed > 0 {
        bail!(tr!("algunos capítulos no se han podido descargar", "some chapters couldn't be downloaded"));
    }
    Ok(())
}

async fn cli_search(query: String, lang: Option<String>, all: bool) -> Result<()> {
    let installed = ext::list();
    if installed.is_empty() {
        bail!(tr!(
            "no hay extensiones instaladas: añade un repositorio con `hantho ext repo add <url>` e instálalas con `hantho ext install`",
            "no extensions installed: add a repository with `hantho ext repo add <url>` and install them with `hantho ext install`"
        ));
    }
    let started = std::time::Instant::now();
    let host = Host::spawn().await?;
    let (mut sources, errors) = host.load_all(&installed).await;
    for (name, e) in errors {
        eprintln!("! {name}: {}", search::short_error(&e));
    }
    if let Some(lang) = &lang {
        sources.retain(|s| &s.info.lang == lang || s.info.lang == "all");
    }
    let mut left = 0;
    if !all {
        let health = health::Health::load();
        let (active, skipped) = health.partition(sources, library::now());
        if !skipped.is_empty() {
            eprintln!("{}", trf!("{} fuentes omitidas por sus fallos o desactivadas", "{} sources skipped for failing or disabled", skipped.len()));
        }
        let library = library::Library::load();
        let favorites = library.favorites.iter().map(|f| f.source_id.as_str()).collect();
        let (first, rest) = health.first_batch(active, &favorites);
        sources = first;
        left = rest.len();
    }
    eprintln!(
        "{}",
        trf!("{} fuentes listas en {:.1}s; buscando…", "{} sources ready in {:.1}s; searching…", sources.len(), started.elapsed().as_secs_f32())
    );

    let mut rx = search::global(host, sources, query);
    while let Some(ev) = rx.recv().await {
        match ev {
            SearchEvent::Results { source, mangas } if !mangas.is_empty() => {
                println!("{}", trf!("── {} ({}) · {} resultados", "── {} ({}) · {} results", source.info.name, source.info.lang, mangas.len()));
                for m in mangas.iter().take(5) {
                    println!("   {}", m.title);
                }
            }
            SearchEvent::Results { .. } => {}
            SearchEvent::Failed { source, error } => eprintln!("✗ {}: {error}", source.info.name),
            SearchEvent::Done => break,
        }
    }
    eprintln!("{}", trf!("listo en {:.1}s", "done in {:.1}s", started.elapsed().as_secs_f32()));
    if left > 0 {
        eprintln!("{}", trf!("quedan {left} fuentes sin consultar (--all para buscar en todas)", "{left} sources not queried (--all to search them all)"));
    }
    Ok(())
}

async fn ext_cmd(cmd: ExtCmd) -> Result<()> {
    let matches = |e: &catalog::Extension, lang: &Option<String>, sfw: bool| {
        lang.as_ref().is_none_or(|l| e.langs().contains(&l.as_str())) && !(sfw && e.content_warning == 3)
    };
    match cmd {
        ExtCmd::Repo(RepoCmd::Add { url }) => {
            let (url, n) = catalog::add_repo(&url).await?;
            println!("{}", trf!("✓ repositorio añadido: {url} ({n} extensiones)", "✓ repository added: {url} ({n} extensions)"));
        }
        ExtCmd::Repo(RepoCmd::Remove { url }) => {
            if !catalog::remove_repo(&url)? {
                bail!(trf!("{url} no está en tus repositorios", "{url} isn't one of your repositories"));
            }
            println!("{}", tr!("repositorio quitado (las extensiones instaladas siguen funcionando)", "repository removed (installed extensions keep working)"));
        }
        ExtCmd::Repo(RepoCmd::List) => {
            let repos = catalog::repos();
            if repos.is_empty() {
                println!("{}", tr!("no hay repositorios: añade uno con `hantho ext repo add <url>`", "no repositories yet: add one with `hantho ext repo add <url>`"));
            }
            for r in repos {
                println!("{r}");
            }
        }
        ExtCmd::Update => {
            let exts = catalog::update().await?;
            println!("{}", trf!("catálogo actualizado: {} extensiones", "catalog updated: {} extensions", exts.len()));
            let outdated = catalog::outdated(&exts, &ext::list());
            if !outdated.is_empty() {
                println!("{}", trf!("↑ {} instaladas tienen versión nueva: hantho ext upgrade", "↑ {} installed ones have a new version: hantho ext upgrade", outdated.len()));
            }
        }
        ExtCmd::Upgrade => {
            let installed = ext::list();
            let outdated = catalog::outdated(&catalog::update().await?, &installed);
            if outdated.is_empty() {
                println!("{}", trf!("✓ las {} extensiones están al día", "✓ all {} extensions are up to date", installed.len()));
                return Ok(());
            }
            let mut ok = 0;
            for (e, res) in ext::upgrade_all(&outdated).await {
                let old = installed.iter().find(|i| i.pkg == e.package_name).map(|i| i.version_name.as_str()).unwrap_or("?");
                match res {
                    Ok(_) => {
                        ok += 1;
                        println!("↑ {:<32} {old} → {}", e.name, e.version_name);
                    }
                    Err(err) => println!("✕ {:<32} {err}", e.name),
                }
            }
            println!("{}", trf!("{ok} de {} actualizadas", "{ok} of {} updated", outdated.len()));
            println!("{}", tr!("Si Han Thỏ estaba abierto (la TUI o el MCP de un agente), sigue con las versiones anteriores hasta que lo reinicies.", "If Han Thỏ was already open (the TUI or an agent's MCP), it keeps the previous versions until you restart it."));
        }
        ExtCmd::Find { query, lang, sfw } => {
            let catalog = catalog::load().await?;
            let found: Vec<_> = match &query {
                Some(q) => ext::find(&catalog, q),
                None => catalog.iter().collect(),
            };
            let installed = ext::list();
            for e in found.into_iter().filter(|e| matches(e, &lang, sfw)) {
                let mark = if installed.iter().any(|i| i.pkg == e.package_name) { "✓" } else { " " };
                println!("{mark} {:<32} {:<6} {:<6} {}", e.name, e.langs().join(","), e.warning_label(), e.package_name);
            }
        }
        ExtCmd::Install { names, lang, sfw } => {
            let catalog = catalog::load().await?;
            let mut targets: Vec<&catalog::Extension> = Vec::new();
            for name in &names {
                match ext::find(&catalog, name).as_slice() {
                    [] => eprintln!("{}", trf!("✗ '{name}' no está en el catálogo", "✗ '{name}' isn't in the catalog")),
                    [one] => targets.push(one),
                    many => {
                        eprintln!("{}", trf!("'{name}' es ambiguo:", "'{name}' is ambiguous:"));
                        for e in many.iter().take(10) {
                            eprintln!("   {} ({})", e.name, e.package_name);
                        }
                    }
                }
            }
            if names.is_empty() && lang.is_some() {
                targets.extend(catalog.iter().filter(|e| matches(e, &lang, sfw)));
            }
            // Descargas en paralelo, de a 8
            use futures::StreamExt;
            let results: Vec<_> = futures::stream::iter(targets)
                .map(|e| async move { (e, ext::install(e).await) })
                .buffer_unordered(8)
                .collect()
                .await;
            for (e, res) in results {
                match res {
                    Ok(i) => println!("✓ {} {}", i.name, i.version_name),
                    Err(err) => eprintln!("✗ {}: {err}", e.name),
                }
            }
        }
        ExtCmd::Remove { pkg } => {
            if ext::uninstall(&pkg)? {
                println!("{}", trf!("desinstalada {pkg}", "uninstalled {pkg}"));
            } else {
                bail!(trf!("{pkg} no está instalada", "{pkg} isn't installed"));
            }
        }
        ExtCmd::List => {
            for i in ext::list() {
                println!("{:<32} {:<8} {:<6} {}", i.name, i.version_name, i.langs.join(","), i.pkg);
            }
        }
    }
    Ok(())
}
