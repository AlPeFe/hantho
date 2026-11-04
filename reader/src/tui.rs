//! Interfaz de terminal: biblioteca / buscar → elegir fuente → capítulos → leer.
//!
//! Todo lo lento (host, búsquedas, descargas, decodificación, AniList) corre en tareas aparte y
//! vuelve como `Msg` al bucle principal, que es el único que toca el estado y dibuja.

use crate::anilist::{self, Credentials, Media, SyncResult};
use crate::anilist_auth;
use crate::backup::{self, Backup};
use crate::catalog;
use crate::download::{self, Manifest};
use crate::reads::{self, ReadLog};
use crate::ext::Installed;
use crate::health::{self, Health, Record};
use crate::home::{Home, MenuItem};
use crate::host::{Host, LoadedSource};
use crate::library::{AniListLink, LastRead, Library, Status};
use crate::theme::{self, ERROR, FAINT, LAVENDER, MINT, MUTED, ON_PINK, PEACH, PINK, PINK_DIM, TEXT};
use crate::model::{Chapter, Manga, MangaDetails, Page};
use crate::news::{self, News};
use crate::paths;
use crate::prefs::{self, Prefs};
use crate::render::{self, Encoded, Fit, View};
use crate::search::{self, SearchEvent};
use anyhow::Result;
use image::imageops::FilterType;
use image::DynamicImage;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FontSize, StatefulImage};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Páginas que se descargan por delante de la actual.
const PREFETCH: usize = 4;
/// Páginas codificadas que se mantienen alrededor de la actual (el resto se libera).
const KEEP_AROUND: usize = 8;

enum Msg {
    Key(KeyEvent),
    /// Paso de animación (~20 por segundo).
    Tick,
    Resize,
    /// El host, sus fuentes y las extensiones que no han cargado (nombre, error).
    HostReady(Result<(Arc<Host>, Vec<LoadedSource>, Vec<(String, String)>)>),
    Search(u64, SearchEvent),
    Details(u64, Result<MangaDetails>),
    Pages(u64, Result<Vec<Page>>),
    Image(u64, usize, Result<DynamicImage>),
    Encoded(u64, usize, EncodeKey, Encoded),
    AnilistMatches(Result<Vec<Media>>),
    /// (fuente, url del manga, resultado)
    AnilistSynced(String, String, Result<SyncResult>),
    AnilistLogin(Result<Credentials>),
    /// Resultado de buscar en AniList un favorito recién añadido: (fuente, url, título, candidatas)
    AutoLink(String, String, String, Result<Vec<Media>>),
    BackupRead(Result<(Backup, Vec<catalog::Extension>)>),
    /// Extensiones instaladas y cargadas tras importar un backup (y los errores, por nombre).
    BackupExtensions(Vec<Installed>, Vec<LoadedSource>, Vec<String>),
    Catalog(Result<Vec<catalog::Extension>>),
    RepoAdded(Result<(String, usize)>),
    /// Paquete, y la extensión instalada con sus fuentes ya cargadas en el host.
    ExtInstalled(String, Result<(Installed, Vec<LoadedSource>)>),
    /// Catálogo para comprobar si hay versiones nuevas de las extensiones instaladas.
    Updates(Result<Vec<catalog::Extension>>),
    /// Extensiones actualizadas: las instaladas, sus fuentes recargadas, los errores y los ids de
    /// fuente de la versión anterior (para quitarlas).
    Upgraded(Vec<Installed>, Vec<LoadedSource>, Vec<String>, Vec<String>),
    Download(download::Event),
    News(news::Event),
}

/// Una página descargada. En modo Sixel se codifica con nuestro render (`render.rs`) para el
/// tamaño de área actual; en los demás protocolos la dibuja ratatui-image.
struct PageImage {
    img: Arc<DynamicImage>,
    encoded: Option<(EncodeKey, Encoded)>,
    encoding: Option<EncodeKey>,
    /// Otros protocolos: la parte visible ya recortada (para esa vista) y cuánto sobresale.
    proto: Option<(EncodeKey, StatefulProtocol, (u32, u32))>,
}

/// Para qué tamaño de área (en celdas) y qué vista está preparada una página.
type EncodeKey = ((u16, u16), View);

impl PageImage {
    /// Cuánto sobresale la página del área con la vista que se está mostrando (en píxeles).
    fn overflow(&self) -> (u32, u32) {
        match (&self.encoded, &self.proto) {
            (Some((_, enc)), _) => enc.overflow,
            (None, Some((_, _, overflow))) => *overflow,
            _ => (0, 0),
        }
    }
}

/// Qué abrir al arrancar (`hantho open`): un manga y, opcionalmente, un capítulo.
#[derive(Debug, Clone)]
pub struct OpenTarget {
    pub source_id: String,
    pub source_name: String,
    pub lang: String,
    pub manga_url: String,
    pub title: String,
    pub chapter: ChapterTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChapterTarget {
    /// Solo la ficha del manga.
    None,
    /// Donde se quedó (o el primer capítulo sin leer).
    Continue,
    Number(f32),
    Url(String),
}

pub async fn run(open: Option<OpenTarget>) -> Result<()> {
    let picker = make_picker();
    let mut terminal = ratatui::init();
    let mut app = App::new(picker);
    if let Some(target) = open {
        app.start_with(target);
    }
    let result = app.run(&mut terminal).await;
    ratatui::restore();
    result
}

/// Detecta el protocolo gráfico. Windows Terminal soporta Sixel pero no siempre contesta la
/// consulta, así que si estamos en WT y la detección falla, forzamos Sixel.
/// `HANTHO_PROTOCOL` (sixel|kitty|iterm2|halfblocks) y `HANTHO_FONT` (p. ej. 10x20) lo pisan.
fn make_picker() -> Picker {
    let font = std::env::var("HANTHO_FONT")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))));
    let forced = std::env::var("HANTHO_PROTOCOL").ok().and_then(|p| match p.as_str() {
        "sixel" => Some(ProtocolType::Sixel),
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" => Some(ProtocolType::Iterm2),
        "halfblocks" => Some(ProtocolType::Halfblocks),
        _ => None,
    });
    let in_wt = std::env::var_os("WT_SESSION").is_some();

    // from_fontsize está deprecado a favor de la consulta, pero es el único fallback con tamaño de fuente
    #[allow(deprecated)]
    let mut picker = match (font, Picker::from_query_stdio()) {
        (Some((w, h)), _) => Picker::from_fontsize(FontSize::new(w, h)),
        (None, Ok(p)) => p,
        (None, Err(_)) => Picker::from_fontsize(FontSize::new(10, 20)),
    };
    if let Some(p) = forced {
        picker.set_protocol_type(p);
    } else if in_wt && picker.protocol_type() == ProtocolType::Halfblocks {
        picker.set_protocol_type(ProtocolType::Sixel);
    }
    picker
}

#[derive(PartialEq, Clone, Copy)]
enum ChapterInputKind {
    Filter,
    Goto,
    /// Carpeta donde descargar los capítulos visibles que falten.
    Download,
}

struct ChapterInput {
    kind: ChapterInputKind,
    text: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Home,
    Library,
    Extensions,
    AddExtensions,
    AniList,
    Backup,
    Search,
    Sources,
    Manga,
    Reader,
    AnilistPick,
    /// Fuentes que han fallado o se han omitido en la última búsqueda.
    SourceIssues,
}

/// El mismo manga encontrado en varias fuentes.
struct Group {
    key: String,
    title: String,
    entries: Vec<(LoadedSource, Manga)>,
}

/// Una fuente con problemas en la última búsqueda.
struct SourceIssue {
    source: LoadedSource,
    /// Omitida sin consultarla (si no, ha fallado ahora mismo).
    skipped: bool,
    detail: String,
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

struct Reader {
    source: LoadedSource,
    manga: Manga,
    /// URL del manga tal como está en la biblioteca (clave del favorito).
    manga_url: String,
    chapters: Vec<Chapter>,
    chapter: usize,
    pages: Vec<Page>,
    page: usize,
    requested: HashSet<usize>,
    images: HashMap<usize, PageImage>,
    error: Option<String>,
    /// Ajuste, zoom y posición dentro de la página actual.
    view: View,
}

impl Reader {
    /// Vista con la que preparar cada página: la actual tal cual, las demás desde el principio.
    fn view_for(&self, index: usize) -> View {
        if index == self.page {
            self.view
        } else {
            self.view.at_start()
        }
    }

    /// Clave para guardar el ajuste preferido de este manga.
    fn prefs_key(&self) -> String {
        reads::key(&self.source.info.id, &self.manga_url)
    }

    /// Las extensiones suelen listar del capítulo más nuevo al más viejo.
    fn descending(&self) -> bool {
        let first = self.chapters.first().map(|c| c.chapter_number).unwrap_or(0.0);
        let last = self.chapters.last().map(|c| c.chapter_number).unwrap_or(0.0);
        first >= last
    }

    fn next_chapter(&self) -> Option<usize> {
        if self.descending() {
            self.chapter.checked_sub(1)
        } else {
            Some(self.chapter + 1).filter(|&c| c < self.chapters.len())
        }
    }

    fn prev_chapter(&self) -> Option<usize> {
        if self.descending() {
            Some(self.chapter + 1).filter(|&c| c < self.chapters.len())
        } else {
            self.chapter.checked_sub(1)
        }
    }
}

struct App {
    picker: Picker,
    sixel: bool,
    font: (u16, u16),
    /// Área (en celdas) donde va la imagen del lector, medida en el último dibujo.
    reader_area: Rect,
    /// Qué página y en qué posición está pintada ya en pantalla (Sixel se escribe aparte de ratatui).
    sixel_drawn: Option<(usize, Rect, View)>,
    prefs: Prefs,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    host: Option<Arc<Host>>,
    sources: Vec<LoadedSource>,
    screen: Screen,
    status: String,
    quit: bool,
    clear: bool,
    /// Se incrementa al cambiar de contexto para descartar respuestas viejas.
    generation: u64,
    /// Igual, pero para búsquedas: así entrar a un manga no corta la búsqueda en curso.
    search_gen: u64,

    input: String,
    editing: bool,
    query: String,
    groups: Vec<Group>,
    groups_state: ListState,
    pending: usize,
    issues: Vec<SourceIssue>,
    issues_state: ListState,
    health: Health,
    /// Fuentes que quedan por consultar en esta búsqueda (`m` pide la siguiente tanda).
    search_rest: Vec<LoadedSource>,

    group: Option<usize>,
    sources_state: ListState,

    current: Option<(LoadedSource, Manga)>,
    details: Option<MangaDetails>,
    /// Por qué no se ha podido abrir el manga actual.
    details_error: Option<String>,
    chapters_state: ListState,
    /// A dónde vuelve "esc" desde la pantalla del manga.
    manga_back: Screen,

    reader: Option<Reader>,

    library: Library,
    library_state: ListState,
    anilist_matches: Option<Vec<Media>>,
    anilist_state: ListState,
    anilist_user: Option<String>,
    /// Login por navegador en curso.
    anilist_login: bool,
    /// Client ID que se está escribiendo (configuración única, si el binario no trae uno).
    client_input: String,

    reads: ReadLog,
    /// Fecha de modificación de library.json / read.json tal como los conocemos: si cambian
    /// (otro proceso, p. ej. el servidor MCP), se releen.
    library_mtime: Option<std::time::SystemTime>,
    reads_mtime: Option<std::time::SystemTime>,
    /// Capítulo a abrir en cuanto carguen los detalles (`hantho open --chapter …`).
    pending_chapter: Option<ChapterTarget>,
    /// Manga a abrir en cuanto cargue el host.
    pending_open: Option<OpenTarget>,
    /// Índices (en `details.chapters`) de los capítulos visibles: filtrados y en el orden elegido.
    chapter_view: Vec<usize>,
    chapter_filter: String,
    chapters_reversed: bool,
    /// Entrada de texto en la lista de capítulos: filtrar o ir a un número.
    chapter_input: Option<ChapterInput>,

    downloads: Manifest,
    download_queue: VecDeque<download::Job>,
    /// Hay una descarga en marcha: para cancelarla.
    download_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    download_progress: Option<String>,

    /// Capítulos por leer de los favoritos (`u` en la biblioteca).
    news: News,
    news_busy: bool,
    /// Extensiones que no han cargado: (nombre, error).
    load_errors: Vec<(String, String)>,

    backup_files: Vec<std::path::PathBuf>,
    backup_state: ListState,
    backup_busy: bool,
    backup_report: Vec<String>,

    home: Home,
    /// Tamaño de pantalla del último dibujo (para animar los pétalos).
    screen_area: Rect,
    tick: u64,
    /// Hay que redibujar. Los ticks solo marcan esto en pantallas animadas.
    dirty: bool,
    installed: Vec<Installed>,
    /// Instaladas con versión nueva en el catálogo (la versión del catálogo).
    updates: Vec<catalog::Extension>,
    upgrading: bool,
    extensions_state: ListState,
    /// «Añadir extensiones»: catálogo de los repositorios del usuario.
    add: AddExtensions,
}

#[derive(Default)]
struct AddExtensions {
    repos: Vec<String>,
    catalog: Option<Vec<catalog::Extension>>,
    loading: bool,
    filter: String,
    editing: bool,
    lang: Option<String>,
    hide_nsfw: bool,
    state: ListState,
    /// URL del repositorio que se está escribiendo (si está abierto ese cuadro).
    repo_input: Option<String>,
    busy: HashSet<String>,
}

impl AddExtensions {
    /// Índices del catálogo que pasan los filtros.
    fn view(&self) -> Vec<usize> {
        let Some(cat) = &self.catalog else { return Vec::new() };
        let q = self.filter.to_lowercase();
        cat.iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.name.to_lowercase().contains(&q))
            .filter(|(_, e)| self.lang.as_ref().is_none_or(|l| e.langs().contains(&l.as_str())))
            .filter(|(_, e)| !(self.hide_nsfw && e.content_warning == 3))
            .map(|(i, _)| i)
            .collect()
    }

    fn langs(&self) -> Vec<String> {
        let mut langs: Vec<String> =
            self.catalog.iter().flatten().flat_map(|e| e.langs().into_iter().map(String::from)).collect();
        langs.sort();
        langs.dedup();
        langs
    }

    fn reselect(&mut self) {
        self.state.select((!self.view().is_empty()).then_some(0));
    }
}

impl App {
    fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let font = picker.font_size();
        let library = Library::load();
        let has_favorites = !library.favorites.is_empty();
        let installed = crate::ext::list();
        App {
            sixel: picker.protocol_type() == ProtocolType::Sixel,
            font: (font.width, font.height),
            reader_area: Rect::default(),
            sixel_drawn: None,
            prefs: Prefs::load(),
            picker,
            tx,
            rx,
            host: None,
            sources: Vec::new(),
            screen: Screen::Home,
            status: tr!("cargando extensiones…", "loading extensions…").into(),
            quit: false,
            clear: false,
            generation: 0,
            search_gen: 0,
            input: String::new(),
            editing: false,
            query: String::new(),
            groups: Vec::new(),
            groups_state: ListState::default(),
            pending: 0,
            issues: Vec::new(),
            issues_state: ListState::default(),
            health: Health::load(),
            search_rest: Vec::new(),
            group: None,
            sources_state: ListState::default(),
            current: None,
            details: None,
            details_error: None,
            chapters_state: ListState::default(),
            manga_back: Screen::Search,
            reader: None,
            library_state: ListState::default().with_selected(has_favorites.then_some(0)),
            library,
            anilist_matches: None,
            anilist_state: ListState::default(),
            anilist_user: Credentials::load().map(|c| c.user_name),
            anilist_login: false,
            client_input: String::new(),
            reads: ReadLog::load(),
            library_mtime: mtime(&paths::library_file()),
            reads_mtime: mtime(&paths::read_file()),
            pending_chapter: None,
            pending_open: None,
            chapter_view: Vec::new(),
            chapter_filter: String::new(),
            chapters_reversed: false,
            chapter_input: None,
            downloads: Manifest::load(),
            download_queue: VecDeque::new(),
            download_cancel: None,
            download_progress: None,
            news: News::load(),
            news_busy: false,
            load_errors: Vec::new(),
            backup_files: Vec::new(),
            backup_state: ListState::default(),
            backup_busy: false,
            backup_report: Vec::new(),
            home: Home::new(),
            screen_area: Rect::default(),
            tick: 0,
            dirty: true,
            extensions_state: ListState::default().with_selected((!installed.is_empty()).then_some(0)),
            installed,
            updates: Vec::new(),
            upgrading: false,
            add: AddExtensions { repos: catalog::repos(), ..Default::default() },
        }
    }

    async fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        spawn_input(self.tx.clone());
        tokio::task::spawn_blocking(crate::cache::sweep);
        let tick_tx = self.tx.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_millis(50));
            while tick_tx.send(Msg::Tick).is_ok() {
                every.tick().await;
            }
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = async {
                let host = Host::spawn().await?;
                let (sources, errors) = host.load_all(&crate::ext::list()).await;
                Ok((host, sources, errors))
            }
            .await;
            let _ = tx.send(Msg::HostReady(res));
        });

        while !self.quit {
            if std::mem::take(&mut self.clear) {
                terminal.clear()?;
                self.sixel_drawn = None;
                self.dirty = true;
            }
            if std::mem::take(&mut self.dirty) {
                terminal.draw(|f| {
                    self.screen_area = f.area();
                    self.draw(f)
                })?;
                self.flush_sixel()?;
            }
            let Some(msg) = self.rx.recv().await else { break };
            self.update(msg);
            // Procesar todo lo acumulado antes de redibujar (las búsquedas llegan en ráfagas)
            while let Ok(msg) = self.rx.try_recv() {
                self.update(msg);
            }
        }
        Ok(())
    }

    // ───────────────────────────── eventos ─────────────────────────────

    fn update(&mut self, msg: Msg) {
        // En el lector, redibujar por el progreso de una descarga haría parpadear la página
        let in_background = matches!(msg, Msg::Download(_)) && self.screen == Screen::Reader;
        if !matches!(msg, Msg::Tick) && !in_background {
            self.dirty = true;
        }
        match msg {
            Msg::Tick => {
                self.tick += 1;
                if self.screen == Screen::Home {
                    self.home.tick(self.screen_area);
                }
                // La sesión de AniList se inicia en otra terminal: detectarla sola
                if self.screen == Screen::AniList && self.tick % 40 == 0 {
                    self.anilist_user = Credentials::load().map(|c| c.user_name);
                }
                if self.tick % 40 == 20 {
                    self.reload_if_changed();
                }
                if self.animated() {
                    self.dirty = true;
                }
            }
            Msg::Key(key) => self.on_key(key),
            Msg::Resize => self.clear = true,
            Msg::HostReady(Ok((host, sources, errors))) => {
                let failed = errors.len();
                self.status = trf!("{} fuentes listas", "{} sources ready", sources.len())
                    + &if failed > 0 { trf!(" ({failed} extensiones no han cargado: míralo en Extensiones)", " ({failed} extensions failed to load: see Extensions)") } else { String::new() };
                self.load_errors = errors;
                if sources.is_empty() {
                    self.status = tr!("no hay extensiones: añádelas desde «Añadir extensiones» en el menú", "no extensions yet: add them from “Add extensions” in the menu").into();
                }
                self.host = Some(host);
                self.sources = sources;
                if let Some(target) = self.pending_open.take() {
                    match self.sources.iter().find(|s| s.info.id == target.source_id).cloned() {
                        Some(src) => {
                            let manga = Manga { url: target.manga_url, title: target.title, ..Default::default() };
                            self.open_manga(src, manga, false);
                        }
                        None => {
                            self.pending_chapter = None;
                            self.status = trf!("la fuente {} no está instalada", "source {} isn't installed", target.source_name);
                        }
                    }
                }
                if !self.query.is_empty() {
                    self.start_search();
                }
                self.check_updates();
            }
            Msg::HostReady(Err(e)) => self.status = trf!("error iniciando ext-host: {e}", "error starting ext-host: {e}"),
            Msg::Search(generation, ev) => {
                if generation == self.search_gen {
                    self.on_search_event(ev)
                }
            }
            Msg::Details(generation, res) if generation == self.generation => match res {
                Ok(d) => {
                    self.health = Health::load();
                    // Si es un favorito, posicionarse en el último capítulo leído
                    let last = self.current_favorite().and_then(|f| f.last_read.clone());
                    let index = last.and_then(|l| d.chapters.iter().position(|c| c.url == l.chapter_url));
                    self.status = if d.chapters.is_empty() {
                        tr!("esta fuente no tiene capítulos: s para buscar en otras", "this source has no chapters: s to look in others").into()
                    } else {
                        trf!("{} capítulos", "{} chapters", d.chapters.len())
                    };
                    self.details = Some(d);
                    self.chapter_filter.clear();
                    self.chapter_input = None;
                    self.rebuild_chapter_view();
                    match index {
                        Some(i) => self.select_chapter(i),
                        None => self.chapters_state.select((!self.chapter_view.is_empty()).then_some(0)),
                    }
                    if let Some(target) = self.pending_chapter.take() {
                        self.open_chapter_target(&target);
                    }
                }
                Err(e) => {
                    self.pending_chapter = None;
                    self.health = Health::load();
                    let error = search::short_error(&e.to_string());
                    self.status = format!("error: {error}");
                    self.details_error = Some(error);
                }
            },
            Msg::Pages(generation, res) if generation == self.generation => {
                let Some(r) = self.reader.as_mut() else { return };
                match res {
                    Ok(pages) if pages.is_empty() => r.error = Some(tr!("el capítulo no tiene páginas", "this chapter has no pages").into()),
                    Ok(pages) => {
                        // "Continuar" pudo fijar una página antes de saber cuántas hay
                        r.page = r.page.min(pages.len() - 1);
                        r.pages = pages;
                        self.track_progress();
                        self.prefetch();
                    }
                    Err(e) => r.error = Some(search::short_error(&e.to_string())),
                }
            }
            Msg::Image(generation, index, res) if generation == self.generation => {
                let Some(r) = self.reader.as_mut() else { return };
                match res {
                    Ok(img) => {
                        let img = Arc::new(img);
                        r.images.insert(index, PageImage { img, encoded: None, encoding: None, proto: None });
                        if index == r.page {
                            self.clear = true;
                        }
                        self.request_encode(index);
                    }
                    Err(e) if index == r.page => r.error = Some(search::short_error(&e.to_string())),
                    Err(_) => {
                        r.requested.remove(&index);
                    }
                }
            }
            Msg::Encoded(generation, index, key, enc) if generation == self.generation => {
                let Some(page) = self.reader.as_mut().and_then(|r| r.images.get_mut(&index)) else { return };
                if page.encoding == Some(key) {
                    page.encoding = None;
                }
                page.encoded = Some((key, enc));
            }
            Msg::Details(..) | Msg::Pages(..) | Msg::Image(..) | Msg::Encoded(..) => {}
            Msg::AnilistMatches(res) if self.screen == Screen::AnilistPick => match res {
                Ok(list) if list.is_empty() => {
                    self.status = tr!("AniList no ha encontrado ese título", "AniList didn't find that title").into();
                    self.screen = Screen::Manga;
                }
                Ok(list) => {
                    self.anilist_state.select(Some(0));
                    self.anilist_matches = Some(list);
                }
                Err(e) => {
                    self.status = format!("AniList: {e}");
                    self.screen = Screen::Manga;
                }
            },
            Msg::AnilistMatches(_) => {}
            Msg::AnilistSynced(source_id, url, res) => self.on_synced(&source_id, &url, res),
            Msg::BackupRead(res) => self.on_backup_read(res),
            Msg::BackupExtensions(installed, sources, errors) => {
                self.backup_busy = false;
                let n = installed.len();
                self.sources.extend(sources);
                self.installed = crate::ext::list();
                self.backup_report.push(trf!("✓ {n} extensiones instaladas y cargadas", "✓ {n} extensions installed and loaded"));
                for e in errors {
                    self.backup_report.push(format!("✕ {e}"));
                }
                self.status = tr!("importación terminada ✿", "import finished ✿").into();
            }
            Msg::Catalog(res) => {
                self.add.loading = false;
                match res {
                    Ok(cat) => {
                        self.status = trf!("{} extensiones en {} repositorios", "{} extensions in {} repositories", cat.len(), self.add.repos.len());
                        self.updates = catalog::outdated(&cat, &self.installed);
                        self.add.catalog = Some(cat);
                        self.add.reselect();
                    }
                    Err(e) => self.status = format!("✕ {e}"),
                }
            }
            Msg::RepoAdded(res) => {
                self.add.loading = false;
                match res {
                    Ok((url, n)) => {
                        self.add.repo_input = None;
                        self.open_add_extensions();
                        self.status = trf!("✓ repositorio añadido: {n} extensiones · {url}", "✓ repository added: {n} extensions · {url}");
                    }
                    Err(e) => self.status = format!("✕ {e}"),
                }
            }
            Msg::ExtInstalled(pkg, res) => {
                self.add.busy.remove(&pkg);
                match res {
                    Ok((ext, sources)) => {
                        self.status = trf!("✓ {} instalada · {} fuentes nuevas", "✓ {} installed · {} new sources", ext.name, sources.len());
                        self.load_errors.retain(|(name, _)| *name != ext.name);
                        self.sources.retain(|s| !sources.iter().any(|n| n.info.id == s.info.id));
                        self.sources.extend(sources);
                        self.installed = crate::ext::list();
                        self.updates.retain(|u| u.package_name != pkg);
                    }
                    Err(e) => {
                        self.installed = crate::ext::list();
                        self.status = format!("✕ {e}");
                    }
                }
            }
            Msg::Updates(Ok(cat)) => {
                self.updates = catalog::outdated(&cat, &self.installed);
                if !self.updates.is_empty() && self.screen == Screen::Home {
                    let n = self.updates.len();
                    self.status = trf!("↑ {n} extensiones tienen versión nueva: U en Extensiones", "↑ {n} extensions have a new version: U in Extensions");
                }
            }
            // Sin conexión o sin repositorio: ya se comprobará la próxima vez
            Msg::Updates(Err(_)) => {}
            Msg::Upgraded(installed, sources, errors, old_ids) => {
                self.upgrading = false;
                self.load_errors.retain(|(name, _)| !installed.iter().any(|i| i.name == *name));
                self.sources.retain(|s| !old_ids.contains(&s.info.id));
                self.sources.extend(sources);
                self.installed = crate::ext::list();
                self.updates.retain(|u| !installed.iter().any(|i| i.pkg == u.package_name));
                self.status = trf!("✓ {} extensiones actualizadas", "✓ {} extensions updated", installed.len());
                if let Some(first) = errors.first() {
                    self.status += &trf!(" · {} con errores ({first})", " · {} with errors ({first})", errors.len());
                }
            }
            Msg::Download(event) => self.on_download_event(event),
            Msg::News(event) => self.on_news_event(event),
            Msg::AutoLink(source_id, url, title, res) => self.on_auto_link(&source_id, &url, &title, res),
            Msg::AnilistLogin(res) => {
                self.anilist_login = false;
                match res {
                    Ok(creds) => {
                        self.status = trf!("✓ conectado como @{}", "✓ logged in as @{}", creds.user_name);
                        self.anilist_user = Some(creds.user_name);
                    }
                    Err(e) => self.status = format!("✕ AniList: {e}"),
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.screen {
            Screen::Home => self.key_home(key),
            Screen::AddExtensions => self.key_add_extensions(key),
            Screen::Extensions => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Home,
                KeyCode::Char('a') => self.open_add_extensions(),
                KeyCode::Char('U') | KeyCode::Char('u') => self.upgrade_all(),
                KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.extensions_state),
                KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.extensions_state, self.installed.len()),
                _ => {}
            },
            Screen::AniList => self.key_anilist(key),
            Screen::Backup => self.key_backup(key),
            Screen::Library => self.key_library(key),
            Screen::Search => self.key_search(key),
            Screen::Sources => self.key_sources(key),
            Screen::Manga => self.key_manga(key),
            Screen::Reader => self.key_reader(key),
            Screen::AnilistPick => self.key_anilist_pick(key),
            Screen::SourceIssues => self.key_source_issues(key),
        }
    }

    /// Pantallas con algo en movimiento: solo ahí los ticks redibujan.
    /// (En el lector con la página ya en pantalla nunca: redibujar borraría el Sixel.)
    fn animated(&self) -> bool {
        match self.screen {
            Screen::Home => true,
            Screen::Search => self.editing || self.pending > 0,
            Screen::AnilistPick => self.anilist_matches.is_none(),
            Screen::AniList => self.anilist_login || anilist_auth::client_id().is_none(),
            Screen::Backup => self.backup_busy,
            Screen::AddExtensions => {
                self.add.loading || self.add.editing || self.add.repo_input.is_some() || !self.add.busy.is_empty()
            }
            Screen::Manga => (self.details.is_none() && self.details_error.is_none()) || self.chapter_input.is_some(),
            Screen::Reader => self.reader.as_ref().is_some_and(|r| {
                r.error.is_none() && r.images.get(&r.page).is_none_or(|p| self.sixel && p.encoded.is_none())
            }),
            _ => false,
        }
    }

    fn key_anilist(&mut self, key: KeyEvent) {
        if self.anilist_login {
            if key.code == KeyCode::Esc {
                self.status = tr!("login en curso: si has cerrado el navegador, se cancela solo a los 5 min", "login in progress: if you closed the browser, it cancels itself after 5 min").into();
            }
            return;
        }
        let configured = anilist_auth::client_id().is_some();
        match key.code {
            KeyCode::Esc => self.screen = Screen::Home,
            // Configuración única: escribir/pegar el client ID
            KeyCode::Char(c) if !configured && c.is_ascii_digit() => self.client_input.push(c),
            KeyCode::Backspace if !configured => {
                self.client_input.pop();
            }
            KeyCode::Char('d') if !configured => anilist::open_browser(anilist_auth::DEVELOPER_URL),
            KeyCode::Enter if !configured => match anilist_auth::save_client_id(&self.client_input) {
                Ok(()) => self.start_anilist_login(),
                Err(e) => self.status = format!("✕ {e}"),
            },
            KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Home,
            KeyCode::Char('l') | KeyCode::Enter => self.start_anilist_login(),
            KeyCode::Char('o') if self.anilist_user.is_some() => {
                Credentials::delete();
                self.anilist_user = None;
                self.status = tr!("sesión de AniList cerrada", "logged out of AniList").into();
            }
            _ => {}
        }
    }

    fn start_anilist_login(&mut self) {
        let Some(id) = anilist_auth::client_id() else { return };
        self.anilist_login = true;
        self.status = tr!("se ha abierto el navegador: aprueba el acceso en AniList", "your browser is open: approve access on AniList").into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::AnilistLogin(anilist_auth::login(&id).await));
        });
    }

    fn key_home(&mut self, key: KeyEvent) {
        const ITEMS: usize = 7;
        let choice = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit = true;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.home.selected = (self.home.selected + ITEMS - 1) % ITEMS;
                return;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.home.selected = (self.home.selected + 1) % ITEMS;
                return;
            }
            KeyCode::Char(c @ '1'..='7') => c as usize - '1' as usize,
            KeyCode::Enter => self.home.selected,
            KeyCode::Char('/') => 1,
            _ => return,
        };
        self.home.selected = choice;
        match choice {
            0 => self.screen = Screen::Library,
            1 => {
                self.screen = Screen::Search;
                self.editing = true;
            }
            2 => {
                self.installed = crate::ext::list();
                self.screen = Screen::Extensions;
            }
            3 => self.open_add_extensions(),
            4 => {
                self.anilist_user = Credentials::load().map(|c| c.user_name);
                self.screen = Screen::AniList;
            }
            5 => {
                self.backup_files = backup::find_backups();
                self.backup_state.select(Some(0));
                self.screen = Screen::Backup;
            }
            _ => self.quit = true,
        }
    }

    /// Comprueba en segundo plano si hay versiones nuevas: con el índice guardado, o descargándolo
    /// si tiene más de un día.
    fn check_updates(&mut self) {
        if catalog::repos().is_empty() {
            return;
        }
        let stale = catalog::age().is_none_or(|age| age > Duration::from_secs(24 * 60 * 60));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let cat = if stale { catalog::update().await } else { catalog::load().await };
            let _ = tx.send(Msg::Updates(cat));
        });
    }

    /// Instala la versión nueva de todas las extensiones desactualizadas y las recarga en el host.
    fn upgrade_all(&mut self) {
        if self.upgrading || self.updates.is_empty() {
            return;
        }
        self.upgrading = true;
        self.status = trf!("actualizando {} extensiones…", "updating {} extensions…", self.updates.len());
        let (tx, host, exts) = (self.tx.clone(), self.host.clone(), self.updates.clone());
        tokio::spawn(async move {
            let mut installed = Vec::new();
            let mut errors = Vec::new();
            for (e, res) in crate::ext::upgrade_all(&exts).await {
                match res {
                    Ok(i) => installed.push(i),
                    Err(err) => errors.push(format!("{}: {err}", e.name)),
                }
            }
            let old_ids: Vec<String> = exts
                .iter()
                .filter(|e| installed.iter().any(|i| i.pkg == e.package_name))
                .flat_map(|e| e.sources.iter().map(|s| s.id.to_string()))
                .collect();
            let mut sources = Vec::new();
            if let Some(host) = host {
                let (loaded, failed) = host.load_all(&installed).await;
                sources = loaded;
                errors.extend(failed.into_iter().map(|(name, e)| format!("{name}: {}", search::short_error(&e))));
            }
            let _ = tx.send(Msg::Upgraded(installed, sources, errors, old_ids));
        });
    }

    fn open_add_extensions(&mut self) {
        self.screen = Screen::AddExtensions;
        self.installed = crate::ext::list();
        self.add.repos = catalog::repos();
        if self.add.repos.is_empty() {
            self.add.catalog = None;
            self.add.repo_input = Some(String::new());
            return;
        }
        self.add.loading = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Catalog(catalog::load().await));
        });
    }

    fn key_add_extensions(&mut self, key: KeyEvent) {
        let add = &mut self.add;
        if let Some(input) = add.repo_input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    add.repo_input = None;
                    if add.repos.is_empty() {
                        self.screen = Screen::Home;
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Enter if !input.trim().is_empty() && !add.loading => {
                    let url = input.clone();
                    add.loading = true;
                    self.status = tr!("comprobando el repositorio…", "checking the repository…").into();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::RepoAdded(catalog::add_repo(&url).await));
                    });
                }
                _ => {}
            }
            return;
        }
        if add.editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Down => add.editing = false,
                KeyCode::Backspace => {
                    add.filter.pop();
                }
                KeyCode::Char(c) => add.filter.push(c),
                _ => {}
            }
            add.reselect();
            return;
        }
        let view = add.view();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Home,
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut add.state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut add.state, view.len()),
            KeyCode::PageDown => page_move(&mut add.state, view.len(), 20),
            KeyCode::PageUp => page_move(&mut add.state, view.len(), -20),
            KeyCode::Char('/') => add.editing = true,
            KeyCode::Char('l') => {
                let langs = add.langs();
                add.lang = match &add.lang {
                    None => langs.first().cloned(),
                    Some(l) => langs.iter().skip_while(|x| *x != l).nth(1).cloned(),
                };
                add.reselect();
            }
            KeyCode::Char('n') => {
                add.hide_nsfw = !add.hide_nsfw;
                add.reselect();
            }
            KeyCode::Char('r') => add.repo_input = Some(String::new()),
            KeyCode::Char('u') if !add.loading => {
                add.loading = true;
                self.status = tr!("actualizando el catálogo…", "updating the catalog…").into();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Catalog(catalog::update().await));
                });
            }
            KeyCode::Enter => {
                let Some(ext) = add.state.selected().and_then(|i| view.get(i)).and_then(|&i| add.catalog.as_ref()?.get(i)).cloned()
                else {
                    return;
                };
                if add.busy.contains(&ext.package_name) {
                    return;
                }
                let current = self.installed.iter().find(|i| i.pkg == ext.package_name);
                // Instalada y al día: enter la quita. Si hay versión nueva, enter actualiza.
                if current.is_some_and(|i| i.version_code >= ext.version_code) {
                    match crate::ext::uninstall(&ext.package_name) {
                        Ok(_) => {
                            let ids: Vec<String> = ext.sources.iter().map(|s| s.id.to_string()).collect();
                            self.sources.retain(|s| !ids.contains(&s.info.id));
                            self.installed = crate::ext::list();
                            self.status = trf!("{} desinstalada", "{} uninstalled", ext.name);
                        }
                        Err(e) => self.status = format!("✕ {e}"),
                    }
                    return;
                }
                add.busy.insert(ext.package_name.clone());
                self.status = trf!("instalando {}…", "installing {}…", ext.name);
                let (tx, host) = (self.tx.clone(), self.host.clone());
                tokio::spawn(async move {
                    let res = async {
                        let installed = crate::ext::install(&ext).await?;
                        // Sin host todavía: se cargará con el resto cuando arranque
                        let Some(host) = host else { return Ok((installed, Vec::new())) };
                        let (sources, errors) = host.load_all(std::slice::from_ref(&installed)).await;
                        if let Some((_, e)) = errors.first() {
                            anyhow::bail!(trf!("{} se ha instalado, pero no carga: {}", "{} was installed but doesn't load: {}", installed.name, search::short_error(e)));
                        }
                        Ok((installed, sources))
                    }
                    .await;
                    let _ = tx.send(Msg::ExtInstalled(ext.package_name.clone(), res));
                });
            }
            _ => {}
        }
    }

    fn key_library(&mut self, key: KeyEvent) {
        let len = self.library.favorites.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Home,
            KeyCode::Char('/') | KeyCode::Char('s') | KeyCode::Tab => {
                self.screen = Screen::Search;
                self.editing = true;
            }
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.library_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.library_state, len),
            KeyCode::Char('u') => self.check_news(),
            KeyCode::Char('d') | KeyCode::Delete => {
                let Some(i) = self.library_state.selected().filter(|&i| i < len) else { return };
                let fav = self.library.favorites.remove(i);
                self.save_library();
                self.status = trf!("quitado de favoritos: {}", "removed from favorites: {}", fav.manga.title);
                self.library_state.select(if self.library.favorites.is_empty() { None } else { Some(i.min(len - 2)) });
            }
            KeyCode::Enter => {
                let Some(fav) = self.library_state.selected().and_then(|i| self.library.favorites.get(i)).cloned() else {
                    return;
                };
                match self.sources.iter().find(|s| s.info.id == fav.source_id).cloned() {
                    Some(src) => {
                        self.manga_back = Screen::Library;
                        self.open_manga(src, fav.manga, false);
                    }
                    None if self.host.is_none() => self.status = tr!("esperando a que carguen las extensiones…", "waiting for extensions to load…").into(),
                    None => self.status = trf!("la fuente {} no está instalada", "source {} isn't installed", fav.source_name),
                }
            }
            _ => {}
        }
    }

    fn key_anilist_pick(&mut self, key: KeyEvent) {
        let len = self.anilist_matches.as_ref().map_or(0, Vec::len);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Manga,
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.anilist_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.anilist_state, len),
            KeyCode::Enter => {
                let Some(media) = self.anilist_state.selected().and_then(|i| self.anilist_matches.as_ref()?.get(i)).cloned()
                else {
                    return;
                };
                let Some(fav) = self.current_favorite_mut() else { return };
                fav.anilist = Some(AniListLink {
                    media_id: media.id,
                    title: media.display_title(),
                    synced_progress: None,
                    synced_status: None,
                    synced_at: None,
                });
                self.save_library();
                self.screen = Screen::Manga;
                self.anilist_sync(false);
            }
            _ => {}
        }
    }

    fn key_search(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cycle_search_langs();
            return;
        }
        if self.editing {
            match key.code {
                KeyCode::Enter if !self.input.trim().is_empty() => {
                    self.query = self.input.trim().to_string();
                    self.editing = false;
                    self.start_search();
                }
                KeyCode::Char(c) => self.input.push(c),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Esc => self.screen = Screen::Home,
                KeyCode::Down | KeyCode::Tab if !self.groups.is_empty() => self.editing = false,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.screen = Screen::Home,
            KeyCode::Char('/') | KeyCode::Char('s') => self.editing = true,
            KeyCode::Char('b') | KeyCode::Tab => self.screen = Screen::Library,
            KeyCode::Char('l') => self.cycle_search_langs(),
            KeyCode::Char('f') => self.open_source_issues(),
            KeyCode::Char('m') => self.search_more(),
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.groups_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.groups_state, self.groups.len()),
            KeyCode::Enter => {
                let Some(i) = self.groups_state.selected() else { return };
                if self.groups[i].entries.len() == 1 {
                    let (src, manga) = self.groups[i].entries[0].clone();
                    self.group = None;
                    self.manga_back = Screen::Search;
                    self.open_manga(src, manga, false);
                } else {
                    self.show_group(i, None);
                }
            }
            _ => {}
        }
    }

    /// Enseña las fuentes de un grupo, primero las que mejor funcionan. La fuente `last` (la que
    /// acaba de fallar) va al final.
    fn show_group(&mut self, index: usize, last: Option<&str>) {
        let health = &self.health;
        self.groups[index].entries.sort_by_key(|(s, _)| {
            let record = health.get(&s.info.id);
            let problem = record.is_some_and(|r| r.disabled || r.has_problem());
            (last == Some(s.info.id.as_str()), problem, record.and_then(|r| r.latency_ms).unwrap_or(u64::MAX))
        });
        self.group = Some(index);
        self.sources_state.select(Some(0));
        self.screen = Screen::Sources;
    }

    fn open_source_issues(&mut self) {
        if self.issues.is_empty() {
            self.status = tr!("ninguna fuente ha dado problemas", "no sources had problems").into();
            return;
        }
        self.health = Health::load();
        self.issues_state.select(Some(0));
        self.screen = Screen::SourceIssues;
    }

    fn key_source_issues(&mut self, key: KeyEvent) {
        let len = self.issues.len();
        let selected = self.issues_state.selected().filter(|&i| i < len);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Search,
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.issues_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.issues_state, len),
            KeyCode::Char('U') => self.upgrade_all(),
            KeyCode::Char('d') => {
                let Some(i) = selected else { return };
                let id = self.issues[i].source.info.id.clone();
                let disabled = !self.health.get(&id).is_some_and(|r| r.disabled);
                Health::update(|h| {
                    h.set_disabled(&id, disabled);
                    if !disabled {
                        h.forgive(&id);
                    }
                });
                self.health = Health::load();
                let name = &self.issues[i].source.info.name;
                self.status = if disabled {
                    trf!("{name} desactivada: no se buscará en ella", "{name} disabled: it won't be searched")
                } else {
                    trf!("{name} activada", "{name} enabled")
                };
            }
            // Las respuestas de una búsqueda en curso se mezclarían con las del reintento
            KeyCode::Char('r') | KeyCode::Char('R') if self.pending > 0 => {
                self.status = tr!("espera a que termine la búsqueda", "wait for the search to finish").into();
            }
            KeyCode::Char('r') => {
                let Some(i) = selected else { return };
                let issue = self.issues.remove(i);
                self.retry_sources(vec![issue.source]);
                if self.issues.is_empty() {
                    self.screen = Screen::Search;
                } else {
                    self.issues_state.select(Some(i.min(self.issues.len() - 1)));
                }
            }
            KeyCode::Char('R') => {
                let sources = std::mem::take(&mut self.issues).into_iter().map(|issue| issue.source).collect();
                self.retry_sources(sources);
                self.screen = Screen::Search;
            }
            _ => {}
        }
    }

    /// Olvida los fallos de estas fuentes y las vuelve a consultar, sin perder los resultados que ya hay.
    fn retry_sources(&mut self, sources: Vec<LoadedSource>) {
        Health::update(|h| sources.iter().for_each(|s| h.forgive(&s.info.id)));
        self.health = Health::load();
        // Las desactivadas siguen fuera hasta que se activen
        let (sources, disabled): (Vec<_>, Vec<_>) =
            sources.into_iter().partition(|s| !self.health.get(&s.info.id).is_some_and(|r| r.disabled));
        self.issues.extend(disabled.into_iter().map(|source| SourceIssue { source, skipped: true, detail: String::new() }));
        if !sources.is_empty() {
            self.spawn_search(sources);
        }
    }

    /// Busca el manga actual en otras fuentes (cuando esta falla o no tiene capítulos).
    fn find_alternatives(&mut self) {
        let Some((source, manga)) = self.current.clone() else { return };
        let title = self.details.as_ref().map(|d| d.manga.title.clone()).filter(|t| !t.is_empty()).unwrap_or(manga.title);
        let key = search::normalize(&title);
        if key.is_empty() {
            self.status = tr!("este manga no tiene título con el que buscar", "this manga has no title to search for").into();
            return;
        }
        let group = self.groups.iter().position(|g| g.key == key && g.entries.iter().any(|(s, _)| s.info.id != source.info.id));
        let same_search = search::normalize(&self.query) == key;
        if group.is_none() && same_search && self.pending == 0 && self.search_rest.is_empty() {
            self.status = tr!("ninguna otra fuente lo tiene", "no other source has it").into();
            return;
        }
        self.generation += 1;
        match group {
            Some(i) => self.show_group(i, Some(&source.info.id)),
            None => {
                // Desde la biblioteca, o con otra búsqueda en pantalla: buscar este título
                self.manga_back = Screen::Search;
                self.screen = Screen::Search;
                self.editing = false;
                if !same_search {
                    self.group = None;
                    self.query = title.clone();
                    self.input = title;
                    self.start_search();
                } else if self.pending == 0 {
                    self.search_more();
                }
            }
        }
    }

    /// Busca capítulos nuevos en lo que se está leyendo.
    fn check_news(&mut self) {
        if self.news_busy {
            return;
        }
        let Some(host) = self.host.clone() else {
            self.status = tr!("esperando a que carguen las extensiones…", "waiting for extensions to load…").into();
            return;
        };
        self.news_busy = true;
        self.status = tr!("comprobando capítulos nuevos…", "checking for new chapters…").into();
        let loaded = self.sources.iter().map(|s| s.info.id.clone()).collect();
        let mut rx = news::check(host, self.library.favorites.clone(), loaded);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if tx.send(Msg::News(event)).is_err() {
                    break;
                }
            }
        });
    }

    fn on_news_event(&mut self, event: news::Event) {
        match event {
            news::Event::Found { .. } | news::Event::Failed { .. } => {}
            news::Event::Progress { done, total } => {
                self.news = News::load();
                self.status = trf!("comprobando capítulos nuevos… {done}/{total}", "checking for new chapters… {done}/{total}");
            }
            news::Event::Done { checked, failed, skipped } => {
                self.news_busy = false;
                self.news = News::load();
                let with_news = self.library.favorites.iter().filter(|f| self.news.count(f, &self.reads) > 0).count();
                self.status = trf!("{with_news} mangas con capítulos por leer · {checked} comprobados", "{with_news} manga with chapters to read · {checked} checked");
                if failed + skipped > 0 {
                    self.status = trf!("{} · {failed} fallidos · {skipped} omitidos", "{} · {failed} failed · {skipped} skipped", self.status);
                }
            }
        }
    }

    /// Capítulos visibles (según el filtro) que aún no están descargados.
    fn missing_chapters(&self) -> Vec<usize> {
        let key = self.current_key().unwrap_or_default();
        let Some(details) = &self.details else { return Vec::new() };
        self.chapter_view.iter().copied().filter(|&i| !self.downloads.contains(&key, &details.chapters[i].url)).collect()
    }

    /// Pone capítulos en la cola de descargas y recuerda el destino.
    fn download_chapters(&mut self, indices: &[usize], dest_text: &str) {
        let (Some((source, manga)), Some(details)) = (&self.current, &self.details) else { return };
        if self.host.is_none() {
            self.status = tr!("esperando a que carguen las extensiones…", "waiting for extensions to load…").into();
            return;
        }
        if indices.is_empty() {
            self.status = tr!("no hay nada que descargar", "nothing to download").into();
            return;
        }
        let dest = match download::Dest::parse(dest_text) {
            Ok(dest) => dest,
            Err(e) => {
                self.status = format!("✕ {e}");
                return;
            }
        };
        let title = if details.manga.title.is_empty() { &manga.title } else { &details.manga.title };
        let job = download::Job::new(&source.info.id, &manga.url, title, &details.chapters, indices, dest, self.prefs.download_format);
        self.status = trf!("descarga: {} capítulos a {dest_text}", "download: {} chapters to {dest_text}", indices.len());
        self.prefs.download_dest = Some(dest_text.trim().to_string());
        self.prefs.save();
        self.download_queue.push_back(job);
        self.next_download();
    }

    fn next_download(&mut self) {
        if self.download_cancel.is_some() {
            return;
        }
        let Some(host) = self.host.clone() else { return };
        let Some(job) = self.download_queue.pop_front() else { return };
        self.download_cancel = Some(job.cancel.clone());
        let mut rx = download::start(host, job);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if tx.send(Msg::Download(event)).is_err() {
                    break;
                }
            }
        });
    }

    fn cancel_downloads(&mut self) {
        let Some(cancel) = &self.download_cancel else {
            self.status = tr!("no hay ninguna descarga en marcha", "no download in progress").into();
            return;
        };
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.download_queue.clear();
        self.status = tr!("cancelando la descarga…", "cancelling the download…").into();
    }

    fn on_download_event(&mut self, event: download::Event) {
        match event {
            download::Event::Progress { chapter, chapters, name, page, pages } => {
                self.download_progress = Some(if pages == 0 {
                    format!("⬇ {chapter}/{chapters} {name}")
                } else {
                    format!("⬇ {chapter}/{chapters} {name} · {page}/{pages}")
                });
            }
            download::Event::Saved { .. } => self.downloads = Manifest::load(),
            download::Event::Failed { .. } => {}
            download::Event::Finished(summary) => {
                self.download_cancel = None;
                self.download_progress = None;
                self.downloads = Manifest::load();
                let mut text = if summary.cancelled {
                    trf!("descarga cancelada · {} guardados", "download cancelled · {} saved", summary.saved)
                } else {
                    trf!("✓ {} capítulos guardados", "✓ {} chapters saved", summary.saved)
                };
                if summary.skipped > 0 {
                    text = trf!("{text} · {} ya estaban", "{text} · {} already there", summary.skipped);
                }
                if summary.missing_pages > 0 {
                    text = trf!("{text} · la web no tenía {} páginas", "{text} · the site was missing {} pages", summary.missing_pages);
                }
                if summary.failed > 0 {
                    text = trf!("{text} · {} han fallado", "{text} · {} failed", summary.failed);
                }
                if let Some(reason) = summary.stopped.or(summary.last_error) {
                    text = format!("{text} ({reason})");
                }
                if self.screen != Screen::Reader {
                    self.status = text;
                }
                self.next_download();
            }
        }
    }

    fn key_sources(&mut self, key: KeyEvent) {
        let Some(g) = self.group else { return };
        let len = self.groups[g].entries.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Search,
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.sources_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.sources_state, len),
            KeyCode::Enter => {
                if let Some(i) = self.sources_state.selected() {
                    let (src, manga) = self.groups[g].entries[i].clone();
                    self.manga_back = Screen::Sources;
                    self.open_manga(src, manga, false);
                }
            }
            _ => {}
        }
    }

    fn key_manga(&mut self, key: KeyEvent) {
        if self.chapter_input.is_some() {
            self.key_chapter_input(key);
            return;
        }
        let len = self.chapter_view.len();
        match key.code {
            KeyCode::Char('/') if self.details.is_some() => {
                self.chapter_input = Some(ChapterInput { kind: ChapterInputKind::Filter, text: self.chapter_filter.clone() })
            }
            KeyCode::Char('#') | KeyCode::Char(':') if self.details.is_some() => {
                self.chapter_input = Some(ChapterInput { kind: ChapterInputKind::Goto, text: String::new() })
            }
            KeyCode::Char('o') => {
                let selected = self.selected_chapter();
                self.chapters_reversed = !self.chapters_reversed;
                self.rebuild_chapter_view();
                if let Some(i) = selected {
                    self.select_chapter(i);
                }
            }
            KeyCode::Char('m') => self.toggle_selected_read(),
            KeyCode::Char('M') => self.mark_read_up_to_selected(),
            KeyCode::Char('[') => page_move(&mut self.chapters_state, len, -100),
            KeyCode::Char(']') => page_move(&mut self.chapters_state, len, 100),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                self.generation += 1;
                self.screen = self.manga_back;
            }
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('e') => {
                let Some(fav) = self.current_favorite_mut() else {
                    self.status = tr!("primero añádelo a favoritos (f)", "add it to favorites first (f)").into();
                    return;
                };
                fav.status = fav.status.next();
                let label = fav.status.label();
                self.save_library();
                self.status = trf!("estado: {label} (pulsa 'a' para enviarlo a AniList)", "status: {label} (press 'a' to send it to AniList)");
            }
            KeyCode::Char('a') => self.anilist_sync(false),
            KeyCode::Char('A') => self.anilist_sync(true),
            KeyCode::Char('s') => self.find_alternatives(),
            KeyCode::Char('d') => {
                if let Some(i) = self.selected_chapter() {
                    let dest = self.prefs.download_dest();
                    self.download_chapters(&[i], &dest);
                }
            }
            KeyCode::Char('D') if self.details.is_some() => {
                self.chapter_input = Some(ChapterInput { kind: ChapterInputKind::Download, text: self.prefs.download_dest() })
            }
            KeyCode::Char('X') => self.cancel_downloads(),
            KeyCode::Char('r') => {
                if let Some((src, manga)) = self.current.clone() {
                    self.open_manga(src, manga, true);
                }
            }
            KeyCode::Char('c') => {
                // Continuar: abrir el último capítulo leído en la página donde quedó
                let last = self.current_favorite().and_then(|f| f.last_read.clone());
                let index = last.as_ref().and_then(|l| self.details.as_ref()?.chapters.iter().position(|c| c.url == l.chapter_url));
                match (index, last) {
                    (Some(i), Some(l)) => {
                        self.open_chapter(i);
                        if let Some(r) = self.reader.as_mut() {
                            r.page = l.page;
                        }
                    }
                    _ => self.status = tr!("no hay ninguna lectura que continuar", "nothing to continue reading").into(),
                }
            }
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.chapters_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.chapters_state, len),
            KeyCode::PageUp => page_move(&mut self.chapters_state, len, -20),
            KeyCode::PageDown => page_move(&mut self.chapters_state, len, 20),
            KeyCode::Home | KeyCode::Char('g') => self.chapters_state.select((len > 0).then_some(0)),
            KeyCode::End | KeyCode::Char('G') => self.chapters_state.select(len.checked_sub(1)),
            KeyCode::Enter => {
                if let Some(i) = self.selected_chapter() {
                    self.open_chapter(i);
                }
            }
            _ => {}
        }
    }

    /// Escribiendo un filtro (`/`, en vivo) o un número de capítulo (`#`).
    fn key_chapter_input(&mut self, key: KeyEvent) {
        let Some(input) = self.chapter_input.as_mut() else { return };
        match key.code {
            KeyCode::Tab if input.kind == ChapterInputKind::Download => {
                self.prefs.download_format = self.prefs.download_format.toggled();
                return;
            }
            KeyCode::Char(c) => input.text.push(c),
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Esc => {
                if input.kind == ChapterInputKind::Filter {
                    self.chapter_filter.clear();
                    self.rebuild_chapter_view();
                }
                self.chapter_input = None;
                return;
            }
            KeyCode::Enter => {
                let input = self.chapter_input.take().unwrap();
                match input.kind {
                    ChapterInputKind::Goto => self.goto_chapter_number(&input.text),
                    ChapterInputKind::Download => {
                        let pending = self.missing_chapters();
                        self.download_chapters(&pending, &input.text);
                    }
                    ChapterInputKind::Filter => {}
                }
                return;
            }
            _ => return,
        }
        if input.kind == ChapterInputKind::Filter {
            self.chapter_filter = input.text.clone();
            let selected = self.selected_chapter();
            self.rebuild_chapter_view();
            match selected.filter(|i| self.chapter_view.contains(i)) {
                Some(i) => self.select_chapter(i),
                None => self.chapters_state.select((!self.chapter_view.is_empty()).then_some(0)),
            }
        }
    }

    fn key_backup(&mut self, key: KeyEvent) {
        if self.backup_busy {
            return;
        }
        let len = self.backup_files.len() + 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.screen = Screen::Home,
            KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.backup_state),
            KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.backup_state, len),
            KeyCode::Char('r') => self.backup_files = backup::find_backups(),
            KeyCode::Enter => match self.backup_state.selected() {
                Some(0) => self.export_backup(),
                Some(i) => {
                    if let Some(path) = self.backup_files.get(i - 1).cloned() {
                        self.backup_busy = true;
                        self.backup_report = vec![trf!("leyendo {}…", "reading {}…", path.display())];
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let res = async {
                                let backup = tokio::task::spawn_blocking(move || backup::read_file(&path)).await??;
                                // Sin repositorios se importa igual, solo que sin instalar extensiones
                                Ok((backup, catalog::load().await.unwrap_or_default()))
                            }
                            .await;
                            let _ = tx.send(Msg::BackupRead(res));
                        });
                    }
                }
                None => {}
            },
            _ => {}
        }
    }

    fn key_reader(&mut self, key: KeyEvent) {
        let Some(r) = self.reader.as_mut() else { return };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                // Al volver, dejar seleccionado el capítulo que se estaba leyendo
                let chapter = r.chapter;
                self.select_chapter(chapter);
                self.reader = None;
                self.save_library();
                self.generation += 1;
                self.screen = Screen::Manga;
                self.clear = true;
            }
            // Con zoom o ajuste al ancho, las flechas primero recorren la página y al llegar al
            // borde pasan de página (como un webtoon)
            KeyCode::Char('r') if r.error.is_some() => self.retry_reader(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_or_turn(Axis::Y, 0.35, true),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_or_turn(Axis::Y, 0.35, false),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_or_turn(Axis::Y, 0.9, true),
            KeyCode::PageUp => self.scroll_or_turn(Axis::Y, 0.9, false),
            KeyCode::Right | KeyCode::Char('l') => self.scroll_or_turn(Axis::X, 0.5, true),
            KeyCode::Left | KeyCode::Char('h') => self.scroll_or_turn(Axis::X, 0.5, false),
            KeyCode::Char('f') => {
                let fit = r.view.fit.next();
                r.view = View { fit, ..View::default() };
                let key = r.prefs_key();
                self.prefs.set_fit(key, fit);
                self.view_changed();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom(1),
            KeyCode::Char('-') => self.zoom(-1),
            KeyCode::Char('0') => {
                r.view = View { fit: r.view.fit, ..View::default() };
                self.view_changed();
            }
            KeyCode::Char('n') => {
                if let Some(next) = r.next_chapter() {
                    self.open_chapter(next);
                }
            }
            KeyCode::Char('p') => {
                if let Some(prev) = r.prev_chapter() {
                    self.open_chapter(prev);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                r.page = 0;
                self.page_changed();
            }
            KeyCode::End | KeyCode::Char('G') => {
                r.page = r.pages.len().saturating_sub(1);
                self.page_changed();
            }
            _ => {}
        }
    }

    /// Desplaza la vista una fracción del área en `axis`; si ya está en el borde, cambia de página.
    fn scroll_or_turn(&mut self, axis: Axis, fraction: f32, forward: bool) {
        let area = (self.reader_area.width as u32 * self.font.0 as u32, self.reader_area.height as u32 * self.font.1 as u32);
        let Some(r) = self.reader.as_mut() else { return };
        let overflow = r.images.get(&r.page).map(|p| p.overflow()).unwrap_or_default();
        let (over, visible, pos) = match axis {
            Axis::X => (overflow.0, area.0, &mut r.view.x),
            Axis::Y => (overflow.1, area.1, &mut r.view.y),
        };
        let at_edge = if forward { *pos >= 1000 } else { *pos == 0 };
        if over > 0 && !at_edge {
            let step = ((visible as f32 * fraction / over as f32 * 1000.0) as u16).max(1);
            *pos = if forward { pos.saturating_add(step).min(1000) } else { pos.saturating_sub(step) };
            self.view_changed();
        } else if forward {
            if r.page + 1 < r.pages.len() {
                r.page += 1;
                self.page_changed();
            } else if let Some(next) = r.next_chapter() {
                self.open_chapter(next);
            }
        } else if r.page > 0 {
            r.page -= 1;
            self.page_changed();
            // Subiendo se llega al final de la página anterior
            if let (Axis::Y, Some(r)) = (axis, self.reader.as_mut()) {
                r.view.y = 1000;
                self.view_changed();
            }
        }
    }

    fn zoom(&mut self, delta: i32) {
        let Some(r) = self.reader.as_mut() else { return };
        let i = render::ZOOMS.iter().position(|&z| z >= r.view.zoom).unwrap_or(0) as i32;
        let zoom = render::ZOOMS[(i + delta).clamp(0, render::ZOOMS.len() as i32 - 1) as usize];
        if zoom != r.view.zoom {
            r.view.zoom = zoom;
            self.view_changed();
        }
    }

    /// Cambió la vista: preparar de nuevo las páginas descargadas.
    fn view_changed(&mut self) {
        self.dirty = true;
        let loaded: Vec<usize> = self.reader.as_ref().map(|r| r.images.keys().copied().collect()).unwrap_or_default();
        for index in loaded {
            self.request_encode(index);
        }
    }

    // ───────────────────────────── acciones ─────────────────────────────

    /// Pasa al siguiente filtro de idioma, lo guarda y repite la búsqueda si ya había una.
    fn cycle_search_langs(&mut self) {
        let options = prefs::lang_options(self.sources.iter().map(|s| s.info.lang.as_str()));
        let current = options.iter().position(|o| *o == self.prefs.search_langs);
        self.prefs.search_langs = options[current.map_or(0, |i| (i + 1) % options.len())].clone();
        self.prefs.save();
        let n = self.search_sources().len();
        self.status = trf!("buscar en: {} ({n} fuentes)", "search in: {} ({n} sources)", self.prefs.langs_label());
        if !self.query.is_empty() {
            self.start_search();
        }
    }

    /// Fuentes en las que buscar según el filtro de idioma.
    fn search_sources(&self) -> Vec<LoadedSource> {
        self.sources.iter().filter(|s| prefs::lang_matches(&self.prefs.search_langs, &s.info.lang)).cloned().collect()
    }

    fn start_search(&mut self) {
        self.groups.clear();
        self.groups_state.select(None);
        self.issues.clear();
        self.search_rest.clear();
        if self.host.is_none() {
            self.status = tr!("esperando a que carguen las extensiones…", "waiting for extensions to load…").into();
            return;
        }
        let sources = self.search_sources();
        if sources.is_empty() {
            self.status = trf!("ninguna fuente en {} (cambia con ctrl+L)", "no sources in {} (change it with ctrl+L)", self.prefs.langs_label());
            return;
        }
        self.health = Health::load();
        let (sources, skipped) = self.health.partition(sources, crate::library::now());
        self.issues.extend(skipped.into_iter().map(|source| {
            let detail = self.health.get(&source.info.id).and_then(|r| r.error.clone()).unwrap_or_default();
            SourceIssue { source, skipped: true, detail }
        }));
        if sources.is_empty() {
            self.status = tr!("todas las fuentes de este idioma están desactivadas: f para verlas", "all sources in this language are disabled: f to see them").into();
            return;
        }
        let favorites: HashSet<&str> = self.library.favorites.iter().map(|f| f.source_id.as_str()).collect();
        let (sources, rest) = self.health.first_batch(sources, &favorites);
        self.search_rest = rest;
        self.spawn_search(sources);
    }

    /// Consulta la siguiente tanda de fuentes, sin perder los resultados que ya hay.
    fn search_more(&mut self) {
        if self.search_rest.is_empty() {
            self.status = tr!("ya has buscado en todas las fuentes", "you've already searched every source").into();
        } else if self.pending > 0 {
            self.status = tr!("espera a que termine la búsqueda", "wait for the search to finish").into();
        } else {
            let next: Vec<_> = self.search_rest.drain(..health::BATCH_SIZE.min(self.search_rest.len())).collect();
            self.spawn_search(next);
        }
    }

    fn spawn_search(&mut self, sources: Vec<LoadedSource>) {
        let Some(host) = self.host.clone() else { return };
        self.search_gen += 1;
        self.pending = sources.len();
        let generation = self.search_gen;
        let mut rx = search::global(host, sources, self.query.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if tx.send(Msg::Search(generation, ev)).is_err() {
                    break;
                }
            }
        });
    }

    fn on_search_event(&mut self, ev: SearchEvent) {
        match ev {
            SearchEvent::Results { source, mangas } => {
                self.pending = self.pending.saturating_sub(1);
                for manga in mangas {
                    let key = search::normalize(&manga.title);
                    match self.groups.iter_mut().find(|g| g.key == key) {
                        Some(g) => g.entries.push((source.clone(), manga)),
                        None => self.groups.push(Group { key, title: manga.title.clone(), entries: vec![(source.clone(), manga)] }),
                    }
                }
                self.sort_groups();
            }
            SearchEvent::Failed { source, error } => {
                self.pending = self.pending.saturating_sub(1);
                self.issues.push(SourceIssue { source, skipped: false, detail: error });
            }
            SearchEvent::Done => {
                self.pending = 0;
                self.health = Health::load();
            }
        }
    }

    /// Coincidencia exacta primero, después las que empiezan/contienen la búsqueda,
    /// y a igualdad, las que están en más fuentes. Mantiene la selección en el mismo grupo.
    fn sort_groups(&mut self) {
        let selected = self.groups_state.selected().map(|i| self.groups[i].key.clone());
        let open = self.group.and_then(|i| self.groups.get(i)).map(|g| g.key.clone());
        let q = search::normalize(&self.query);
        let rank = |g: &Group| {
            let r = if g.key == q {
                0
            } else if g.key.starts_with(&q) {
                1
            } else if g.key.contains(&q) {
                2
            } else {
                3
            };
            (r, std::cmp::Reverse(g.entries.len()))
        };
        self.groups.sort_by_key(rank);
        // El grupo abierto en la pantalla de fuentes puede haber cambiado de posición
        if let Some(key) = open {
            self.group = self.groups.iter().position(|g| g.key == key);
        }
        let index = selected.and_then(|k| self.groups.iter().position(|g| g.key == k));
        self.groups_state.select(index.or((!self.groups.is_empty()).then_some(0)));
    }

    fn open_manga(&mut self, source: LoadedSource, manga: Manga, refresh: bool) {
        let Some(host) = self.host.clone() else { return };
        self.generation += 1;
        let generation = self.generation;
        self.details = None;
        self.details_error = None;
        self.chapters_state.select(None);
        self.status = trf!("cargando {} desde {}…", "loading {} from {}…", manga.title, source.info.name);
        self.current = Some((source.clone(), manga.clone()));
        self.screen = Screen::Manga;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = host.details(&source.info.id, &manga, refresh).await;
            let _ = tx.send(Msg::Details(generation, res));
        });
    }

    // ───────────────────────────── favoritos y AniList ─────────────────────────────

    fn current_favorite(&self) -> Option<&crate::library::Favorite> {
        let (src, manga) = self.current.as_ref()?;
        self.library.favorites.iter().find(|f| f.is(&src.info.id, &manga.url))
    }

    fn current_favorite_mut(&mut self) -> Option<&mut crate::library::Favorite> {
        let (src, manga) = self.current.as_ref()?;
        self.library.get_mut(&src.info.id, &manga.url)
    }

    fn save_library(&mut self) {
        if let Err(e) = self.library.save() {
            self.status = trf!("no se ha podido guardar la biblioteca: {e}", "couldn't save the library: {e}");
        }
        self.library_mtime = mtime(&paths::library_file());
    }

    /// Relee biblioteca y leídos si otro proceso (el servidor MCP, otra TUI) los ha cambiado.
    fn reload_if_changed(&mut self) {
        let (lib, reads) = (mtime(&paths::library_file()), mtime(&paths::read_file()));
        let mut changed = false;
        if lib != self.library_mtime {
            self.library = Library::load();
            self.library_mtime = lib;
            let len = self.library.favorites.len();
            self.library_state.select(if len == 0 { None } else { Some(self.library_state.selected().unwrap_or(0).min(len - 1)) });
            changed = true;
        }
        if reads != self.reads_mtime {
            self.reads = ReadLog::load();
            self.reads_mtime = reads;
            changed = true;
        }
        // En el lector no: redibujar borraría la imagen Sixel
        if changed && self.screen != Screen::Reader {
            self.dirty = true;
        }
    }

    fn toggle_favorite(&mut self) {
        let Some((src, manga)) = self.current.clone() else { return };
        // Guardar los datos completos si ya cargaron (portada, autor…), pero con la url original como clave
        let mut full = self.details.as_ref().map(|d| d.manga.clone()).unwrap_or_else(|| manga.clone());
        full.url = manga.url.clone();
        if full.title.is_empty() {
            full.title = manga.title.clone();
        }
        let added = self.library.toggle(&src.info.id, &src.info.name, &src.info.lang, &full);
        self.save_library();
        self.status = if added { tr!("★ añadido a favoritos", "★ added to favorites").into() } else { tr!("quitado de favoritos", "removed from favorites").into() };
        if added {
            self.library_state.select(Some(self.library.favorites.len() - 1));
            // Vincular con AniList si el título coincide exactamente (en segundo plano)
            let (tx, source_id, url, title) = (self.tx.clone(), src.info.id.clone(), full.url.clone(), full.title.clone());
            tokio::spawn(async move {
                let res = anilist::search(&title).await;
                let _ = tx.send(Msg::AutoLink(source_id, url, title, res));
            });
        }
    }

    fn on_auto_link(&mut self, source_id: &str, url: &str, title: &str, res: Result<Vec<Media>>) {
        let Ok(candidates) = res else { return };
        let Some(media) = anilist::best_match(title, &candidates) else {
            self.status = tr!("★ añadido a favoritos · para vincularlo con AniList pulsa 'a'", "★ added to favorites · press 'a' to link it to AniList").into();
            return;
        };
        let Some(fav) = self.library.get_mut(source_id, url).filter(|f| f.anilist.is_none()) else { return };
        fav.anilist = Some(AniListLink { media_id: media.id, title: media.display_title(), synced_progress: None, synced_status: None, synced_at: None });
        self.status = trf!("★ añadido a favoritos y vinculado con «{}» en AniList", "★ added to favorites and linked to “{}” on AniList", media.display_title());
        self.save_library();
        self.auto_sync(source_id, url);
    }

    /// Sincroniza el favorito actual con AniList. La primera vez (o con `relink`) pide elegir la obra.
    fn anilist_sync(&mut self, relink: bool) {
        if Credentials::load().is_none() {
            self.status = tr!("conecta tu cuenta de AniList (tecla l)", "connect your AniList account (key l)").into();
            self.screen = Screen::AniList;
            return;
        }
        let Some(fav) = self.current_favorite().cloned() else {
            self.status = tr!("primero añádelo a favoritos (f)", "add it to favorites first (f)").into();
            return;
        };
        match fav.anilist.filter(|_| !relink) {
            Some(link) => {
                let creds = Credentials::load().unwrap();
                self.status = trf!("sincronizando con AniList: {}…", "syncing with AniList: {}…", link.title);
                let tx = self.tx.clone();
                let (source_id, url) = (fav.source_id.clone(), fav.manga.url.clone());
                let (status, progress) = (fav.status.as_anilist(), fav.progress);
                tokio::spawn(async move {
                    let res = anilist::sync(&creds, link.media_id, status, progress).await;
                    let _ = tx.send(Msg::AnilistSynced(source_id, url, res));
                });
            }
            None => {
                self.anilist_matches = None;
                self.screen = Screen::AnilistPick;
                let tx = self.tx.clone();
                let title = fav.manga.title.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Msg::AnilistMatches(anilist::search(&title).await));
                });
            }
        }
    }

    fn on_synced(&mut self, source_id: &str, url: &str, res: Result<SyncResult>) {
        let Some(fav) = self.library.get_mut(source_id, url) else { return };
        match res {
            Ok(r) => {
                let progress = r.entry.progress.unwrap_or(fav.progress);
                let status = r.entry.status.as_deref().and_then(Status::from_anilist).unwrap_or(fav.status);
                // Si AniList tenía más capítulos leídos, adoptarlos localmente
                fav.progress = fav.progress.max(progress);
                fav.status = status;
                if let Some(link) = fav.anilist.as_mut() {
                    link.synced_progress = Some(fav.progress);
                    link.synced_status = Some(status);
                    link.synced_at = Some(crate::library::now());
                }
                let kept = if r.kept_remote_progress {
                    tr!(" (AniList ya tenía más progreso; se ha conservado)", " (AniList already had more progress; kept it)")
                } else {
                    ""
                };
                self.status = trf!("✓ AniList: {} · capítulo {progress}{kept}", "✓ AniList: {} · chapter {progress}{kept}", status.label());
            }
            Err(e) => self.status = format!("✗ AniList: {e}"),
        }
        self.save_library();
    }

    /// Guarda dónde quedó la lectura y marca el capítulo como leído al llegar a su última página.
    fn track_progress(&mut self) {
        let Some(r) = self.reader.as_ref() else { return };
        let chapter = r.chapters[r.chapter].clone();
        let (page, last_page) = (r.page, !r.pages.is_empty() && r.page + 1 >= r.pages.len());
        let (source_id, url) = (r.source.info.id.clone(), r.manga_url.clone());
        if last_page && self.reads.mark(&reads::key(&source_id, &url), &chapter) {
            self.save_reads();
        }
        let Some(fav) = self.library.get_mut(&source_id, &url) else { return };
        fav.last_read = Some(LastRead { chapter_url: chapter.url.clone(), chapter_name: chapter.name.clone(), page });
        if last_page {
            let before = fav.progress;
            fav.mark_chapter_read(&chapter);
            if fav.progress != before {
                self.save_library();
                self.auto_sync(&source_id, &url);
            }
        }
    }

    /// Arranca directamente en un manga (y capítulo): `hantho open`.
    fn start_with(&mut self, target: OpenTarget) {
        // Fuente provisional hasta que cargue el host, para poder mostrar la ficha ya
        let info = crate::model::SourceInfo {
            id: target.source_id.clone(),
            name: target.source_name.clone(),
            lang: target.lang.clone(),
            base_url: None,
            supports_latest: false,
        };
        let manga = Manga { url: target.manga_url.clone(), title: target.title.clone(), ..Default::default() };
        self.current = Some((LoadedSource { info }, manga));
        self.screen = Screen::Manga;
        self.manga_back = Screen::Library;
        self.status = trf!("abriendo {}…", "opening {}…", target.title);
        self.pending_chapter = (target.chapter != ChapterTarget::None).then(|| target.chapter.clone());
        self.pending_open = Some(target);
    }

    /// Abre el capítulo pedido una vez cargada la lista.
    fn open_chapter_target(&mut self, target: &ChapterTarget) {
        let Some(d) = self.details.as_ref() else { return };
        let key = self.current_key().unwrap_or_default();
        let last = self.current_favorite().and_then(|f| f.last_read.clone());
        let (index, page) = match target {
            ChapterTarget::None => return,
            ChapterTarget::Url(url) => (d.chapters.iter().position(|c| &c.url == url), 0),
            ChapterTarget::Number(n) => (
                d.chapters
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.chapter_number >= 0.0)
                    .min_by(|(_, a), (_, b)| (a.chapter_number - n).abs().total_cmp(&(b.chapter_number - n).abs()))
                    .map(|(i, _)| i),
                0,
            ),
            ChapterTarget::Continue => match last.and_then(|l| d.chapters.iter().position(|c| c.url == l.chapter_url).map(|i| (i, l.page))) {
                Some((i, page)) => (Some(i), page),
                // Sin lectura previa: el capítulo sin leer de número más bajo
                None => (
                    d.chapters
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| !self.reads.is_read(&key, &c.url))
                        .min_by(|(_, a), (_, b)| a.chapter_number.total_cmp(&b.chapter_number))
                        .map(|(i, _)| i),
                    0,
                ),
            },
        };
        match index {
            Some(i) => {
                self.select_chapter(i);
                self.open_chapter(i);
                if let Some(r) = self.reader.as_mut() {
                    r.page = page;
                }
            }
            None => self.status = tr!("no se ha encontrado ese capítulo", "chapter not found").into(),
        }
    }

    fn save_reads(&mut self) {
        if let Err(e) = self.reads.save() {
            self.status = trf!("no se ha podido guardar el registro de leídos: {e}", "couldn't save the read log: {e}");
        }
        self.reads_mtime = mtime(&paths::read_file());
    }

    /// Envía el progreso a AniList sin preguntar, si el favorito está vinculado y hay sesión.
    fn auto_sync(&mut self, source_id: &str, url: &str) {
        let (Some(creds), Some(fav)) = (Credentials::load(), self.library.favorites.iter().find(|f| f.is(source_id, url))) else {
            return;
        };
        let Some(link) = fav.anilist.clone().filter(|_| fav.needs_sync()) else { return };
        let (status, progress) = (fav.status.as_anilist(), fav.progress);
        let (tx, source_id, url) = (self.tx.clone(), source_id.to_string(), url.to_string());
        tokio::spawn(async move {
            let res = anilist::sync(&creds, link.media_id, status, progress).await;
            let _ = tx.send(Msg::AnilistSynced(source_id, url, res));
        });
    }

    // ───────────────────────────── lista de capítulos ─────────────────────────────

    fn current_key(&self) -> Option<String> {
        self.current.as_ref().map(|(src, m)| reads::key(&src.info.id, &m.url))
    }

    fn rebuild_chapter_view(&mut self) {
        let Some(d) = &self.details else {
            self.chapter_view.clear();
            return;
        };
        let filter = self.chapter_filter.trim().to_lowercase();
        let mut view: Vec<usize> = (0..d.chapters.len())
            .filter(|&i| {
                let c = &d.chapters[i];
                filter.is_empty() || c.name.to_lowercase().contains(&filter) || format_number(c.chapter_number) == filter
            })
            .collect();
        if self.chapters_reversed {
            view.reverse();
        }
        self.chapter_view = view;
    }

    fn selected_chapter(&self) -> Option<usize> {
        self.chapters_state.selected().and_then(|v| self.chapter_view.get(v).copied())
    }

    fn select_chapter(&mut self, index: usize) {
        if let Some(pos) = self.chapter_view.iter().position(|&i| i == index) {
            self.chapters_state.select(Some(pos));
        }
    }

    /// Salta al capítulo con ese número (o el más cercano), quitando el filtro si hace falta.
    fn goto_chapter_number(&mut self, text: &str) {
        let Ok(target) = text.trim().replace(',', ".").parse::<f32>() else {
            self.status = trf!("«{text}» no es un número de capítulo", "“{text}” isn't a chapter number");
            return;
        };
        let Some(d) = &self.details else { return };
        let best = d
            .chapters
            .iter()
            .enumerate()
            .filter(|(_, c)| c.chapter_number >= 0.0)
            .min_by(|(_, a), (_, b)| (a.chapter_number - target).abs().total_cmp(&(b.chapter_number - target).abs()))
            .map(|(i, c)| (i, c.name.clone()));
        match best {
            Some((i, name)) => {
                if !self.chapter_view.contains(&i) {
                    self.chapter_filter.clear();
                    self.rebuild_chapter_view();
                }
                self.select_chapter(i);
                self.status = format!("→ {name}");
            }
            None => self.status = tr!("los capítulos de esta fuente no tienen número", "this source's chapters have no numbers").into(),
        }
    }

    /// `m`: marca o desmarca el capítulo seleccionado.
    fn toggle_selected_read(&mut self) {
        let (Some(i), Some(key)) = (self.selected_chapter(), self.current_key()) else { return };
        let Some(chapter) = self.details.as_ref().map(|d| d.chapters[i].clone()) else { return };
        if self.reads.is_read(&key, &chapter.url) {
            self.reads.unmark(&key, &chapter.url);
            self.status = trf!("○ {} sin leer", "○ {} unread", chapter.name);
        } else {
            self.reads.mark(&key, &chapter);
            self.status = trf!("✓ {} leído", "✓ {} read", chapter.name);
        }
        self.save_reads();
        self.after_manual_marks(&key);
    }

    /// `M`: marca como leídos el seleccionado y todos los anteriores.
    fn mark_read_up_to_selected(&mut self) {
        let (Some(i), Some(key)) = (self.selected_chapter(), self.current_key()) else { return };
        let Some(d) = self.details.as_ref() else { return };
        let first = d.chapters.first().map_or(0.0, |c| c.chapter_number);
        let last = d.chapters.last().map_or(0.0, |c| c.chapter_number);
        // Las fuentes suelen listar del más nuevo al más viejo: los anteriores están "abajo"
        let older: Vec<usize> = if first >= last { (i..d.chapters.len()).collect() } else { (0..=i).collect() };
        let chapters: Vec<_> = older.iter().map(|&j| d.chapters[j].clone()).collect();
        let marked = chapters.iter().filter(|c| self.reads.mark(&key, c)).count();
        self.status = trf!("✓ {marked} capítulos marcados como leídos", "✓ {marked} chapters marked as read");
        self.save_reads();
        self.after_manual_marks(&key);
    }

    /// Tras marcar a mano: actualizar el progreso del favorito y AniList.
    fn after_manual_marks(&mut self, key: &str) {
        let Some((src, manga)) = self.current.clone() else { return };
        let max = self.reads.max_number(key);
        let Some(fav) = self.library.get_mut(&src.info.id, &manga.url) else { return };
        if fav.progress != max {
            fav.progress = max;
            self.save_library();
            self.auto_sync(&src.info.id, &manga.url);
        }
    }

    // ───────────────────────────── backups ─────────────────────────────

    fn export_backup(&mut self) {
        let path = backup::default_export_path();
        let data = backup::export(&self.library, &self.reads);
        let chapters: usize = data.manga.iter().map(|m| m.chapters.len()).sum();
        self.backup_report = match backup::write_file(&data, &path) {
            Ok(()) => vec![
                trf!("✓ exportados {} favoritos y {chapters} capítulos leídos", "✓ exported {} favorites and {chapters} read chapters", data.manga.len()),
                format!("  {}", path.display()),
                tr!("  en Mihon: Más → Copia de seguridad → Restaurar", "  in Mihon: More → Backup and restore → Restore").into(),
            ],
            Err(e) => vec![format!("✕ {e}")],
        };
    }

    fn on_backup_read(&mut self, res: Result<(Backup, Vec<catalog::Extension>)>) {
        let (data, catalog) = match res {
            Ok(v) => v,
            Err(e) => {
                self.backup_busy = false;
                self.backup_report = vec![format!("✕ {e}")];
                return;
            }
        };
        let installed: Vec<String> = self.installed.iter().map(|i| i.pkg.clone()).collect();
        let s = backup::import(&data, &catalog, &installed, &mut self.library, &mut self.reads);
        self.save_library();
        self.save_reads();
        self.backup_report = vec![
            trf!("✓ {} favoritos nuevos · {} ya estaban", "✓ {} new favorites · {} already there", s.added, s.merged),
            trf!("✓ {} capítulos marcados como leídos", "✓ {} chapters marked as read", s.chapters_read),
            trf!("✓ {} vinculados con AniList", "✓ {} linked to AniList", s.anilist_links),
        ];
        if !s.unknown_sources.is_empty() {
            self.backup_report.push(trf!("! sin extensión en tus repositorios: {}", "! no extension in your repositories: {}", s.unknown_sources.join(", ")));
        }
        let (Some(host), false) = (self.host.clone(), s.needed_extensions.is_empty()) else {
            self.backup_busy = false;
            self.status = tr!("importación terminada ✿", "import finished ✿").into();
            return;
        };
        let names: Vec<_> = s.needed_extensions.iter().map(|e| e.name.clone()).collect();
        self.backup_report.push(trf!("instalando {} extensiones: {}…", "installing {} extensions: {}…", names.len(), names.join(", ")));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let (mut ok, mut errors) = (Vec::new(), Vec::new());
            for ext in &s.needed_extensions {
                match crate::ext::install(ext).await {
                    Ok(i) => ok.push(i),
                    Err(e) => errors.push(format!("{}: {e}", ext.name)),
                }
            }
            let (sources, load_errors) = host.load_all(&ok).await;
            errors.extend(load_errors.into_iter().map(|(n, e)| format!("{n}: {}", search::short_error(&e))));
            let _ = tx.send(Msg::BackupExtensions(ok, sources, errors));
        });
    }

    fn open_chapter(&mut self, index: usize) {
        let (Some(host), Some((source, manga)), Some(details)) = (self.host.clone(), self.current.clone(), self.details.as_ref())
        else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let chapter = details.chapters[index].clone();
        let fit = self.prefs.fit_for(&reads::key(&source.info.id, &manga.url));
        self.reader = Some(Reader {
            source: source.clone(),
            manga_url: manga.url.clone(),
            manga: details.manga.title.is_empty().then_some(manga).unwrap_or_else(|| details.manga.clone()),
            chapters: details.chapters.clone(),
            chapter: index,
            pages: Vec::new(),
            page: 0,
            requested: HashSet::new(),
            images: HashMap::new(),
            error: None,
            view: View { fit, ..View::default() },
        });
        self.screen = Screen::Reader;
        self.clear = true;
        self.track_progress();
        self.save_library();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = host.pages(&source.info.id, &chapter).await;
            let _ = tx.send(Msg::Pages(generation, res));
        });
    }

    /// Vuelve a pedir lo que ha fallado: la página actual o, si ni siquiera hay lista, el capítulo.
    fn retry_reader(&mut self) {
        let Some(r) = self.reader.as_mut() else { return };
        if r.pages.is_empty() {
            let (chapter, page) = (r.chapter, r.page);
            self.open_chapter(chapter);
            if let Some(r) = self.reader.as_mut() {
                r.page = page;
            }
        } else {
            r.error = None;
            let page = r.page;
            r.requested.remove(&page);
            self.prefetch();
        }
    }

    fn page_changed(&mut self) {
        self.clear = true;
        if let Some(r) = self.reader.as_mut() {
            r.error = None;
            r.view = r.view.at_start();
            let page = r.page;
            r.images.retain(|&i, _| i.abs_diff(page) <= KEEP_AROUND);
            r.requested.retain(|&i| i.abs_diff(page) <= KEEP_AROUND);
        }
        self.track_progress();
        self.prefetch();
    }

    /// Pide la página actual y las siguientes que falten.
    fn prefetch(&mut self) {
        let (Some(host), Some(r)) = (self.host.clone(), self.reader.as_mut()) else { return };
        let end = (r.page + 1 + PREFETCH).min(r.pages.len());
        for index in r.page..end {
            if !r.requested.insert(index) {
                continue;
            }
            let page = r.pages[index].clone();
            let source_id = r.source.info.id.clone();
            let path = page_cache_path(&r.source.info.id, &r.chapters[r.chapter].url, index);
            let (tx, host, generation) = (self.tx.clone(), host.clone(), self.generation);
            tokio::spawn(async move {
                let res = async {
                    if !path.exists() {
                        let tmp = path.with_extension("part");
                        host.image(&source_id, &page, &tmp).await?;
                        std::fs::rename(&tmp, &path)?;
                    }
                    let img = tokio::task::spawn_blocking(move || -> Result<DynamicImage> {
                        Ok(image::ImageReader::open(&path)?.with_guessed_format()?.decode()?)
                    })
                    .await??;
                    Ok(img)
                }
                .await;
                let _ = tx.send(Msg::Image(generation, index, res));
            });
        }
    }

    /// Codifica (en segundo plano) la página para el tamaño actual del área del lector.
    fn request_encode(&mut self, index: usize) {
        let size = (self.reader_area.width, self.reader_area.height);
        let (true, Some(r)) = (self.sixel && size.0 > 0 && size.1 > 0, self.reader.as_mut()) else { return };
        let key = (size, r.view_for(index));
        let Some(page) = r.images.get_mut(&index) else { return };
        if page.encoding == Some(key) || page.encoded.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        page.encoding = Some(key);
        let (img, font, tx, generation) = (page.img.clone(), self.font, self.tx.clone(), self.generation);
        tokio::task::spawn_blocking(move || {
            let enc = render::encode(&img, size.0, size.1, font, key.1);
            let _ = tx.send(Msg::Encoded(generation, index, key, enc));
        });
    }

    /// Escribe el Sixel de la página actual si todavía no está en pantalla. Va por fuera de
    /// ratatui: ratatui no dibuja nada en esa zona, así que la imagen queda intacta.
    fn flush_sixel(&mut self) -> Result<()> {
        use ratatui::crossterm::{cursor::MoveTo, queue};
        use std::io::Write;
        if !self.sixel || self.screen != Screen::Reader {
            return Ok(());
        }
        let area = self.reader_area;
        let size = (area.width, area.height);
        let Some(r) = self.reader.as_ref() else { return Ok(()) };
        // Mientras se prepara otra vista se deja la anterior (sin parpadeo al desplazarse)
        let Some(((_, view), enc)) = r.images.get(&r.page).and_then(|p| p.encoded.as_ref()).filter(|((s, _), _)| *s == size) else {
            return Ok(());
        };
        let x = area.x + area.width.saturating_sub(enc.cols) / 2;
        let y = area.y + area.height.saturating_sub(enc.rows) / 2;
        let target = Rect::new(x, y, enc.cols, enc.rows);
        if self.sixel_drawn == Some((r.page, target, *view)) {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        // Si la imagen nueva ocupa otro rectángulo, borrar lo que quede de la anterior
        if let Some((_, old, _)) = self.sixel_drawn.filter(|(_, old, _)| *old != target) {
            let blank = " ".repeat(old.width as usize);
            for row in old.y..old.y + old.height {
                queue!(out, MoveTo(old.x, row))?;
                out.write_all(blank.as_bytes())?;
            }
        }
        queue!(out, MoveTo(target.x, target.y))?;
        out.write_all(enc.sixel.as_bytes())?;
        out.flush()?;
        self.sixel_drawn = Some((r.page, target, *view));
        Ok(())
    }

    // ───────────────────────────── dibujo ─────────────────────────────

    fn draw(&mut self, f: &mut Frame) {
        match self.screen {
            Screen::Home => self.draw_home(f),
            Screen::Library => self.draw_library(f),
            Screen::Search => self.draw_search(f),
            Screen::Sources => self.draw_sources(f),
            Screen::SourceIssues => self.draw_source_issues(f),
            Screen::Manga => self.draw_manga(f),
            Screen::Reader => self.draw_reader(f),
            Screen::AnilistPick => self.draw_anilist_pick(f),
            Screen::Extensions => self.draw_extensions(f),
            Screen::AddExtensions => self.draw_add_extensions(f),
            Screen::Backup => self.draw_backup(f),
            Screen::AniList => self.draw_anilist(f),
        }
    }

    fn menu_items(&self) -> Vec<MenuItem> {
        let favs = self.library.favorites.len();
        let pending = self.library.favorites.iter().filter(|f| f.needs_sync()).count();
        let sources = if !self.sources.is_empty() { trf!("{} fuentes", "{} sources", self.sources.len()) } else { tr!("cargando…", "loading…").into() };
        let anilist = match &self.anilist_user {
            Some(name) if pending > 0 => trf!("@{name} · {pending} sin sincronizar", "@{name} · {pending} not synced"),
            Some(name) => format!("@{name}"),
            None => tr!("sin sesión", "not logged in").into(),
        };
        vec![
            MenuItem { icon: "♡", label: tr!("Biblioteca", "Library"), detail: if favs == 1 { tr!("1 favorito", "1 favorite").into() } else { trf!("{favs} favoritos", "{favs} favorites") } },
            MenuItem { icon: "⌕", label: tr!("Buscar", "Search"), detail: sources },
            MenuItem {
                icon: "✦",
                label: tr!("Extensiones", "Extensions"),
                detail: match self.updates.len() {
                    0 => trf!("{} instaladas", "{} installed", self.installed.len()),
                    n => trf!("{} instaladas · ↑ {n} nuevas", "{} installed · ↑ {n} new", self.installed.len()),
                },
            },
            MenuItem {
                icon: "✚",
                label: tr!("Añadir extensiones", "Add extensions"),
                detail: match self.add.repos.len() {
                    0 => tr!("sin repositorios", "no repositories").into(),
                    1 => tr!("1 repositorio", "1 repository").into(),
                    n => trf!("{n} repositorios", "{n} repositories"),
                },
            },
            MenuItem { icon: "☁", label: "AniList", detail: anilist },
            MenuItem { icon: "⇄", label: tr!("Backup Mihon", "Mihon backup"), detail: tr!("importar / exportar", "import / export").into() },
            MenuItem { icon: "✕", label: tr!("Salir", "Quit"), detail: String::new() },
        ]
    }

    fn draw_home(&mut self, f: &mut Frame) {
        let items = self.menu_items();
        let footer = if self.host.is_none() && self.sources.is_empty() {
            Line::from(vec![
                Span::raw(theme::spinner(self.tick)).fg(PINK),
                Span::raw(tr!(" cargando extensiones…", " loading extensions…")).fg(MUTED),
            ])
        } else {
            Line::from(vec![
                Span::raw("↑↓").fg(PINK).bold(),
                Span::raw(tr!(" elegir  ", " choose  ")).fg(MUTED),
                Span::raw("enter").fg(PINK).bold(),
                Span::raw(tr!(" abrir  ", " open  ")).fg(MUTED),
                Span::raw("1-7").fg(PINK).bold(),
                Span::raw(tr!(" atajos  ", " shortcuts  ")).fg(MUTED),
                Span::raw("q").fg(PINK).bold(),
                Span::raw(tr!(" salir", " quit")).fg(MUTED),
            ])
        };
        self.home.draw(f, &items, footer);
    }

    fn draw_library(&mut self, f: &mut Frame) {
        let [main, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let title = trf!("♡ biblioteca · {}", "♡ library · {}", self.library.favorites.len());
        if self.library.favorites.is_empty() {
            let msg = vec![
                Line::raw(""),
                Line::from(tr!("todavía no tienes favoritos", "you don't have any favorites yet")).fg(TEXT).centered(),
                Line::raw(""),
                Line::from(vec![
                    Span::raw(tr!("busca un manga con ", "search for a manga with ")).fg(MUTED),
                    Span::raw("/").fg(PINK).bold(),
                    Span::raw(tr!(", ábrelo y pulsa ", ", open it and press ")).fg(MUTED),
                    Span::raw("f").fg(PINK).bold(),
                    Span::raw(tr!(" para guardarlo aquí ✿", " to keep it here ✿")).fg(MUTED),
                ])
                .centered(),
            ];
            f.render_widget(Paragraph::new(msg).block(theme::block_focused(&title)), main);
        } else {
            let [list_area, card_area] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(main);
            let items: Vec<ListItem> = self
                .library
                .favorites
                .iter()
                .map(|fav| {
                    let sync = match &fav.anilist {
                        None => Span::raw("  "),
                        Some(_) if fav.needs_sync() => Span::raw(" ↻").fg(PEACH),
                        Some(_) => Span::raw(" ✓").fg(MINT),
                    };
                    let mut spans = vec![
                        Span::raw(fav.manga.title.clone()).fg(TEXT),
                        Span::raw(trf!("  cap. {}", "  ch. {}", fav.progress)).fg(MUTED),
                        sync,
                    ];
                    let new = self.news.count(fav, &self.reads);
                    if new > 0 {
                        spans.push(Span::raw(format!("  +{new}")).fg(PEACH).bold());
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            f.render_stateful_widget(
                theme::highlight(List::new(items).block(theme::block_focused(&title))),
                list_area,
                &mut self.library_state,
            );
            if let Some(fav) = self.library_state.selected().and_then(|i| self.library.favorites.get(i)) {
                f.render_widget(favorite_card(fav), card_area);
            }
        }
        theme::status_bar(f, status_area, tr!("BIBLIOTECA", "LIBRARY"), &self.status, &[("enter", tr!("abrir", "open")), ("u", tr!("capítulos nuevos", "new chapters")), ("d", tr!("quitar", "remove")), ("/", tr!("buscar", "search")), ("esc", tr!("menú", "menu"))]);
    }

    fn draw_extensions(&mut self, f: &mut Frame) {
        let [main, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let items: Vec<ListItem> = self
            .installed
            .iter()
            .map(|e| {
                let (label, color) = match e.content_warning {
                    3 => ("nsfw", ERROR),
                    2 => (tr!("mixto", "mixed"), PEACH),
                    _ => (tr!("apto", "safe"), MINT),
                };
                // Una extensión puede tener varias fuentes con otros nombres: lo que cuenta es si ha fallado
                let error = self.load_errors.iter().find(|(name, _)| *name == e.name).map(|(_, error)| error);
                let loaded = self.host.is_some() && error.is_none();
                let mut spans = vec![
                    Span::raw(if loaded { "● " } else { "○ " }).fg(if loaded { MINT } else { FAINT }),
                    Span::raw(format!("{:<34}", e.name)).fg(TEXT),
                    Span::raw(format!("{:<9}", e.version_name)).fg(MUTED),
                    Span::raw(format!("{:<8}", e.langs.join(","))).fg(LAVENDER),
                    Span::raw(label).fg(color),
                ];
                if let Some(new) = self.updates.iter().find(|u| u.package_name == e.pkg) {
                    spans.push(Span::raw(format!("  ↑ {}", new.version_name)).fg(PEACH));
                }
                if let Some(error) = error {
                    spans.push(Span::raw(format!("  ✕ {}", search::short_error(error))).fg(ERROR));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let mut title = trf!("✦ extensiones · {} instaladas · {} fuentes cargadas", "✦ extensions · {} installed · {} sources loaded", self.installed.len(), self.sources.len());
        if !self.updates.is_empty() {
            title += &trf!(" · ↑ {} con versión nueva", " · ↑ {} with a new version", self.updates.len());
        }
        f.render_stateful_widget(theme::highlight(List::new(items).block(theme::block_focused(&title))), main, &mut self.extensions_state);
        theme::status_bar(
            f,
            status_area,
            tr!("EXTENSIONES", "EXTENSIONS"),
            &if self.installed.is_empty() {
                tr!("todavía no tienes ninguna: pulsa a para añadir", "you don't have any yet: press a to add some").to_string()
            } else if self.upgrading {
                format!("{} {}", theme::spinner(self.tick), self.status)
            } else if !self.updates.is_empty() {
                trf!("U para actualizar las {}", "U to update all {}", self.updates.len())
            } else {
                tr!("todas al día · a para añadir más", "all up to date · a to add more").to_string()
            },
            &[("↑↓", tr!("mover", "move")), ("U", tr!("actualizar", "update")), ("a", tr!("añadir", "add")), ("esc", tr!("menú", "menu"))],
        );
    }

    fn draw_add_extensions(&mut self, f: &mut Frame) {
        let [input_area, list_area, status_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let tick = self.tick;
        let add = &mut self.add;
        let view = add.view();

        let cursor = if add.editing && tick % 16 < 9 { "▏" } else { " " };
        let mut filter = vec![
            if add.filter.is_empty() && !add.editing {
                Span::raw(tr!("/ para filtrar por nombre", "/ to filter by name")).fg(FAINT).italic()
            } else {
                Span::raw(add.filter.clone()).fg(TEXT)
            },
            Span::raw(cursor).fg(PINK),
            Span::raw(tr!("   idioma: ", "   language: ")).fg(MUTED),
            Span::raw(add.lang.clone().unwrap_or_else(|| tr!("todos", "all").into())).fg(LAVENDER),
        ];
        if add.hide_nsfw {
            filter.push(Span::raw(tr!("  · sin nsfw", "  · no nsfw")).fg(MUTED));
        }
        let input_block = if add.editing { theme::block_focused(tr!("⌕ filtrar", "⌕ filter")) } else { theme::block(tr!("⌕ filtrar", "⌕ filter")) };
        f.render_widget(Paragraph::new(Line::from(filter)).block(input_block), input_area);

        let catalog = add.catalog.as_deref().unwrap_or_default();
        let items: Vec<ListItem> = view
            .iter()
            .map(|&i| {
                let e = &catalog[i];
                let installed = self.installed.iter().find(|x| x.pkg == e.package_name);
                let (mark, color) = if add.busy.contains(&e.package_name) {
                    (format!("{} ", theme::spinner(tick)), PINK)
                } else if installed.is_some_and(|x| x.version_code < e.version_code) {
                    ("↑ ".to_string(), PEACH)
                } else if installed.is_some() {
                    ("✓ ".to_string(), MINT)
                } else {
                    ("  ".to_string(), FAINT)
                };
                let (label, warn) = match e.content_warning {
                    3 => ("nsfw", ERROR),
                    2 => (tr!("mixto", "mixed"), PEACH),
                    _ => (tr!("apto", "safe"), MINT),
                };
                let langs: String = e.langs().join(",").chars().take(9).collect();
                ListItem::new(Line::from(vec![
                    Span::raw(mark).fg(color),
                    Span::raw(format!("{:<34}", e.name)).fg(if installed.is_some() { TEXT } else { MUTED }),
                    Span::raw(format!("{:<9}", e.version_name)).fg(FAINT),
                    Span::raw(format!("{langs:<10}")).fg(LAVENDER),
                    Span::raw(label).fg(warn),
                ]))
            })
            .collect();
        let title = match &add.catalog {
            Some(cat) => {
                let repos = if add.repos.len() == 1 { tr!("1 repositorio", "1 repository").to_string() } else { trf!("{} repositorios", "{} repositories", add.repos.len()) };
                trf!("✚ añadir extensiones · {} de {} · {repos}", "✚ add extensions · {} of {} · {repos}", view.len(), cat.len())
            }
            None => tr!("✚ añadir extensiones", "✚ add extensions").into(),
        };
        f.render_stateful_widget(theme::highlight(List::new(items).block(theme::block_focused(&title))), list_area, &mut add.state);
        if add.loading && add.catalog.is_none() && add.repo_input.is_none() {
            let [center] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(list_area);
            let msg = Line::from(vec![Span::raw(theme::spinner(tick)).fg(PINK), Span::raw(tr!(" cargando el catálogo…", " loading the catalog…")).fg(MUTED)]);
            f.render_widget(Paragraph::new(msg).centered(), center);
        }

        if let Some(input) = &add.repo_input {
            let [card] = Layout::horizontal([Constraint::Length(84.min(list_area.width))]).flex(Flex::Center).areas(list_area);
            let [card] = Layout::vertical([Constraint::Length(12.min(card.height))]).flex(Flex::Center).areas(card);
            let cursor = if tick % 16 < 9 { "▏" } else { " " };
            // Solo el final de la URL si es más larga que el cuadro
            let shown: String = {
                let chars: Vec<char> = input.chars().collect();
                chars[chars.len().saturating_sub(70)..].iter().collect()
            };
            let mut lines = vec![
                Line::raw(""),
                Line::from(tr!("  Han Thỏ no trae extensiones: añade el repositorio que quieras usar.", "  Han Thỏ ships no extensions: add the repository you want to use.")).fg(TEXT),
                Line::raw(""),
                Line::from(tr!("  Vale la URL de su index.pb o index.min.json, la del repo en GitHub o un", "  Paste the URL of its index.pb or index.min.json, the GitHub repo or a")).fg(MUTED),
                Line::from(tr!("  enlace tachiyomi://add-repo. Tiene que publicar las extensiones en .jar", "  tachiyomi://add-repo link. It has to publish extensions as .jar")).fg(MUTED),
                Line::from(tr!("  (formato Keiyoushi): las que solo traen .apk no se pueden cargar.", "  (Keiyoushi format): .apk-only repositories can't be loaded.")).fg(MUTED),
                Line::raw(""),
                Line::from(vec![Span::raw("  URL: ").fg(MUTED), Span::raw(shown).fg(PINK).bold(), Span::raw(cursor).fg(PINK)]),
            ];
            if add.loading {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![Span::raw(format!("  {} ", theme::spinner(tick))).fg(PINK), Span::raw(tr!("comprobando…", "checking…")).fg(MUTED)]));
            } else if !add.repos.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::from(trf!("  ya tienes {} repositorios añadidos", "  you already have {} repositories", add.repos.len())).fg(FAINT));
            }
            f.render_widget(Clear, card);
            f.render_widget(Paragraph::new(lines).block(theme::block_focused(tr!("✚ añadir repositorio", "✚ add repository"))), card);
            theme::status_bar(f, status_area, tr!("EXTENSIONES", "EXTENSIONS"), &self.status, &[("enter", tr!("añadir", "add")), ("esc", tr!("cancelar", "cancel"))]);
            return;
        }
        let keys: &[(&str, &str)] = if add.editing {
            &[("enter", tr!("listo", "done")), ("esc", tr!("listo", "done"))]
        } else {
            &[("enter", tr!("instalar/quitar", "install/remove")), ("/", tr!("filtrar", "filter")), ("l", tr!("idioma", "language")), ("n", "nsfw"), ("r", "repo"), ("esc", tr!("menú", "menu"))]
        };
        theme::status_bar(f, status_area, tr!("EXTENSIONES", "EXTENSIONS"), &self.status, keys);
    }

    fn draw_anilist(&mut self, f: &mut Frame) {
        let [main, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let [card] = Layout::horizontal([Constraint::Length(72.min(main.width))]).flex(Flex::Center).areas(main);
        let [card] = Layout::vertical([Constraint::Length(16.min(card.height))]).flex(Flex::Center).areas(card);
        let linked = self.library.favorites.iter().filter(|f| f.anilist.is_some()).count();
        let pending = self.library.favorites.iter().filter(|f| f.needs_sync()).count();
        let mut lines = vec![Line::raw("")];
        match &self.anilist_user {
            Some(name) => {
                lines.push(Line::from(vec![Span::raw(tr!("  conectado como ", "  logged in as ")).fg(MUTED), Span::raw(format!("@{name}")).fg(PINK).bold()]));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![Span::raw("  ✓ ").fg(MINT), Span::raw(trf!("{linked} favoritos vinculados", "{linked} favorites linked")).fg(TEXT)]));
                let pending_line = if pending > 0 {
                    vec![Span::raw("  ↻ ").fg(PEACH), Span::raw(trf!("{pending} con cambios sin sincronizar", "{pending} with unsynced changes")).fg(TEXT)]
                } else {
                    vec![Span::raw("  ✓ ").fg(MINT), Span::raw(tr!("todo sincronizado", "everything synced")).fg(TEXT)]
                };
                lines.push(Line::from(pending_line));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw(tr!("  para sincronizar, abre un favorito y pulsa ", "  to sync, open a favorite and press ")).fg(MUTED),
                    Span::raw("a").fg(PINK).bold(),
                ]));
            }
            None if self.anilist_login => {
                lines.push(Line::from(vec![
                    Span::raw(format!("  {} ", theme::spinner(self.tick))).fg(PINK),
                    Span::raw(tr!("esperando que apruebes en el navegador…", "waiting for you to approve in the browser…")).fg(TEXT),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::from(tr!("  en la página de AniList pulsa «Approve»;", "  on the AniList page press “Approve”;")).fg(MUTED));
                lines.push(Line::from(tr!("  Han Thỏ recibe la sesión solo, no hace falta copiar nada ✿", "  Han Thỏ gets the session by itself, nothing to copy ✿")).fg(MUTED));
            }
            None if anilist_auth::client_id().is_some() => {
                lines.push(Line::from(tr!("  sin sesión", "  not logged in")).fg(TEXT));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw(tr!("  pulsa ", "  press ")).fg(MUTED),
                    Span::raw("l").fg(PINK).bold(),
                    Span::raw(tr!(" para conectar tu cuenta: se abre el navegador,", " to connect your account: the browser opens,")).fg(MUTED),
                ]));
                lines.push(Line::from(tr!("  apruebas el acceso y listo.", "  you approve access and that's it.")).fg(MUTED));
            }
            None => {
                let cursor = if self.tick % 16 < 9 { "▏" } else { " " };
                lines.push(Line::from(tr!("  configuración única: registrar Han Thỏ en AniList", "  one-time setup: register Han Thỏ on AniList")).fg(TEXT));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw(tr!("  1. pulsa ", "  1. press ")).fg(MUTED),
                    Span::raw("d").fg(PINK).bold(),
                    Span::raw(tr!(" (abre anilist.co/settings/developer) → Create New Client", " (opens anilist.co/settings/developer) → Create New Client")).fg(MUTED),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  2. Name: ").fg(MUTED),
                    Span::raw("Han Thỏ").fg(TEXT),
                    Span::raw("  ·  Redirect URL: ").fg(MUTED),
                    Span::raw(anilist_auth::REDIRECT).fg(LAVENDER),
                ]));
                lines.push(Line::from(tr!("  3. pega aquí el ID del client y pulsa enter:", "  3. paste the client ID here and press enter:")).fg(MUTED));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw("     ID: ").fg(MUTED),
                    Span::raw(self.client_input.clone()).fg(PINK).bold(),
                    Span::raw(cursor).fg(PINK),
                ]));
            }
        }
        f.render_widget(Clear, card);
        f.render_widget(Paragraph::new(lines).block(theme::block_focused("☁ anilist")), card);
        let keys: &[(&str, &str)] = match (&self.anilist_user, anilist_auth::client_id().is_some()) {
            _ if self.anilist_login => &[("esc", tr!("menú", "menu"))],
            (Some(_), _) => &[("l", tr!("reconectar", "reconnect")), ("o", tr!("cerrar sesión", "log out")), ("esc", tr!("menú", "menu"))],
            (None, true) => &[("l", tr!("conectar", "connect")), ("esc", tr!("menú", "menu"))],
            (None, false) => &[("d", tr!("abrir AniList", "open AniList")), ("enter", tr!("guardar y conectar", "save and connect")), ("esc", tr!("menú", "menu"))],
        };
        theme::status_bar(f, status_area, "ANILIST", &self.status, keys);
    }

    fn draw_backup(&mut self, f: &mut Frame) {
        let [main, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let [list_area, report_area] = Layout::vertical([Constraint::Min(6), Constraint::Length(8)]).areas(main);
        let mut items = vec![ListItem::new(Line::from(vec![
            Span::raw("⇪ ").fg(MINT),
            Span::raw(tr!("Exportar biblioteca", "Export library")).fg(TEXT).bold(),
            Span::raw(trf!("  {} favoritos → Descargas (.tachibk)", "  {} favorites → Downloads (.tachibk)", self.library.favorites.len())).fg(MUTED),
        ]))];
        for path in &self.backup_files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
            items.push(ListItem::new(Line::from(vec![
                Span::raw("⇩ ").fg(LAVENDER),
                Span::raw(tr!("Importar ", "Import ")).fg(MUTED),
                Span::raw(name).fg(TEXT).bold(),
                Span::raw(format!("  {dir}")).fg(FAINT),
            ])));
        }
        f.render_stateful_widget(
            theme::highlight(List::new(items).block(theme::block_focused(tr!("⇄ backup de Mihon", "⇄ Mihon backup")))),
            list_area,
            &mut self.backup_state,
        );

        let mut lines: Vec<Line> = if self.backup_report.is_empty() {
            let hint = if self.backup_files.is_empty() {
                tr!("no se han encontrado backups: en Mihon, Más → Copia de seguridad → Crear, y deja el .tachibk en Descargas o Documentos (r para buscar de nuevo)", "no backups found: in Mihon, More → Backup and restore → Create backup, and put the .tachibk in Downloads or Documents (r to look again)")
            } else {
                tr!("importar añade favoritos, capítulos leídos y vínculos con AniList; instala solas las extensiones que falten", "importing adds favorites, read chapters and AniList links; missing extensions get installed automatically")
            };
            vec![Line::from(hint).fg(MUTED)]
        } else {
            self.backup_report
                .iter()
                .map(|l| {
                    let color = if l.starts_with('✕') { ERROR } else if l.starts_with('!') { PEACH } else if l.starts_with('✓') { MINT } else { MUTED };
                    Line::from(l.clone()).fg(color)
                })
                .collect()
        };
        if self.backup_busy {
            lines.push(Line::from(vec![Span::raw(theme::spinner(self.tick)).fg(PINK), Span::raw(tr!(" trabajando…", " working…")).fg(MUTED)]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(theme::block(tr!("resultado", "result"))), report_area);
        theme::status_bar(f, status_area, "BACKUP", &self.status, &[("enter", tr!("elegir", "choose")), ("r", tr!("buscar archivos", "find files")), ("esc", tr!("menú", "menu"))]);
    }

    fn draw_anilist_pick(&mut self, f: &mut Frame) {
        let [list_area, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let manga = self.current.as_ref().map(|(_, m)| m.title.clone()).unwrap_or_default();
        let title = trf!("☁ ¿cuál es «{manga}» en AniList?", "☁ which one is “{manga}” on AniList?");
        match &self.anilist_matches {
            None => f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(theme::spinner(self.tick)).fg(PINK),
                    Span::raw(tr!(" buscando en AniList…", " searching AniList…")).fg(MUTED),
                ]))
                .block(theme::block_focused(&title)),
                list_area,
            ),
            Some(list) => {
                let items: Vec<ListItem> = list
                    .iter()
                    .map(|m| {
                        let year = m.start_date.as_ref().and_then(|d| d.year).map(|y| y.to_string()).unwrap_or_default();
                        let chapters = m.chapters.map(|c| trf!(" · {c} caps", " · {c} ch.")).unwrap_or_default();
                        let english = m.title.english.clone().filter(|e| Some(e) != m.title.romaji.as_ref());
                        ListItem::new(Line::from(vec![
                            Span::raw(m.display_title()).fg(TEXT).bold(),
                            Span::raw(english.map(|e| format!("  ({e})")).unwrap_or_default()).fg(LAVENDER),
                            Span::raw(format!("  {} {year}{chapters}", m.format.clone().unwrap_or_default())).fg(MUTED),
                        ]))
                    })
                    .collect();
                f.render_stateful_widget(
                    theme::highlight(List::new(items).block(theme::block_focused(&title))),
                    list_area,
                    &mut self.anilist_state,
                );
            }
        }
        theme::status_bar(f, status_area, "ANILIST", tr!("elige la obra que quieres vincular", "pick the entry you want to link"), &[("enter", tr!("vincular y sincronizar", "link and sync")), ("esc", tr!("cancelar", "cancel"))]);
    }

    fn draw_search(&mut self, f: &mut Frame) {
        let [input_area, list_area, status_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

        let input = if self.input.is_empty() && self.editing {
            Line::from(vec![Span::raw("▏").fg(PINK), Span::raw(tr!("¿qué te apetece leer hoy?", "what do you feel like reading today?")).fg(FAINT).italic()])
        } else {
            let cursor = if self.editing && self.tick % 16 < 9 { "▏" } else { " " };
            Line::from(vec![Span::raw(self.input.clone()).fg(TEXT), Span::raw(cursor).fg(PINK)])
        };
        let search_title = trf!("⌕ buscar · {}", "⌕ search · {}", self.prefs.langs_label());
        let input_block = if self.editing { theme::block_focused(&search_title) } else { theme::block(&search_title) };
        f.render_widget(Paragraph::new(input).block(input_block), input_area);

        let items: Vec<ListItem> = self
            .groups
            .iter()
            .map(|g| {
                let names: Vec<&str> = g.entries.iter().map(|(s, _)| s.info.name.as_str()).take(3).collect();
                let more = g.entries.len().saturating_sub(3);
                let mut spans = vec![
                    Span::raw(g.title.clone()).fg(TEXT).bold(),
                    Span::raw("  "),
                    Span::raw(names.join(" · ")).fg(MUTED),
                ];
                if more > 0 {
                    spans.push(Span::raw(format!(" +{more}")).fg(LAVENDER));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = if self.query.is_empty() {
            tr!("resultados", "results").to_string()
        } else {
            trf!("«{}» · {} títulos", "“{}” · {} titles", self.query, self.groups.len())
        };
        let list_block = if self.editing { theme::block(&title) } else { theme::block_focused(&title) };
        f.render_stateful_widget(theme::highlight(List::new(items).block(list_block)), list_area, &mut self.groups_state);

        let mut status = self.status.clone();
        if self.pending > 0 {
            status = trf!("{} buscando… faltan {} fuentes", "{} searching… {} sources to go", theme::spinner(self.tick), self.pending);
        } else if !self.query.is_empty() && !self.issues.is_empty() {
            let skipped = self.issues.iter().filter(|i| i.skipped).count();
            let failed = self.issues.len() - skipped;
            if failed > 0 {
                status = trf!("{status} · {failed} han fallado", "{status} · {failed} failed");
            }
            if skipped > 0 {
                status = trf!("{status} · {skipped} omitidas", "{status} · {skipped} skipped");
            }
        }
        if self.pending == 0 && !self.search_rest.is_empty() {
            let rest = self.search_rest.len();
            status = trf!("{status} · quedan {rest} fuentes: m para buscar en más", "{status} · {rest} sources left: m to search more");
        }
        let mut keys = if self.editing {
            vec![("enter", tr!("buscar", "search")), ("^L", tr!("idioma", "language")), ("↓", tr!("resultados", "results")), ("esc", tr!("menú", "menu"))]
        } else {
            vec![("enter", tr!("abrir", "open")), ("/", tr!("editar", "edit")), ("f", tr!("fallos", "failures")), ("l", tr!("idioma", "language")), ("b", tr!("biblioteca", "library")), ("esc", tr!("menú", "menu"))]
        };
        if !self.editing && !self.search_rest.is_empty() {
            keys.insert(1, ("m", tr!("buscar en más", "search more")));
        }
        theme::status_bar(f, status_area, tr!("BUSCAR", "SEARCH"), &status, &keys);
    }

    fn draw_sources(&mut self, f: &mut Frame) {
        let Some(g) = self.group.and_then(|g| self.groups.get(g)) else { return };
        let [list_area, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let items: Vec<ListItem> = g
            .entries
            .iter()
            .map(|(s, m)| {
                let state = match self.health.get(&s.info.id) {
                    Some(r) if r.disabled => Span::raw(format!("{:<11}", tr!("desactivada", "disabled"))).fg(FAINT),
                    Some(r) if r.has_problem() => Span::raw(format!("{:<11}", trf!("⚠ {} fallos", "⚠ {} fails", r.fails.max(r.read_fails)))).fg(PEACH),
                    Some(Record { latency_ms: Some(ms), .. }) => Span::raw(format!("{:<11}", format!("{:.1} s", *ms as f32 / 1000.0))).fg(MINT),
                    _ => Span::raw(format!("{:<11}", "")),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<28}", s.info.name)).fg(TEXT).bold(),
                    Span::raw(format!(" {:<4} ", s.info.lang)).fg(LAVENDER),
                    state,
                    Span::raw(" "),
                    Span::raw(m.title.clone()).fg(MUTED),
                ]))
            })
            .collect();
        let title = trf!("{} · en {} fuentes", "{} · in {} sources", g.title, g.entries.len());
        f.render_stateful_widget(theme::highlight(List::new(items).block(theme::block_focused(&title))), list_area, &mut self.sources_state);
        theme::status_bar(f, status_area, tr!("FUENTE", "SOURCE"), tr!("elige de dónde leerlo", "choose where to read it"), &[("enter", tr!("abrir", "open")), ("esc", tr!("volver", "back"))]);
    }

    fn draw_source_issues(&mut self, f: &mut Frame) {
        let [list_area, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let now = crate::library::now();
        let items: Vec<ListItem> = self
            .issues
            .iter()
            .map(|issue| {
                let info = &issue.source.info;
                let record = self.health.get(&info.id);
                let state = if record.is_some_and(|r| r.disabled) {
                    Span::raw(format!("{:<22}", tr!("desactivada", "disabled"))).fg(FAINT)
                } else if issue.skipped {
                    let wait = record.and_then(|r| r.skip_until()).map_or(0, |until| until.saturating_sub(now));
                    Span::raw(format!("{:<22}", trf!("omitida · vuelve en {}", "skipped · back in {}", health::format_wait(wait)))).fg(PEACH)
                } else {
                    Span::raw(format!("{:<22}", tr!("ha fallado", "failed"))).fg(ERROR)
                };
                let mut spans = vec![
                    Span::raw(format!("{:<28}", info.name)).fg(TEXT).bold(),
                    Span::raw(format!(" {:<4} ", info.lang)).fg(LAVENDER),
                    state,
                    Span::raw(" "),
                ];
                if self.updates.iter().any(|u| u.sources.iter().any(|s| s.id.to_string() == info.id)) {
                    spans.push(Span::raw(tr!("↑ versión nueva  ", "↑ new version  ")).fg(PEACH));
                }
                spans.push(Span::raw(issue.detail.clone()).fg(MUTED));
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = trf!("⚠ fuentes con problemas · {}", "⚠ sources with problems · {}", self.issues.len());
        f.render_stateful_widget(theme::highlight(List::new(items).block(theme::block_focused(&title))), list_area, &mut self.issues_state);
        let mut keys = vec![("r", tr!("reintentar", "retry")), ("R", tr!("reintentar todas", "retry all")), ("d", tr!("activar/desactivar", "enable/disable"))];
        if !self.updates.is_empty() {
            keys.push(("U", tr!("actualizar extensiones", "update extensions")));
        }
        keys.push(("esc", tr!("volver", "back")));
        theme::status_bar(f, status_area, tr!("FALLOS", "FAILURES"), &self.status, &keys);
    }

    fn draw_manga(&mut self, f: &mut Frame) {
        let Some((source, manga)) = &self.current else { return };
        let [info_area, list_area, status_area] =
            Layout::vertical([Constraint::Length(8), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

        let m = self.details.as_ref().map(|d| &d.manga).unwrap_or(manga);
        let title = if m.title.is_empty() { &manga.title } else { &m.title };
        let mut lines = vec![Line::from(vec![
            Span::raw(title.clone()).fg(PINK).bold(),
            Span::raw(format!("  {} · {}", source.info.name, source.info.lang)).fg(MUTED),
        ])];
        let mut meta = Vec::new();
        if let Some(author) = m.author.as_ref().filter(|a| !a.is_empty()) {
            meta.push(Span::raw(format!("✎ {author}")).fg(LAVENDER));
        }
        if let Some(genre) = m.genre.as_ref().filter(|g| !g.is_empty()) {
            meta.push(Span::raw(format!("   {genre}")).fg(MUTED));
        }
        if !meta.is_empty() {
            lines.push(Line::from(meta));
        }
        match self.current_favorite() {
            Some(fav) => {
                let anilist = match &fav.anilist {
                    None => Span::raw(tr!("  ☁ sin vincular", "  ☁ not linked")).fg(FAINT),
                    Some(_) if fav.needs_sync() => Span::raw(tr!("  ☁ ↻ cambios sin sincronizar", "  ☁ ↻ unsynced changes")).fg(PEACH),
                    Some(link) => Span::raw(format!("  ☁ ✓ {}", link.title)).fg(MINT),
                };
                lines.push(Line::from(vec![
                    Span::raw(tr!(" ♥ favorito ", " ♥ favorite ")).bg(PINK).fg(ON_PINK).bold(),
                    Span::raw(" "),
                    Span::raw(format!(" {} ", fav.status.label())).bg(theme::status_color(fav.status)).fg(ON_PINK),
                    Span::raw(trf!("  leídos: {}", "  read: {}", fav.progress)).fg(TEXT),
                    anilist,
                ]));
            }
            None => lines.push(Line::from(vec![
                Span::raw("♡ ").fg(FAINT),
                Span::raw(tr!("no está en favoritos · ", "not in favorites · ")).fg(MUTED),
                Span::raw("f").fg(PINK).bold(),
                Span::raw(tr!(" para añadirlo", " to add it")).fg(MUTED),
            ])),
        }
        if let Some(desc) = &m.description {
            lines.push(Line::from(desc.replace(['\n', '\r'], " ")).fg(MUTED));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(theme::block("manga")), info_area);

        let last_read = self.current_favorite().and_then(|f| f.last_read.as_ref()).map(|l| l.chapter_url.clone());
        let read_key = self.current_key().unwrap_or_default();
        match &self.details {
            None if self.details_error.is_some() => f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("✕ {}", self.details_error.as_deref().unwrap_or_default())).fg(ERROR),
                    Line::default(),
                    Line::from(vec![Span::raw("s").fg(PINK).bold(), Span::raw(tr!(" buscar en otras fuentes", " look in other sources")).fg(MUTED)]),
                    Line::from(vec![Span::raw("r").fg(PINK).bold(), Span::raw(tr!(" reintentar con más margen", " retry with more time")).fg(MUTED)]),
                ])
                .wrap(Wrap { trim: true })
                .block(theme::block_focused(tr!("capítulos", "chapters"))),
                list_area,
            ),
            None => f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(theme::spinner(self.tick)).fg(PINK),
                    Span::raw(tr!(" cargando capítulos…", " loading chapters…")).fg(MUTED),
                ]))
                .block(theme::block_focused(tr!("capítulos", "chapters"))),
                list_area,
            ),
            Some(d) => {
                let items: Vec<ListItem> = self
                    .chapter_view
                    .iter()
                    .map(|&i| {
                        let c = &d.chapters[i];
                        let read = self.reads.is_read(&read_key, &c.url);
                        let marker = if last_read.as_deref() == Some(c.url.as_str()) {
                            Span::raw("▸ ").fg(PINK)
                        } else if read {
                            Span::raw("✓ ").fg(PINK_DIM)
                        } else {
                            Span::raw("  ")
                        };
                        // Los leídos se apagan; los pendientes quedan en primer plano
                        let mut spans = vec![marker];
                        if self.downloads.contains(&read_key, &c.url) {
                            spans.push(Span::raw("⬇ ").fg(MINT));
                        }
                        spans.push(Span::raw(c.name.clone()).fg(if read { FAINT } else { TEXT }));
                        if let Some(s) = c.scanlator.as_ref().filter(|s| !s.is_empty()) {
                            spans.push(Span::raw(format!("  {s}")).fg(FAINT));
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect();
                let read_count = d.chapters.iter().filter(|c| self.reads.is_read(&read_key, &c.url)).count();
                let mut title = trf!("capítulos · {} · {read_count} leídos", "chapters · {} · {read_count} read", d.chapters.len());
                if self.chapters_reversed {
                    title += tr!(" · ↑ viejos primero", " · ↑ oldest first");
                }
                if let Some(progress) = &self.download_progress {
                    title = format!("{title} · {progress}");
                }
                match &self.chapter_input {
                    Some(input) => {
                        let label = match input.kind {
                            ChapterInputKind::Filter => tr!("filtrar", "filter").to_string(),
                            ChapterInputKind::Goto => tr!("ir al capítulo", "go to chapter").to_string(),
                            ChapterInputKind::Download => {
                                trf!("descargar {} capítulos ({}) en", "download {} chapters ({}) to", self.missing_chapters().len(), self.prefs.download_format.label())
                            }
                        };
                        let cursor = if self.tick % 16 < 9 { "▏" } else { " " };
                        title = format!("{label}: {}{cursor}", input.text);
                    }
                    None if !self.chapter_filter.is_empty() => {
                        title += &trf!(" · filtro «{}» ({})", " · filter “{}” ({})", self.chapter_filter, self.chapter_view.len());
                    }
                    None => {}
                }
                f.render_stateful_widget(
                    theme::highlight(List::new(items).block(theme::block_focused(&title))),
                    list_area,
                    &mut self.chapters_state,
                );
            }
        }
        theme::status_bar(
            f,
            status_area,
            "MANGA",
            &self.status,
            &if self.chapter_input.is_some() {
                let mut keys = vec![("enter", tr!("aceptar", "ok")), ("esc", tr!("cancelar", "cancel"))];
                if self.chapter_input.as_ref().is_some_and(|i| i.kind == ChapterInputKind::Download) {
                    keys.push(("tab", tr!("formato", "format")));
                }
                keys
            } else {
                vec![("enter", tr!("leer", "read")), ("s", tr!("otras fuentes", "other sources")), ("d/D", tr!("descargar", "download")), ("m/M", tr!("leído", "read")), ("/", tr!("filtrar", "filter")), ("#", tr!("ir a", "go to")), ("o", tr!("orden", "order")), ("c", tr!("seguir", "continue")), ("f", "fav"), ("a", "anilist")]
            },
        );
    }

    fn draw_reader(&mut self, f: &mut Frame) {
        let [image_area, status_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        if image_area != self.reader_area {
            // Cambió el tamaño: recodificar lo que ya está descargado
            self.reader_area = image_area;
            let loaded: Vec<usize> = self.reader.as_ref().map(|r| r.images.keys().copied().collect()).unwrap_or_default();
            for index in loaded {
                self.request_encode(index);
            }
        }
        let size = (image_area.width, image_area.height);
        let (sixel, font, tick) = (self.sixel, self.font, self.tick);
        let Some(r) = self.reader.as_mut() else { return };

        let chapter = &r.chapters[r.chapter];
        let progress = if r.pages.is_empty() { "…".to_string() } else { format!("{}/{}", r.page + 1, r.pages.len()) };
        let mut info = format!("{} · {} · {}", r.manga.title, chapter.name, progress);
        if r.view.fit != Fit::Page {
            info += &format!(" · {}", r.view.fit.label());
        }
        if r.view.zoom != 100 {
            info += &format!(" · {}%", r.view.zoom);
        }

        let view = r.view;
        let page = r.images.get_mut(&r.page);
        let ready = page.as_ref().is_some_and(|p| !sixel || p.encoded.as_ref().is_some_and(|((s, _), _)| *s == size));
        let has_error = r.error.is_some();
        if let Some(err) = &r.error {
            f.render_widget(Paragraph::new(format!("✕ {err}")).fg(ERROR).wrap(Wrap { trim: true }).centered(), image_area);
        } else if let (true, Some(page)) = (ready, page) {
            if let Some((_, enc)) = page.encoded.as_ref().filter(|_| sixel) {
                // La imagen la escribe flush_sixel() después del frame; aquí solo la info
                let kind = if enc.gray { tr!("gris", "gray") } else { "color" };
                info += &trf!("  · sixel {kind} {}×{} · celda {}×{}", "  · sixel {kind} {}×{} · cell {}×{}", enc.out.0, enc.out.1, font.0, font.1);
            } else {
                // Otros protocolos: recortar la parte visible aquí y que ratatui-image la pinte
                let key = (size, view);
                if page.proto.as_ref().is_none_or(|(k, _, _)| *k != key) {
                    let framed = render::frame(&page.img, size.0 as u32 * font.0 as u32, size.1 as u32 * font.1 as u32, view);
                    page.proto = Some((key, self.picker.new_resize_protocol(framed.img), framed.overflow));
                }
                let (_, proto, _) = page.proto.as_mut().unwrap();
                let resize = ratatui_image::Resize::Fit(Some(FilterType::Lanczos3));
                // Centrar: la imagen se ajusta a un lado y suele sobrar el otro
                let fit = proto.size_for(resize.clone(), image_area.as_size());
                let x = image_area.x + image_area.width.saturating_sub(fit.width) / 2;
                let y = image_area.y + image_area.height.saturating_sub(fit.height) / 2;
                let area = Rect::new(x, y, fit.width.min(image_area.width), fit.height.min(image_area.height));
                f.render_stateful_widget(StatefulImage::<StatefulProtocol>::default().resize(resize), area, proto);
                info += &format!("  · {:?}", self.picker.protocol_type());
            }
        } else {
            let msg = if r.pages.is_empty() { tr!(" cargando páginas…", " loading pages…") } else { tr!(" cargando imagen…", " loading image…") };
            let [center] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(image_area);
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::raw(theme::spinner(tick)).fg(PINK), Span::raw(msg).fg(MUTED)])).centered(),
                center,
            );
        }
        let keys = if has_error {
            vec![("r", tr!("reintentar", "retry")), ("←→", tr!("página", "page")), ("n/p", tr!("capítulo", "chapter")), ("esc", tr!("volver", "back"))]
        } else {
            vec![("←→", tr!("página", "page")), ("n/p", tr!("capítulo", "chapter")), ("f", tr!("ajuste", "fit")), ("+/-", "zoom"), ("esc", tr!("volver", "back"))]
        };
        theme::status_bar(f, status_area, tr!("LEYENDO", "READING"), &info, &keys);
    }
}

/// Ficha del favorito seleccionado en la biblioteca.
fn favorite_card(fav: &crate::library::Favorite) -> Paragraph<'_> {
    let mut lines = vec![
        Line::raw(""),
        Line::from(fav.manga.title.clone()).fg(PINK).bold(),
        Line::from(format!("{} · {}", fav.source_name, fav.lang)).fg(MUTED),
        Line::raw(""),
        Line::from(vec![
            Span::raw(format!(" {} ", fav.status.label())).bg(theme::status_color(fav.status)).fg(ON_PINK).bold(),
            Span::raw(trf!("  {} capítulos leídos", "  {} chapters read", fav.progress)).fg(TEXT),
        ]),
    ];
    if let Some(last) = &fav.last_read {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw(tr!("▸ seguir en ", "▸ continue at ")).fg(MUTED),
            Span::raw(last.chapter_name.clone()).fg(TEXT),
            Span::raw(trf!(" · pág. {}", " · p. {}", last.page + 1)).fg(MUTED),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(match &fav.anilist {
        None => Line::from(tr!("☁ sin vincular a AniList", "☁ not linked to AniList")).fg(FAINT),
        Some(link) if fav.needs_sync() => Line::from(trf!("☁ {} · ↻ cambios sin sincronizar", "☁ {} · ↻ unsynced changes", link.title)).fg(PEACH),
        Some(link) => Line::from(trf!("☁ {} · ✓ sincronizado", "☁ {} · ✓ synced", link.title)).fg(MINT),
    });
    if let Some(desc) = fav.manga.description.as_ref().filter(|d| !d.is_empty()) {
        lines.push(Line::raw(""));
        lines.push(Line::from(desc.replace(['\n', '\r'], " ")).fg(MUTED));
    }
    Paragraph::new(lines).wrap(Wrap { trim: true }).block(theme::block(tr!("ficha", "details")).padding(ratatui::widgets::Padding::horizontal(1)))
}

// ───────────────────────────── helpers ─────────────────────────────

fn spawn_input(tx: mpsc::UnboundedSender<Msg>) {
    std::thread::spawn(move || loop {
        if !event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if tx.is_closed() {
                break;
            }
            continue;
        }
        let msg = match event::read() {
            Ok(Event::Key(k)) => Msg::Key(k),
            Ok(Event::Resize(..)) => Msg::Resize,
            _ => continue,
        };
        if tx.send(msg).is_err() {
            break;
        }
    });
}

pub fn page_cache_path(source_id: &str, chapter_url: &str, index: usize) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (source_id, chapter_url).hash(&mut h);
    let dir = paths::pages_cache().join(format!("{:016x}", h.finish()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{index:04}"))
}

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// "12" para 12.0, "12.5" para 12.5 (para comparar con lo que escribe el usuario).
fn format_number(n: f32) -> String {
    if n.fract() == 0.0 { format!("{n:.0}") } else { n.to_string() }
}

fn select_next(state: &mut ListState, len: usize) {
    if len > 0 {
        state.select(Some(state.selected().map_or(0, |i| (i + 1).min(len - 1))));
    }
}

fn select_prev(state: &mut ListState) {
    state.select(Some(state.selected().map_or(0, |i| i.saturating_sub(1))));
}

fn page_move(state: &mut ListState, len: usize, delta: isize) {
    if len > 0 {
        let i = state.selected().unwrap_or(0) as isize + delta;
        state.select(Some(i.clamp(0, len as isize - 1) as usize));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Favorite;
    use crate::model::SourceInfo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_source(id: &str) -> LoadedSource {
        LoadedSource { info: SourceInfo { id: id.into(), name: id.into(), lang: "es".into(), base_url: None, supports_latest: true } }
    }

    fn test_manga(title: &str) -> Manga {
        Manga { url: format!("/{title}"), title: title.into(), ..Default::default() }
    }

    #[test]
    fn open_group_follows_its_position_when_results_arrive() {
        let mut app = App::new(Picker::halfblocks());
        app.query = "blue".into();
        app.on_search_event(SearchEvent::Results { source: test_source("a"), mangas: vec![test_manga("Blue Lock"), test_manga("Grand Blue")] });
        let index = app.groups.iter().position(|g| g.key == "grandblue").unwrap();
        app.group = Some(index);
        app.on_search_event(SearchEvent::Results { source: test_source("b"), mangas: vec![test_manga("Blue"), test_manga("Blue Exorcist")] });
        assert_eq!(app.groups[app.group.unwrap()].key, "grandblue");
    }

    #[test]
    fn alternatives_put_the_failed_source_last() {
        let mut app = App::new(Picker::halfblocks());
        app.query = "grand blue".into();
        app.on_search_event(SearchEvent::Results { source: test_source("mala"), mangas: vec![test_manga("Grand Blue")] });
        app.on_search_event(SearchEvent::Results { source: test_source("buena"), mangas: vec![test_manga("Grand Blue")] });
        app.current = Some((test_source("mala"), test_manga("Grand Blue")));
        app.details_error = Some("error".into());
        app.screen = Screen::Manga;
        app.find_alternatives();
        assert!(app.screen == Screen::Sources);
        let entries = &app.groups[app.group.unwrap()].entries;
        assert_eq!(entries.last().unwrap().0.info.id, "mala");
    }

    #[test]
    fn alternatives_say_so_when_no_other_source_has_it() {
        let mut app = App::new(Picker::halfblocks());
        app.query = "grand blue".into();
        app.on_search_event(SearchEvent::Results { source: test_source("mala"), mangas: vec![test_manga("Grand Blue")] });
        app.current = Some((test_source("mala"), test_manga("Grand Blue")));
        app.screen = Screen::Manga;
        app.find_alternatives();
        assert!(app.screen == Screen::Manga);
        assert_eq!(app.status, tr!("ninguna otra fuente lo tiene", "no other source has it"));
    }

    fn app_with_chapters(n: usize) -> App {
        let mut app = App::new(Picker::halfblocks());
        app.downloads = Manifest::default();
        let manga = test_manga("Grand Blue");
        app.current = Some((test_source("a"), manga.clone()));
        let chapters = (0..n).map(|i| Chapter { url: format!("/c/{i}"), name: format!("Cap {i}"), chapter_number: i as f32, ..Default::default() }).collect();
        app.screen = Screen::Manga;
        app.update(Msg::Details(app.generation, Ok(MangaDetails { manga, chapters })));
        app
    }

    #[test]
    fn extensions_show_why_they_failed_to_load() {
        let mut app = App::new(Picker::halfblocks());
        app.installed = vec![Installed { pkg: "p".into(), name: "Rota".into(), version_code: 1, version_name: "1.0".into(), content_warning: 1, langs: vec!["es".into()] }];
        app.load_errors = vec![("Rota".into(), "java.lang.NoClassDefFoundError: android/webkit/Foo".into())];
        app.screen = Screen::Extensions;
        let mut terminal = Terminal::new(TestBackend::new(140, 10)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("✕") && text.contains("Android"), "{text}");
    }

    #[test]
    fn retry_asks_again_for_the_failed_page() {
        let mut app = App::new(Picker::halfblocks());
        app.screen = Screen::Reader;
        app.reader = Some(Reader {
            source: test_source("a"),
            manga_url: "/m".into(),
            manga: test_manga("M"),
            chapters: vec![Chapter { url: "/c".into(), ..Default::default() }],
            chapter: 0,
            pages: vec![serde_json::from_str(r#"{"index":0,"url":"/p"}"#).unwrap()],
            page: 0,
            requested: HashSet::from([0]),
            images: HashMap::new(),
            error: Some("error".into()),
            view: View::default(),
        });
        app.key_reader(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let r = app.reader.as_ref().unwrap();
        assert!(r.error.is_none() && !r.requested.contains(&0));
    }

    #[test]
    fn download_prompt_counts_chapters_and_toggles_format() {
        let mut app = app_with_chapters(3);
        app.prefs.download_format = download::Format::Cbz;
        app.key_manga(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        assert!(app.chapter_input.as_ref().is_some_and(|i| i.kind == ChapterInputKind::Download));
        app.key_manga(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.prefs.download_format, download::Format::Folder);

        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains(&trf!("descargar {} capítulos", "download {} chapters", 3)), "{text}");

        app.key_manga(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.chapter_input.is_none());
    }

    #[test]
    fn download_progress_shows_and_clears() {
        let mut app = app_with_chapters(1);
        app.on_download_event(download::Event::Progress { chapter: 1, chapters: 2, name: "Cap 0".into(), page: 3, pages: 10 });
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("3/10"), "{text}");

        app.download_cancel = Some(Arc::default());
        app.on_download_event(download::Event::Finished(download::Summary { saved: 1, ..Default::default() }));
        assert!(app.download_progress.is_none() && app.download_cancel.is_none());
        assert_eq!(app.status, trf!("✓ {} capítulos guardados", "✓ {} chapters saved", 1));
    }

    #[test]
    fn a_download_does_not_redraw_the_reader() {
        let mut app = App::new(Picker::halfblocks());
        app.screen = Screen::Reader;
        app.dirty = false;
        app.update(Msg::Download(download::Event::Progress { chapter: 1, chapters: 1, name: "c".into(), page: 1, pages: 2 }));
        assert!(!app.dirty);
    }

    #[test]
    fn m_asks_for_the_next_batch_keeping_the_results() {
        let mut app = App::new(Picker::halfblocks());
        app.screen = Screen::Search;
        app.on_search_event(SearchEvent::Results { source: test_source("a"), mangas: vec![test_manga("Grand Blue")] });
        app.search_rest = (0..health::BATCH_SIZE + 5).map(|i| test_source(&format!("s{i}"))).collect();

        app.key_search(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.search_rest.len(), 5);
        assert_eq!(app.groups.len(), 1);

        app.key_search(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(app.search_rest.is_empty());

        app.key_search(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.status, tr!("ya has buscado en todas las fuentes", "you've already searched every source"));
    }

    #[test]
    fn m_waits_for_the_current_batch() {
        let mut app = App::new(Picker::halfblocks());
        app.screen = Screen::Search;
        app.pending = 3;
        app.search_rest = vec![test_source("a")];
        app.key_search(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.search_rest.len(), 1);
    }

    #[test]
    fn alternatives_search_the_next_batch_before_giving_up() {
        let mut app = App::new(Picker::halfblocks());
        app.query = "grand blue".into();
        app.on_search_event(SearchEvent::Results { source: test_source("mala"), mangas: vec![test_manga("Grand Blue")] });
        app.search_rest = vec![test_source("otra")];
        app.current = Some((test_source("mala"), test_manga("Grand Blue")));
        app.screen = Screen::Manga;
        app.find_alternatives();
        assert!(app.screen == Screen::Search);
        assert!(app.search_rest.is_empty());
    }

    #[test]
    fn failed_sources_can_be_listed_from_the_search() {
        let mut app = App::new(Picker::halfblocks());
        app.screen = Screen::Search;
        let f_key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);
        app.key_search(f_key);
        assert!(app.screen == Screen::Search, "sin fallos no hay panel");

        app.on_search_event(SearchEvent::Failed { source: test_source("a"), error: "error 500".into() });
        app.key_search(f_key);
        assert!(app.screen == Screen::SourceIssues);

        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("error 500"), "{text}");
    }

    #[test]
    fn a_failed_manga_shows_the_error_instead_of_spinning() {
        let mut app = App::new(Picker::halfblocks());
        app.current = Some((test_source("a"), test_manga("Grand Blue")));
        app.screen = Screen::Manga;
        assert!(app.animated(), "cargando");
        app.update(Msg::Details(app.generation, Err(anyhow::anyhow!("eu.kanade.tachiyomi.network.HttpException: HTTP error 500"))));
        assert!(!app.animated(), "ya no carga nada");
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("500") && !text.contains(tr!("cargando capítulos", "loading chapters")), "{text}");
    }

    fn fake_favorite(title: &str, status: Status, progress: i32, linked: bool, synced: bool) -> Favorite {
        Favorite {
            source_id: "1".into(),
            source_name: tr!("Manga TV", "MangaDex").into(),
            lang: tr!("es", "en").into(),
            manga: Manga {
                url: format!("/manga/{title}"),
                title: title.into(),
                description: Some(
                    tr!(
                        "Iori Kitahara se muda a una ciudad costera para empezar la universidad y termina en un club de buceo muy particular.",
                        "Iori Kitahara moves to a seaside town to start university and ends up in a very peculiar diving club."
                    )
                    .into(),
                ),
                ..Default::default()
            },
            added_at: 0,
            status,
            progress,
            last_read: Some(LastRead { chapter_url: "/c/12".into(), chapter_name: tr!("Capítulo 12", "Chapter 12").into(), page: 7 }),
            anilist: linked.then(|| AniListLink {
                media_id: 87395,
                title: "Grand Blue".into(),
                synced_progress: Some(if synced { progress } else { progress - 2 }),
                synced_status: Some(status),
                synced_at: None,
            }),
        }
    }

    /// Previsualiza biblioteca y manga como JSON (ver tools/preview.py). Uso:
    /// HANTHO_PREVIEW_DIR=<dir> cargo test --release screens_preview
    #[test]
    fn screens_preview() {
        let Ok(dir) = std::env::var("HANTHO_PREVIEW_DIR") else { return };
        let mut app = App::new(Picker::halfblocks());
        app.library.favorites = vec![
            fake_favorite("Grand Blue", Status::Current, 12, true, false),
            fake_favorite("Chainsaw Man", Status::Completed, 97, true, true),
            fake_favorite("Frieren", Status::Planning, 0, false, false),
            fake_favorite("Dandadan", Status::Paused, 54, true, true),
        ];
        app.library_state.select(Some(0));
        app.anilist_user = Some("alexp".into());

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        app.screen = Screen::Library;
        app.status = tr!("★ añadido a favoritos", "★ added to favorites").into();
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/library.json"));

        let fav = app.library.favorites[0].clone();
        app.current = Some((LoadedSource { info: SourceInfo { id: "1".into(), name: tr!("Manga TV", "MangaDex").into(), lang: tr!("es", "en").into(), base_url: None, supports_latest: true } }, fav.manga.clone()));
        let chapters: Vec<Chapter> = (1..=20)
            .rev()
            .map(|n| Chapter { url: format!("/c/{n}"), name: trf!("Capítulo {n}", "Chapter {n}"), chapter_number: n as f32, scanlator: Some(tr!("Plot Twist No Fansub", "Kodansha").into()), ..Default::default() })
            .collect();
        let mut manga = fav.manga.clone();
        manga.author = Some("Kenji Inoue, Kimitake Yoshioka".into());
        manga.genre = Some(tr!("Comedia, Recuentos de la vida, Seinen", "Comedy, Slice of Life, Seinen").into());
        let key = reads::key("1", &fav.manga.url);
        for c in chapters.iter().filter(|c| c.chapter_number <= 12.0) {
            app.reads.mark(&key, c);
        }
        app.details = Some(MangaDetails { manga, chapters });
        app.rebuild_chapter_view();
        app.chapters_state.select(Some(7));
        app.screen = Screen::Manga;
        app.status = tr!("✓ AniList: Leyendo · capítulo 12", "✓ AniList: Reading · chapter 12").into();
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/manga.json"));

        app.backup_files = vec![
            std::path::PathBuf::from(r"C:\Users\alexp\Downloads\app.mihon_2026-10-29_21-04.tachibk"),
            std::path::PathBuf::from(r"C:\Users\alexp\Documents\tachiyomi_2025-03-02_10-11.tachibk"),
        ];
        app.backup_state.select(Some(1));
        app.backup_report = vec![
            tr!("✓ 48 favoritos nuevos · 2 ya estaban", "✓ 48 new favorites · 2 already there").into(),
            tr!("✓ 3120 capítulos marcados como leídos", "✓ 3120 chapters marked as read").into(),
            tr!("✓ 31 vinculados con AniList", "✓ 31 linked to AniList").into(),
            tr!("! sin extensión en tus repositorios: Fuente Rara", "! no extension in your repositories: Rare Source").into(),
            tr!("✓ 6 extensiones instaladas y cargadas", "✓ 6 extensions installed and loaded").into(),
        ];
        app.screen = Screen::Backup;
        app.status = tr!("importación terminada ✿", "import finished ✿").into();
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/backup.json"));

        // Inicio
        let src = |id: &str, name: &str| LoadedSource {
            info: SourceInfo { id: id.into(), name: name.into(), lang: tr!("es", "en").into(), base_url: None, supports_latest: true },
        };
        let names = tr!(
            ["Manga TV", "LectorTMO", "Olympus Scanlation", "InManga", "Ikigai Mangas"],
            ["MangaDex", "Weeb Central", "MangaPark", "Bato.to", "Comick"]
        );
        app.sources = names
            .iter()
            .enumerate()
            .map(|(i, n)| src(&i.to_string(), n))
            .collect();
        app.installed = app
            .sources
            .iter()
            .map(|s| Installed {
                pkg: format!("eu.kanade.tachiyomi.extension.es.{}", s.info.name.to_lowercase().replace(' ', "")),
                name: s.info.name.clone(),
                version_code: 12,
                version_name: "1.4.12".into(),
                content_warning: 1,
                langs: vec!["es".into()],
            })
            .collect();
        app.screen = Screen::Home;
        app.home.skip_intro();
        for _ in 0..40 {
            app.home.tick(Rect::new(0, 0, 120, 30));
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/home.json"));

        // Búsqueda global
        let results = [
            ("Grand Blue", vec![0, 1, 2, 3]),
            ("Grand Blue Dreaming", vec![4]),
            ("Grand Blue: Gekijouban", vec![1]),
            ("Blue Lock", vec![0, 1, 2, 3, 4]),
            ("Blue Period", vec![1, 3]),
            ("Ao no Hako (Blue Box)", vec![0, 2]),
        ];
        app.groups = results
            .iter()
            .map(|(title, idx)| Group {
                key: search::normalize(title),
                title: title.to_string(),
                entries: idx.iter().map(|&i| (app.sources[i].clone(), Manga { title: title.to_string(), ..Default::default() })).collect(),
            })
            .collect();
        app.query = "grand blue".into();
        app.input = "grand blue".into();
        app.editing = false;
        app.groups_state.select(Some(0));
        app.status = tr!("5 fuentes en 0,8 s", "5 sources in 0.8 s").into();
        app.screen = Screen::Search;
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/search.json"));

        // Añadir extensiones
        let cat_ext = |name: &str, langs: &[&str], version: i64, warning: i32| catalog::Extension {
            name: name.into(),
            package_name: format!("eu.kanade.tachiyomi.extension.es.{}", name.to_lowercase().replace(' ', "")),
            version_code: version,
            version_name: format!("1.4.{version}"),
            content_warning: warning,
            sources: langs
                .iter()
                .map(|l| catalog::Source { id: 0, name: name.into(), language: l.to_string(), home_url: String::new() })
                .collect(),
            ..Default::default()
        };
        app.add.repos = vec!["https://example.org/repo/index.pb".into()];
        let catalog = tr!(
            [
                ("Ikigai Mangas", 12, 1),
                ("InManga", 12, 1),
                ("Lector Manga", 8, 2),
                ("LectorTMO", 13, 1),
                ("Manga Crab", 4, 1),
                ("Manga TV", 12, 1),
                ("Mangas.in", 6, 1),
                ("Olympus Scanlation", 12, 1),
                ("Plot Twist No Fansub", 3, 1),
                ("Tu Manga Online", 9, 2),
                ("Visor Ikigai", 5, 1),
            ],
            [
                ("Asura Scans", 6, 1),
                ("Bato.to", 12, 1),
                ("Comick", 12, 1),
                ("Flame Comics", 4, 1),
                ("MangaBuddy", 8, 2),
                ("MangaDex", 12, 1),
                ("MangaFire", 13, 1),
                ("MangaPark", 12, 1),
                ("Manganato", 3, 1),
                ("Toonily", 9, 3),
                ("Weeb Central", 12, 1),
            ]
        );
        let lang = tr!("es", "en");
        let mut catalog: Vec<catalog::Extension> = catalog.iter().map(|&(n, v, w)| cat_ext(n, &[lang], v, w)).collect();
        catalog.push(cat_ext("Hitomi.la", &["all"], 16, 3));
        app.add.catalog = Some(catalog);
        app.add.lang = Some(lang.into());
        app.add.busy.insert(tr!("eu.kanade.tachiyomi.extension.es.mangacrab", "eu.kanade.tachiyomi.extension.es.flamecomics").into());
        app.add.state.select(Some(tr!(3, 6)));
        app.status = tr!("instalando Manga Crab…", "installing Flame Comics…").into();
        app.screen = Screen::AddExtensions;
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/add_extensions.json"));
        app.add.busy.clear();
        app.add.repos.clear();
        app.add.repo_input = Some("https://github.com/".into());
        app.status = String::new();
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/add_repo.json"));
        app.add.repo_input = None;

        // AniList
        app.screen = Screen::AniList;
        app.status = tr!("conectado ✿", "logged in ✿").into();
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/anilist.json"));

        // Elegir la obra de AniList
        let media = |id: i64, romaji: &str, english: Option<&str>, format: &str, year: i32, chapters: Option<i32>| Media {
            id,
            title: anilist::Title { romaji: Some(romaji.into()), english: english.map(Into::into), native: None },
            format: Some(format.into()),
            chapters,
            status: None,
            start_date: Some(anilist::StartDate { year: Some(year) }),
            site_url: None,
        };
        app.anilist_matches = Some(vec![
            media(87395, "Grand Blue", Some("Grand Blue Dreaming"), "MANGA", 2014, None),
            media(2, "Blue Lock", None, "MANGA", 2018, None),
            media(3, "Blue Period", None, "MANGA", 2017, None),
            media(4, "Ao no Hako", Some("Blue Box"), "MANGA", 2021, None),
        ]);
        app.anilist_state.select(Some(0));
        app.screen = Screen::AnilistPick;
        terminal.draw(|f| app.draw(f)).unwrap();
        theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/anilist_pick.json"));

        // Lector: la página la dibuja tools/screenshots.py encima (Sixel no pasa por el buffer)
        let Ok(page_path) = std::env::var("HANTHO_PREVIEW_PAGE") else { return };
        let img = Arc::new(image::open(page_path).unwrap());
        let details = app.details.clone().unwrap();
        for (name, width) in [("reader", 120u16), ("reader_side", 64)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            app.sixel = true;
            app.font = (10, 20);
            app.reader_area = Rect::new(0, 0, width, 29);
            let encoded = render::encode(&img, width, 29, app.font, View::default());
            let pages: Vec<Page> = (0..18).map(|i| Page { index: i, url: String::new(), image_url: None }).collect();
            let mut images = HashMap::new();
            images.insert(6, PageImage { img: img.clone(), encoded: Some((((width, 29), View::default()), encoded)), encoding: None, proto: None });
            app.reader = Some(Reader {
                source: app.sources[0].clone(),
                manga: details.manga.clone(),
                manga_url: details.manga.url.clone(),
                chapters: details.chapters.clone(),
                chapter: 7,
                pages,
                page: 6,
                requested: HashSet::new(),
                images,
                error: None,
                view: View::default(),
            });
            app.screen = Screen::Reader;
            terminal.draw(|f| app.draw(f)).unwrap();
            theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/{name}.json"));
        }
    }

    /// Graba una sesión de ejemplo fotograma a fotograma para el vídeo del README
    /// (ver tools/demo.py): inicio → búsqueda → manga → capítulo 13 → lectura → AniList.
    /// Lo que en la app real hace el host (buscar, cargar capítulos, descargar páginas) se
    /// inyecta con los mismos mensajes que mandaría; el resto son pulsaciones de teclas.
    #[test]
    fn demo_frames() {
        let (Ok(dir), Ok(pages_dir)) = (std::env::var("HANTHO_DEMO_DIR"), std::env::var("HANTHO_DEMO_PAGES")) else { return };
        let mut app = App::new(Picker::halfblocks());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut frames: Vec<serde_json::Value> = Vec::new();
        let area = Rect::new(0, 0, 120, 30);

        // Un fotograma: dibuja, vuelca y apunta cuánto dura (y qué página va encima, si es el lector)
        let mut shot = |app: &mut App, ms: u32| {
            app.tick += 1;
            if app.screen == Screen::Home {
                app.home.tick(area);
            }
            terminal.draw(|f| app.draw(f)).unwrap();
            let n = frames.len();
            theme::dump_buffer(terminal.backend().buffer(), &format!("{dir}/frame_{n:04}.json"));
            let page = (app.screen == Screen::Reader).then(|| app.reader.as_ref().unwrap().page);
            frames.push(serde_json::json!({ "file": format!("frame_{n:04}.json"), "ms": ms, "page": page }));
        };
        let key = |app: &mut App, code: KeyCode| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));

        // Datos: fuentes cargadas, Grand Blue en favoritos (12 leídos, vinculado con AniList)
        let src = |id: &str, name: &str| LoadedSource {
            info: SourceInfo { id: id.into(), name: name.into(), lang: tr!("es", "en").into(), base_url: None, supports_latest: true },
        };
        let names = tr!(
            ["Manga TV", "LectorTMO", "Olympus Scanlation", "InManga", "Ikigai Mangas"],
            ["MangaDex", "Weeb Central", "MangaPark", "Bato.to", "Comick"]
        );
        app.sources = names.iter().enumerate().map(|(i, n)| src(&i.to_string(), n)).collect();
        app.installed = names
            .iter()
            .map(|n| Installed { pkg: n.to_lowercase(), name: n.to_string(), version_code: 1, version_name: "1.4.12".into(), content_warning: 1, langs: vec![tr!("es", "en").into()] })
            .collect();
        app.add.repos = vec!["https://example.org/index.pb".into()];
        app.status = trf!("{} fuentes listas", "{} sources ready", app.sources.len());
        let mut fav = fake_favorite("Grand Blue", Status::Current, 12, true, true);
        fav.source_name = names[1].into();
        app.library.favorites = vec![fav.clone()];
        app.anilist_user = Some("alexp".into());
        let chapters: Vec<Chapter> = (1..=20)
            .rev()
            .map(|n| Chapter { url: format!("/c/{n}"), name: trf!("Capítulo {n}", "Chapter {n}"), chapter_number: n as f32, scanlator: Some(tr!("Plot Twist No Fansub", "Kodansha").into()), ..Default::default() })
            .collect();
        let key_read = reads::key("1", &fav.manga.url);
        for c in chapters.iter().filter(|c| c.chapter_number <= 12.0) {
            app.reads.mark(&key_read, c);
        }

        // 1. Inicio: el logo aparece y caen pétalos
        app.screen = Screen::Home;
        for i in 0..32 {
            app.home.set_time(i as f32 * 0.08);
            shot(&mut app, 80);
        }
        for _ in 0..8 {
            shot(&mut app, 80);
        }
        key(&mut app, KeyCode::Down);
        shot(&mut app, 600);

        // 2. Buscar «grand blue»
        key(&mut app, KeyCode::Enter);
        shot(&mut app, 500);
        for c in "grand blue".chars() {
            key(&mut app, KeyCode::Char(c));
            shot(&mut app, 110);
        }
        shot(&mut app, 400);
        app.query = app.input.trim().to_string();
        app.editing = false;
        app.search_gen += 1;
        app.pending = app.sources.len();
        shot(&mut app, 350);
        let results: [&[&str]; 5] = [
            &["Grand Blue", "Blue Lock", "Ao no Hako (Blue Box)"],
            &["Grand Blue", "Grand Blue: Gekijouban", "Blue Lock", "Blue Period"],
            &["Grand Blue", "Blue Lock", "Ao no Hako (Blue Box)"],
            &["Grand Blue", "Blue Lock", "Blue Period"],
            &["Grand Blue Dreaming", "Blue Lock"],
        ];
        for i in [1usize, 0, 2, 3, 4] {
            let mangas = results[i].iter().map(|t| Manga { url: format!("/manga/{t}"), title: t.to_string(), ..Default::default() }).collect();
            app.update(Msg::Search(app.search_gen, SearchEvent::Results { source: app.sources[i].clone(), mangas }));
            shot(&mut app, 300);
        }
        app.status = tr!("5 fuentes en 2,1 s", "5 sources in 2.1 s").into();
        shot(&mut app, 1300);

        // 3. Elegir fuente y abrir el manga
        key(&mut app, KeyCode::Enter);
        shot(&mut app, 1100);
        let (source, manga) = app.groups[app.group.unwrap()].entries[0].clone();
        app.generation += 1;
        app.details = None;
        app.chapters_state.select(None);
        app.status = trf!("cargando {} desde {}…", "loading {} from {}…", manga.title, source.info.name);
        app.current = Some((source.clone(), manga.clone()));
        app.manga_back = Screen::Sources;
        app.screen = Screen::Manga;
        for _ in 0..4 {
            shot(&mut app, 120);
        }
        let mut full = fav.manga.clone();
        full.author = Some("Kenji Inoue, Kimitake Yoshioka".into());
        full.genre = Some(tr!("Comedia, Recuentos de la vida, Seinen", "Comedy, Slice of Life, Seinen").into());
        app.update(Msg::Details(app.generation, Ok(MangaDetails { manga: full, chapters: chapters.clone() })));
        shot(&mut app, 1800);

        // 4. Saltar al capítulo 13 con #
        key(&mut app, KeyCode::Char('#'));
        shot(&mut app, 450);
        for c in "13".chars() {
            key(&mut app, KeyCode::Char(c));
            shot(&mut app, 220);
        }
        key(&mut app, KeyCode::Enter);
        shot(&mut app, 1000);

        // 5. Leer: las páginas son dibujos de ejemplo que tools/demo.py pone encima
        let page_imgs: Vec<Arc<DynamicImage>> =
            (0..3).map(|i| Arc::new(image::open(format!("{pages_dir}/page_{i}.png")).unwrap())).collect();
        app.sixel = true;
        app.font = (10, 20);
        app.reader_area = Rect::new(0, 0, 120, 29);
        let chapter = chapters.iter().position(|c| c.chapter_number == 13.0).unwrap();
        let total = 6;
        app.reader = Some(Reader {
            source: source.clone(),
            manga: app.details.as_ref().unwrap().manga.clone(),
            manga_url: manga.url.clone(),
            chapters: chapters.clone(),
            chapter,
            pages: (0..total as i32).map(|i| Page { index: i, url: String::new(), image_url: None }).collect(),
            page: 0,
            requested: HashSet::new(),
            images: HashMap::new(),
            error: None,
            view: View::default(),
        });
        app.screen = Screen::Reader;
        let ensure_page = |app: &mut App| {
            let r = app.reader.as_mut().unwrap();
            let img = page_imgs[r.page % page_imgs.len()].clone();
            let encoded = render::encode(&img, 120, 29, (10, 20), View::default());
            r.images.insert(r.page, PageImage { img, encoded: Some((((120, 29), View::default()), encoded)), encoding: None, proto: None });
        };
        for page in 0..total {
            ensure_page(&mut app);
            shot(&mut app, if page == 0 { 1600 } else { 1100 });
            if page + 1 < total {
                key(&mut app, KeyCode::Right);
            }
        }
        // Última página: el capítulo queda leído y se envía a AniList
        let synced = anilist::SyncResult { entry: anilist::ListEntry { status: Some("CURRENT".into()), progress: Some(13) }, kept_remote_progress: false };
        app.update(Msg::AnilistSynced("1".into(), manga.url.clone(), Ok(synced)));
        ensure_page(&mut app);
        shot(&mut app, 1800);

        // 6. De vuelta a la lista: el 13 ya aparece como leído
        key(&mut app, KeyCode::Esc);
        shot(&mut app, 2600);

        std::fs::write(format!("{dir}/frames.json"), serde_json::to_vec_pretty(&frames).unwrap()).unwrap();
    }
}
