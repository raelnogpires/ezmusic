use std::{
    collections::HashSet,
    io::{self, Stdout},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use rand::Rng;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
    },
};

use crate::{
    config::{AppConfig, AppPaths},
    db::LibraryDb,
    download::{DownloadService, safe_component},
    model::{Album, DownloadEvent, DownloadRequest, SearchItem, SearchKind, Track},
    player::{AudioPlayer, PlaybackState, PlayerEvent},
    source::{MAX_COLLECTION_ITEMS, SourceProvider, YouTubeProvider, is_http_url},
    tools::{ToolKind, ToolManager},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Search,
    Library,
    Queue,
    Downloads,
    Playlists,
}

#[derive(Debug)]
enum InputMode {
    None,
    Search,
    LibraryFilter,
    Import,
    Playlist { track_id: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    One,
    All,
}

impl RepeatMode {
    fn cycle(self) -> Self {
        match self {
            Self::Off => Self::One,
            Self::One => Self::All,
            Self::All => Self::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "uma",
            Self::All => "todas",
        }
    }
}

enum BackgroundEvent {
    Search {
        label: String,
        direct_url: bool,
        result: Result<Vec<SearchItem>>,
    },
    Resolve {
        label: String,
        kind: SearchKind,
        result: Result<Vec<SearchItem>>,
    },
    DownloadTools(Result<(PathBuf, PathBuf, Vec<DownloadRequest>)>),
    Import(Result<usize>),
}

const MAX_SEARCH_INPUT_CHARS: usize = 512;
const MAX_PATH_INPUT_CHARS: usize = 4096;
const MAX_PLAYLIST_NAME_CHARS: usize = 128;
const MAX_JOB_HISTORY: usize = 512;
const MAX_SEARCH_HISTORY: usize = 8;

#[derive(Debug, Clone)]
struct JobView {
    id: String,
    title: String,
    state: String,
    error: Option<String>,
}

struct SearchPage {
    title: String,
    is_collection: bool,
    results: Vec<SearchItem>,
    selected: HashSet<usize>,
    index: usize,
}

struct AlbumDetail {
    album: Album,
    tracks: Vec<Track>,
    index: usize,
    parent_index: usize,
}

struct PlaylistDetail {
    id: i64,
    name: String,
    tracks: Vec<Track>,
    index: usize,
    parent_index: usize,
}

struct App {
    paths: AppPaths,
    config: AppConfig,
    db: LibraryDb,
    tools: ToolManager,
    downloads: Option<DownloadService>,
    player: AudioPlayer,
    background_tx: Sender<BackgroundEvent>,
    background_rx: Receiver<BackgroundEvent>,
    screen: Screen,
    input_mode: InputMode,
    input: String,
    status: String,
    busy: bool,
    show_notice: bool,
    search_title: String,
    search_is_collection: bool,
    search_history: Vec<SearchPage>,
    search_results: Vec<SearchItem>,
    selected_results: HashSet<usize>,
    search_index: usize,
    albums: Vec<Album>,
    library: Vec<Track>,
    library_index: usize,
    library_filter: String,
    album_detail: Option<AlbumDetail>,
    queue: Vec<Track>,
    queue_index: usize,
    current_index: Option<usize>,
    jobs: Vec<JobView>,
    job_index: usize,
    playlists: Vec<(i64, String, u64)>,
    playlist_index: usize,
    playlist_detail: Option<PlaylistDetail>,
    shuffle: bool,
    repeat: RepeatMode,
    show_help: bool,
    should_quit: bool,
}

impl App {
    fn new(paths: AppPaths, config: AppConfig, db: LibraryDb) -> Result<Self> {
        let (background_tx, background_rx) = unbounded();
        let tools = ToolManager::new(paths.clone());
        let albums = db.albums(None, 500)?;
        let library = db.standalone_tracks(None, 500, 0)?;
        let queue = db.load_queue()?;
        let playlists = db.playlists()?;
        let show_notice = !config.accepted_download_notice;
        let player = AudioPlayer::new(config.audio_device.clone());
        Ok(Self {
            paths,
            config,
            db,
            tools,
            downloads: None,
            player,
            background_tx,
            background_rx,
            screen: Screen::Library,
            input_mode: InputMode::None,
            input: String::new(),
            status: "Pronto. / busca musica, ? mostra ajuda.".into(),
            busy: false,
            show_notice,
            search_title: "RESULTADOS".into(),
            search_is_collection: false,
            search_history: Vec::new(),
            search_results: Vec::new(),
            selected_results: HashSet::new(),
            search_index: 0,
            albums,
            library,
            library_index: 0,
            library_filter: String::new(),
            album_detail: None,
            queue,
            queue_index: 0,
            current_index: None,
            jobs: Vec::new(),
            job_index: 0,
            playlists,
            playlist_index: 0,
            playlist_detail: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            show_help: false,
            should_quit: false,
        })
    }

    fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.background_rx.try_recv() {
            changed = true;
            self.handle_background(event);
        }
        while let Some(event) = self.downloads.as_ref().and_then(DownloadService::try_event) {
            changed = true;
            self.handle_download(event);
        }
        while let Some(event) = self.player.try_event() {
            changed = true;
            match event {
                PlayerEvent::Ended => self.on_track_ended(),
                PlayerEvent::Error(error) => {
                    self.player.stop();
                    self.current_index = None;
                    self.status = error;
                }
            }
        }
        changed
    }

    fn handle_background(&mut self, event: BackgroundEvent) {
        match event {
            BackgroundEvent::Search {
                label,
                direct_url,
                result,
            } => {
                self.busy = false;
                match result {
                    Ok(items) => {
                        let is_collection = direct_url && items.len() > 1;
                        self.search_history.clear();
                        self.search_title = short_label(&label, 48);
                        self.search_is_collection = is_collection;
                        self.search_results = items;
                        self.selected_results.clear();
                        self.search_index = 0;
                        self.screen = Screen::Search;
                        self.status = format!(
                            "{} resultado(s). Enter abre, A baixa album completo.",
                            self.search_results.len()
                        );
                    }
                    Err(error) => self.status = format!("Busca falhou: {error:#}"),
                }
            }
            BackgroundEvent::Resolve {
                label,
                kind,
                result,
            } => {
                self.busy = false;
                match result {
                    Ok(items) => {
                        self.save_search_page();
                        self.search_title = short_label(&label, 48);
                        self.search_is_collection =
                            matches!(kind, SearchKind::Album | SearchKind::Playlist)
                                || items.len() > 1;
                        self.search_results = items;
                        self.selected_results.clear();
                        self.search_index = 0;
                        self.screen = Screen::Search;
                        self.status = format!(
                            "Colecao aberta: {} faixa(s). A baixa tudo, Esc volta.",
                            self.search_results.len()
                        );
                    }
                    Err(error) => self.status = format!("Nao foi possivel abrir: {error:#}"),
                }
            }
            BackgroundEvent::DownloadTools(result) => {
                self.busy = false;
                match result {
                    Ok((yt_dlp, ffmpeg, requests)) => {
                        if self.downloads.is_none() {
                            match DownloadService::start(
                                yt_dlp,
                                ffmpeg,
                                self.config.library_path.clone(),
                                self.paths.cache_dir.clone(),
                                self.config.max_parallel_downloads,
                                self.config.opus_bitrate_kbps,
                            ) {
                                Ok(service) => self.downloads = Some(service),
                                Err(error) => {
                                    self.status = format!("Falha ao iniciar downloads: {error:#}");
                                    return;
                                }
                            }
                        }
                        for request in requests {
                            self.enqueue_request(request);
                        }
                        self.screen = Screen::Downloads;
                    }
                    Err(error) => self.status = format!("Bootstrap falhou: {error:#}"),
                }
            }
            BackgroundEvent::Import(result) => {
                self.busy = false;
                match result {
                    Ok(count) => {
                        self.refresh_library();
                        self.status = format!("{count} arquivo(s) indexado(s).");
                    }
                    Err(error) => self.status = format!("Importacao falhou: {error:#}"),
                }
            }
        }
    }

    fn save_search_page(&mut self) {
        if self.search_history.len() >= MAX_SEARCH_HISTORY {
            self.search_history.remove(0);
        }
        self.search_history.push(SearchPage {
            title: std::mem::take(&mut self.search_title),
            is_collection: self.search_is_collection,
            results: std::mem::take(&mut self.search_results),
            selected: std::mem::take(&mut self.selected_results),
            index: self.search_index,
        });
    }

    fn back_search(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let Some(page) = self.search_history.pop() else {
            self.status = "Nao ha uma pagina anterior nesta busca.".into();
            return;
        };
        self.search_title = page.title;
        self.search_is_collection = page.is_collection;
        self.search_results = page.results;
        self.selected_results = page.selected;
        self.search_index = page.index.min(self.search_results.len().saturating_sub(1));
        self.status = "Pagina anterior restaurada.".into();
    }

    fn handle_download(&mut self, event: DownloadEvent) {
        let job_id = event.job_id().to_string();
        match event {
            DownloadEvent::Queued { title, .. } => {
                if self.jobs.len() >= MAX_JOB_HISTORY {
                    self.jobs.remove(0);
                    self.job_index = self.job_index.saturating_sub(1);
                }
                self.jobs.push(JobView {
                    id: job_id.clone(),
                    title: title.clone(),
                    state: "na fila".into(),
                    error: None,
                });
                let _ = self.db.record_job(&job_id, &title, "queued", None);
            }
            DownloadEvent::Downloading { .. } => {
                self.update_job(&job_id, "baixando", None);
            }
            DownloadEvent::Converting { current, total, .. } => {
                self.update_job(&job_id, &format!("convertendo {current}/{total}"), None);
            }
            DownloadEvent::Completed { tracks, album, .. } => match album.as_ref().map_or_else(
                || {
                    tracks
                        .iter()
                        .try_for_each(|track| self.db.upsert_track(track).map(|_| ()))
                },
                |album| self.db.upsert_album(album, &tracks).map(|_| ()),
            ) {
                Ok(_) => {
                    self.update_job(&job_id, "concluido", None);
                    self.refresh_library();
                }
                Err(error) => {
                    self.update_job(&job_id, "erro", Some(format!("{error:#}")));
                }
            },
            DownloadEvent::Cancelled { .. } => self.update_job(&job_id, "cancelado", None),
            DownloadEvent::Failed { error, .. } => {
                self.update_job(&job_id, "erro", Some(error));
            }
        }
    }

    fn update_job(&mut self, id: &str, state: &str, error: Option<String>) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            job.state = state.into();
            job.error = error.clone();
            let _ = self.db.record_job(id, &job.title, state, error.as_deref());
        }
    }

    fn refresh_library(&mut self) {
        let filter = (!self.library_filter.is_empty()).then_some(self.library_filter.as_str());
        match (
            self.db.albums(filter, 500),
            self.db.standalone_tracks(filter, 500, 0),
        ) {
            (Ok(albums), Ok(tracks)) => {
                self.albums = albums;
                self.library = tracks;
                self.library_index = self.library_index.min(self.library_len().saturating_sub(1));
            }
            (Err(error), _) | (_, Err(error)) => {
                self.status = format!("Falha ao ler biblioteca: {error:#}")
            }
        }
        if let Some(detail) = &mut self.album_detail {
            match self.db.album_tracks(detail.album.id) {
                Ok(tracks) => {
                    detail.tracks = tracks;
                    detail.index = detail.index.min(detail.tracks.len().saturating_sub(1));
                }
                Err(error) => self.status = format!("Falha ao ler album: {error:#}"),
            }
        }
        self.playlists = self.db.playlists().unwrap_or_default();
    }

    fn apply_library_filter(&mut self) {
        self.library_filter = self.input.trim().to_string();
        self.input.clear();
        self.input_mode = InputMode::None;
        self.library_index = 0;
        self.album_detail = None;
        self.refresh_library();
        self.status = if self.library_filter.is_empty() {
            "Filtro da biblioteca removido.".into()
        } else {
            format!("Biblioteca filtrada por: {}", self.library_filter)
        };
    }

    fn library_len(&self) -> usize {
        self.albums.len() + self.library.len()
    }

    fn selected_library_track(&self) -> Option<&Track> {
        self.library
            .get(self.library_index.saturating_sub(self.albums.len()))
            .filter(|_| self.library_index >= self.albums.len())
    }

    fn open_library_selection(&mut self) {
        if let Some(album) = self.albums.get(self.library_index).cloned() {
            match self.db.album_tracks(album.id) {
                Ok(tracks) => {
                    self.album_detail = Some(AlbumDetail {
                        album,
                        tracks,
                        index: 0,
                        parent_index: self.library_index,
                    });
                    self.status = "Album aberto. Enter toca a faixa; A toca o album.".into();
                }
                Err(error) => self.status = format!("Falha ao abrir album: {error:#}"),
            }
        } else {
            self.play_library();
        }
    }

    fn close_album(&mut self) {
        if let Some(detail) = self.album_detail.take() {
            self.library_index = detail
                .parent_index
                .min(self.library_len().saturating_sub(1));
            self.status = "Biblioteca restaurada.".into();
        }
    }

    fn play_album(&mut self, from_selection: bool) {
        let Some(detail) = &self.album_detail else {
            return;
        };
        let selected_id = from_selection
            .then(|| detail.tracks.get(detail.index))
            .flatten()
            .map(|track| track.id);
        if from_selection
            && detail
                .tracks
                .get(detail.index)
                .is_some_and(|track| !track.available)
        {
            self.status = "Arquivo indisponivel. Restaure o caminho para tocar esta faixa.".into();
            return;
        }
        let tracks = detail.tracks.clone();
        self.start_queue(tracks, selected_id);
    }

    fn start_queue(&mut self, tracks: Vec<Track>, selected_id: Option<i64>) {
        let available: Vec<_> = tracks.into_iter().filter(|track| track.available).collect();
        if available.is_empty() {
            self.status = "Nenhuma faixa disponivel nesta colecao.".into();
            return;
        }
        self.queue = available;
        self.current_index = selected_id
            .and_then(|id| self.queue.iter().position(|track| track.id == id))
            .or(Some(0));
        self.queue_index = self.current_index.unwrap_or(0);
        self.persist_queue();
        self.play_current();
    }

    fn submit_search(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let query = self.input.trim().to_string();
        if query.is_empty() {
            self.status = "Digite artista, faixa, album ou URL.".into();
            return;
        }
        self.input.clear();
        self.input_mode = InputMode::None;
        self.busy = true;
        self.status = "Preparando busca...".into();
        let tools = self.tools.clone();
        let sender = self.background_tx.clone();
        let label = query.clone();
        let direct_url = is_http_url(&query);
        thread::spawn(move || {
            let result = tools
                .ensure(ToolKind::YtDlp)
                .and_then(|path| YouTubeProvider::new(path).search(&query, 20));
            let _ = sender.send(BackgroundEvent::Search {
                label,
                direct_url,
                result,
            });
        });
    }

    fn resolve_current(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let Some(item) = self.search_results.get(self.search_index).cloned() else {
            return;
        };
        self.busy = true;
        self.status = format!("Abrindo {}...", item.title);
        let tools = self.tools.clone();
        let sender = self.background_tx.clone();
        let label = item.title.clone();
        let kind = item.kind;
        thread::spawn(move || {
            let result = tools
                .ensure(ToolKind::YtDlp)
                .and_then(|path| YouTubeProvider::new(path).resolve(&item.url));
            let _ = sender.send(BackgroundEvent::Resolve {
                label,
                kind,
                result,
            });
        });
    }

    fn start_selected_downloads(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let items: Vec<_> = if self.selected_results.is_empty() {
            self.search_results
                .get(self.search_index)
                .cloned()
                .into_iter()
                .collect()
        } else {
            self.selected_results
                .iter()
                .filter_map(|index| self.search_results.get(*index).cloned())
                .collect()
        };
        if items.is_empty() {
            self.status = "Nenhum resultado selecionado.".into();
            return;
        }
        self.start_download_items(items);
    }

    fn download_full_collection(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let Some(items) = self.full_collection_items() else {
            self.status =
                "Abra um album/playlist ou selecione um resultado desse tipo para baixar tudo."
                    .into();
            return;
        };
        let count = items.len();
        self.status = format!("Preparando colecao completa ({count} item(ns))...");
        self.start_download_items(items);
    }

    fn full_collection_items(&self) -> Option<Vec<SearchItem>> {
        if self.search_is_collection && !self.search_results.is_empty() {
            return Some(self.search_results.clone());
        }
        self.search_results
            .get(self.search_index)
            .filter(|item| matches!(item.kind, SearchKind::Album | SearchKind::Playlist))
            .cloned()
            .map(|item| vec![item])
    }

    fn start_download_items(&mut self, mut items: Vec<SearchItem>) {
        if items.len() > MAX_COLLECTION_ITEMS {
            self.status = format!("Selecao excede o limite de {MAX_COLLECTION_ITEMS} faixas.");
            return;
        }
        let mut selected = HashSet::new();
        items.retain(|item| selected.insert((item.provider.clone(), item.source_id.clone())));
        self.busy = true;
        self.status = format!(
            "Resolvendo e preparando {} item(ns) para download...",
            items.len()
        );
        let tools = self.tools.clone();
        let sender = self.background_tx.clone();
        thread::spawn(move || {
            let result = tools.ensure(ToolKind::YtDlp).and_then(|yt_dlp| {
                let provider = YouTubeProvider::new(yt_dlp.clone());
                let mut resolved = Vec::new();
                for item in items {
                    if item.segment.is_some() || item.album_identity.is_some() {
                        resolved.push(item);
                    } else {
                        let collection = provider.resolve(&item.url)?;
                        if resolved.len().saturating_add(collection.len()) > MAX_COLLECTION_ITEMS {
                            anyhow::bail!(
                                "selecao excede o limite de {MAX_COLLECTION_ITEMS} faixas"
                            );
                        }
                        resolved.extend(collection);
                    }
                }
                let mut seen = HashSet::new();
                resolved
                    .retain(|item| seen.insert((item.provider.clone(), item.source_id.clone())));
                let requests = download_requests(resolved);
                tools
                    .ensure(ToolKind::Ffmpeg)
                    .map(|ffmpeg| (yt_dlp, ffmpeg, requests))
            });
            let _ = sender.send(BackgroundEvent::DownloadTools(result));
        });
    }

    fn enqueue_request(&mut self, request: DownloadRequest) {
        let job_id = request.job_id.clone();
        if let Some(index) = self.jobs.iter().position(|job| job.id == job_id) {
            if matches!(self.jobs[index].state.as_str(), "na fila" | "baixando")
                || self.jobs[index].state.starts_with("convertendo")
            {
                return;
            }
            self.jobs.remove(index);
        }
        let Some(downloads) = &self.downloads else {
            self.status = "Servico de downloads indisponivel.".into();
            return;
        };
        let title = request
            .album
            .as_ref()
            .map(|album| album.title.clone())
            .or_else(|| request.items.first().map(|item| item.title.clone()))
            .unwrap_or_else(|| "Download".into());
        match downloads.enqueue(request) {
            Ok(()) => self.handle_download(DownloadEvent::Queued { job_id, title }),
            Err(error) => self.status = format!("Falha ao enfileirar: {error:#}"),
        }
    }

    fn submit_import(&mut self) {
        if self.busy {
            self.status = "Aguarde a operacao atual terminar.".into();
            return;
        }
        let root = PathBuf::from(self.input.trim());
        self.input.clear();
        self.input_mode = InputMode::None;
        if root.as_os_str().is_empty() {
            return;
        }
        self.busy = true;
        self.status = format!("Indexando {}...", root.display());
        let database = self.paths.database_file.clone();
        let sender = self.background_tx.clone();
        thread::spawn(move || {
            let result = LibraryDb::open(database).and_then(|db| db.scan_import_root(&root));
            let _ = sender.send(BackgroundEvent::Import(result));
        });
    }

    fn submit_playlist(&mut self, track_id: i64) {
        let name = self.input.trim().to_string();
        self.input.clear();
        self.input_mode = InputMode::None;
        if name.is_empty() {
            return;
        }
        match self
            .db
            .create_playlist(&name)
            .and_then(|playlist| self.db.add_to_playlist(playlist, track_id))
        {
            Ok(()) => {
                self.playlists = self.db.playlists().unwrap_or_default();
                self.status = format!("Adicionada a playlist {name}.");
            }
            Err(error) => self.status = format!("Falha na playlist: {error:#}"),
        }
    }

    fn open_playlist(&mut self) {
        let Some((id, name, _)) = self.playlists.get(self.playlist_index).cloned() else {
            return;
        };
        match self.db.playlist_tracks(id) {
            Ok(tracks) => {
                self.playlist_detail = Some(PlaylistDetail {
                    id,
                    name,
                    tracks,
                    index: 0,
                    parent_index: self.playlist_index,
                });
                self.status = "Playlist aberta. Enter toca a faixa; A toca desde o inicio.".into();
            }
            Err(error) => self.status = format!("Falha ao abrir playlist: {error:#}"),
        }
    }

    fn close_playlist(&mut self) {
        if let Some(detail) = self.playlist_detail.take() {
            self.playlist_index = detail
                .parent_index
                .min(self.playlists.len().saturating_sub(1));
            self.status = "Lista de playlists restaurada.".into();
        }
    }

    fn play_playlist(&mut self, from_selection: bool) {
        let Some(detail) = &self.playlist_detail else {
            return;
        };
        let selected = from_selection
            .then(|| detail.tracks.get(detail.index))
            .flatten();
        if selected.is_some_and(|track| !track.available) {
            self.status = "Arquivo indisponivel. Restaure o caminho para tocar esta faixa.".into();
            return;
        }
        let selected_id = selected.map(|track| track.id);
        self.start_queue(detail.tracks.clone(), selected_id);
    }

    fn remove_playlist_track(&mut self) {
        let Some(detail) = &self.playlist_detail else {
            return;
        };
        let playlist_id = detail.id;
        let position = detail.index;
        match self.db.remove_from_playlist(playlist_id, position) {
            Ok(true) => match self.db.playlist_tracks(playlist_id) {
                Ok(tracks) => {
                    if let Some(detail) = &mut self.playlist_detail {
                        detail.tracks = tracks;
                        detail.index = detail.index.min(detail.tracks.len().saturating_sub(1));
                    }
                    self.playlists = self.db.playlists().unwrap_or_default();
                    self.status =
                        "Faixa removida da playlist; biblioteca e fila preservadas.".into();
                }
                Err(error) => self.status = format!("Falha ao atualizar playlist: {error:#}"),
            },
            Ok(false) => self.status = "A faixa selecionada nao existe mais na playlist.".into(),
            Err(error) => self.status = format!("Falha ao remover da playlist: {error:#}"),
        }
    }

    fn play_library(&mut self) {
        let Some(selected) = self.selected_library_track().cloned() else {
            return;
        };
        if !selected.available {
            self.status = "Arquivo indisponivel. Importe novamente ou restaure o caminho.".into();
            return;
        }
        let current_id = self
            .current_index
            .and_then(|index| self.queue.get(index))
            .map(|track| track.id);
        if self.player.is_active() && current_id == Some(selected.id) {
            self.toggle_playback();
            return;
        }
        let selected_id = selected.id;
        self.start_queue(self.library.clone(), Some(selected_id));
    }

    fn activate_queue_track(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        if self.player.is_active() && self.current_index == Some(index) {
            self.toggle_playback();
            return;
        }
        self.current_index = Some(index);
        self.queue_index = index;
        self.play_current();
    }

    fn play_current(&mut self) {
        let Some(index) = self.current_index else {
            return;
        };
        let Some(track) = self.queue.get(index) else {
            return;
        };
        match self.player.play(&track.path) {
            Ok(()) => {
                self.status = format!("Tocando: {} - {}", track.artist, track.title);
            }
            Err(error) => {
                self.current_index = None;
                self.status = format!("Nao foi possivel tocar: {error:#}");
            }
        }
    }

    fn toggle_playback(&mut self) {
        self.status = match self.player.toggle_pause() {
            Some(PlaybackState::Paused) => "Reproducao pausada.".into(),
            Some(PlaybackState::Playing) => "Reproducao retomada.".into(),
            Some(PlaybackState::Stopped) | None => "Nenhuma musica esta tocando.".into(),
        };
    }

    fn stop_playback(&mut self) {
        if self.player.is_active() {
            self.player.stop();
            self.current_index = None;
            self.status = "Reproducao parada.".into();
        } else {
            self.status = "Nenhuma musica esta tocando.".into();
        }
    }

    fn next_track(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let next = if self.shuffle && self.queue.len() > 1 {
            rand::rng().random_range(0..self.queue.len())
        } else {
            match self.current_index {
                Some(index) if index + 1 < self.queue.len() => index + 1,
                None => 0,
                _ if self.repeat == RepeatMode::All => 0,
                _ => {
                    self.stop_playback();
                    return;
                }
            }
        };
        self.current_index = Some(next);
        self.queue_index = next;
        self.play_current();
    }

    fn previous_track(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let previous = self.current_index.unwrap_or(0).saturating_sub(1);
        self.current_index = Some(previous);
        self.queue_index = previous;
        self.play_current();
    }

    fn on_track_ended(&mut self) {
        if self.repeat == RepeatMode::One {
            self.play_current();
        } else {
            self.next_track();
        }
    }

    fn persist_queue(&mut self) {
        if let Err(error) = self.db.save_queue(&self.queue) {
            self.status = format!("Falha ao salvar fila: {error:#}");
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.show_notice {
            match key.code {
                KeyCode::Enter => {
                    self.config.accepted_download_notice = true;
                    if let Err(error) = self.config.save(&self.paths) {
                        self.status = format!("Falha ao salvar aceite: {error:#}");
                    } else {
                        self.show_notice = false;
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                _ => {}
            }
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => self.show_help = false,
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
            return;
        }
        if !matches!(self.input_mode, InputMode::None) {
            self.input_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('1') => self.screen = Screen::Search,
            KeyCode::Char('2') => self.screen = Screen::Library,
            KeyCode::Char('3') => self.screen = Screen::Queue,
            KeyCode::Char('4') => self.screen = Screen::Downloads,
            KeyCode::Char('5') => self.screen = Screen::Playlists,
            KeyCode::Char('/') => {
                self.input_mode = if self.screen == Screen::Library {
                    InputMode::LibraryFilter
                } else {
                    InputMode::Search
                };
                self.input.clear();
            }
            KeyCode::Char('I') => {
                self.input_mode = InputMode::Import;
                self.input.clear();
            }
            KeyCode::Char(' ') if self.screen != Screen::Search => self.toggle_playback(),
            KeyCode::Char('p') => self.toggle_playback(),
            KeyCode::Char('z') => self.stop_playback(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('b') => self.previous_track(),
            KeyCode::Left => self.player.seek_relative(-5),
            KeyCode::Right => self.player.seek_relative(5),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.player.set_volume(self.player.volume() + 0.05)
            }
            KeyCode::Char('-') => self.player.set_volume(self.player.volume() - 0.05),
            KeyCode::Char('s') => {
                self.shuffle = !self.shuffle;
                self.status = format!("Shuffle: {}.", if self.shuffle { "on" } else { "off" });
            }
            KeyCode::Char('r') => {
                self.repeat = self.repeat.cycle();
                self.status = format!("Repeat: {}.", self.repeat.label());
            }
            KeyCode::Esc | KeyCode::Backspace if self.screen == Screen::Search => {
                self.back_search()
            }
            KeyCode::Esc | KeyCode::Backspace
                if self.screen == Screen::Library && self.album_detail.is_some() =>
            {
                self.close_album()
            }
            KeyCode::Esc | KeyCode::Backspace
                if self.screen == Screen::Playlists && self.playlist_detail.is_some() =>
            {
                self.close_playlist()
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => self.screen_key(key),
        }
    }

    fn input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::None;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => match self.input_mode {
                InputMode::Search => self.submit_search(),
                InputMode::LibraryFilter => self.apply_library_filter(),
                InputMode::Import => self.submit_import(),
                InputMode::Playlist { track_id } => self.submit_playlist(track_id),
                InputMode::None => {}
            },
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.input.chars().count() < self.input_limit() =>
            {
                self.input.push(character);
            }
            _ => {}
        }
    }

    fn screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Search => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.search_index = self.search_index.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.search_index = next_index(self.search_index, self.search_results.len())
                }
                KeyCode::Char('x') | KeyCode::Char(' ') => {
                    if !self.selected_results.remove(&self.search_index) {
                        self.selected_results.insert(self.search_index);
                    }
                }
                KeyCode::Char('a') => {
                    self.selected_results = (0..self.search_results.len()).collect()
                }
                KeyCode::Char('A') => self.download_full_collection(),
                KeyCode::Enter => self.resolve_current(),
                KeyCode::Char('d') => self.start_selected_downloads(),
                _ => {}
            },
            Screen::Library => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(detail) = &mut self.album_detail {
                        detail.index = detail.index.saturating_sub(1);
                    } else {
                        self.library_index = self.library_index.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(detail) = &mut self.album_detail {
                        detail.index = next_index(detail.index, detail.tracks.len());
                    } else {
                        self.library_index = next_index(self.library_index, self.library_len());
                    }
                }
                KeyCode::Enter => {
                    if self.album_detail.is_some() {
                        self.play_album(true);
                    } else {
                        self.open_library_selection();
                    }
                }
                KeyCode::Char('A') if self.album_detail.is_some() => self.play_album(false),
                KeyCode::Char('P') => {
                    let track = self
                        .album_detail
                        .as_ref()
                        .and_then(|detail| detail.tracks.get(detail.index))
                        .or_else(|| self.selected_library_track());
                    if let Some(track) = track {
                        self.input_mode = InputMode::Playlist { track_id: track.id };
                        self.input.clear();
                    }
                }
                _ => {}
            },
            Screen::Queue => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.queue_index = self.queue_index.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.queue_index = next_index(self.queue_index, self.queue.len())
                }
                KeyCode::Enter => {
                    self.activate_queue_track(self.queue_index);
                }
                KeyCode::Delete | KeyCode::Char('x') => {
                    if self.queue_index < self.queue.len() {
                        let removed = self.queue_index;
                        if self.current_index == Some(removed) {
                            self.stop_playback();
                        }
                        self.queue.remove(removed);
                        self.current_index = match self.current_index {
                            Some(current) if current == removed => None,
                            Some(current) if current > removed => Some(current - 1),
                            other => other,
                        };
                        self.queue_index = self.queue_index.min(self.queue.len().saturating_sub(1));
                        self.persist_queue();
                    }
                }
                _ => {}
            },
            Screen::Downloads => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.job_index = self.job_index.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.job_index = next_index(self.job_index, self.jobs.len())
                }
                KeyCode::Char('c') => {
                    if let (Some(service), Some(job)) =
                        (&self.downloads, self.jobs.get(self.job_index))
                        && service.cancel(&job.id)
                    {
                        self.status = format!("Cancelando {}...", job.title);
                    }
                }
                _ => {}
            },
            Screen::Playlists => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(detail) = &mut self.playlist_detail {
                        detail.index = detail.index.saturating_sub(1);
                    } else {
                        self.playlist_index = self.playlist_index.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(detail) = &mut self.playlist_detail {
                        detail.index = next_index(detail.index, detail.tracks.len());
                    } else {
                        self.playlist_index = next_index(self.playlist_index, self.playlists.len());
                    }
                }
                KeyCode::Enter => {
                    if self.playlist_detail.is_some() {
                        self.play_playlist(true);
                    } else {
                        self.open_playlist();
                    }
                }
                KeyCode::Char('A') if self.playlist_detail.is_some() => self.play_playlist(false),
                KeyCode::Delete | KeyCode::Char('x') if self.playlist_detail.is_some() => {
                    self.remove_playlist_track()
                }
                _ => {}
            },
        }
    }

    fn input_limit(&self) -> usize {
        match self.input_mode {
            InputMode::Search | InputMode::LibraryFilter => MAX_SEARCH_INPUT_CHARS,
            InputMode::Import => MAX_PATH_INPUT_CHARS,
            InputMode::Playlist { .. } => MAX_PLAYLIST_NAME_CHARS,
            InputMode::None => 0,
        }
    }
}

pub fn run(paths: AppPaths, config: AppConfig, db: LibraryDb) -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let app = App::new(paths, config, db)?;
    run_loop(&mut session.terminal, app)
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mut app: App) -> Result<()> {
    let mut dirty = true;
    let mut next_periodic_draw = Instant::now();
    while !app.should_quit {
        dirty |= app.tick();
        let now = Instant::now();
        if dirty || now >= next_periodic_draw {
            terminal.draw(|frame| render(frame, &app))?;
            dirty = false;
            next_periodic_draw = now + Duration::from_secs(1);
        }
        let timeout = next_periodic_draw
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        if event::poll(timeout)? {
            dirty |= dispatch_terminal_event(&mut app, event::read()?);
        }
    }
    app.persist_queue();
    Ok(())
}

fn dispatch_terminal_event(app: &mut App, event: Event) -> bool {
    if let Event::Key(key) = event
        && key.kind != KeyEventKind::Release
    {
        app.key(key);
        return true;
    }
    false
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("falha ao ativar terminal raw")?;
        let result = (|| {
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen)?;
            let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
            terminal.clear()?;
            Ok(Self { terminal })
        })();
        if result.is_err() {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

const ACCENT: Color = Color::Rgb(109, 226, 191);
const TEXT: Color = Color::Rgb(226, 232, 240);
const MUTED: Color = Color::Rgb(123, 139, 155);
const BORDER: Color = Color::Rgb(55, 65, 77);
const SURFACE: Color = Color::Rgb(22, 27, 34);
const SELECTION: Color = Color::Rgb(35, 46, 52);

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(6),
            Constraint::Length(2),
        ])
        .split(area);
    render_header(frame, chunks[0], app);
    render_workspace(frame, chunks[1], app);
    render_player(frame, chunks[2], app);
    render_status(frame, chunks[3], app);
    if app.show_notice {
        render_notice(frame);
    } else if app.show_help {
        render_help(frame);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let tabs = [
        ("1", "SEARCH", Screen::Search),
        ("2", "LIBRARY", Screen::Library),
        ("3", "QUEUE", Screen::Queue),
        ("4", "TRANSFERS", Screen::Downloads),
        ("5", "PLAYLISTS", Screen::Playlists),
    ];
    let mut navigation = Vec::new();
    for (key, label, screen) in tabs {
        let active = screen == app.screen;
        navigation.push(Span::styled(
            format!(" {key} {label} "),
            if active {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            },
        ));
        navigation.push(Span::raw(" "));
    }
    if (!app.search_history.is_empty() && app.screen == Screen::Search)
        || app.screen == Screen::Library && app.album_detail.is_some()
        || app.screen == Screen::Playlists && app.playlist_detail.is_some()
    {
        navigation.push(Span::styled(
            " ← ESC BACK ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " EZMUSIC // ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("LOCAL AUDIO RESEARCH CONSOLE", Style::default().fg(MUTED)),
            ]),
            Line::from(navigation),
        ])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn render_workspace(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= 104 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(60), Constraint::Length(31)])
            .split(area);
        render_screen(frame, columns[0], app);
        render_context(frame, columns[1], app);
    } else {
        render_screen(frame, area, app);
    }
}

fn render_screen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.screen {
        Screen::Search => render_search(frame, area, app),
        Screen::Library => render_library(frame, area, app),
        Screen::Queue => render_queue(frame, area, app),
        Screen::Downloads => render_downloads(frame, area, app),
        Screen::Playlists => render_playlists(frame, area, app),
    }
}

fn render_search(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app
        .search_results
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let selected = if app.selected_results.contains(&index) {
                "◆"
            } else {
                "◇"
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{selected} "), Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{:<8}", item.kind.label().to_uppercase()),
                    Style::default().fg(MUTED),
                ),
                Span::styled(item.title.clone(), Style::default().fg(TEXT)),
                Span::styled(format!("  /  {}", item.artist), Style::default().fg(MUTED)),
                Span::styled(
                    format!("  {}", duration_label(item.duration_seconds)),
                    Style::default().fg(MUTED),
                ),
            ]))
        })
        .collect();
    render_list(
        frame,
        area,
        format!(
            " SEARCH / {} / {} ITEMS ",
            app.search_title.to_uppercase(),
            app.search_results.len()
        ),
        "[/] query  [enter] open  [d] selected  [A] full album  [esc] back",
        items,
        app.search_index,
    );
}

fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let current_id = app
        .current_index
        .and_then(|index| app.queue.get(index))
        .map(|track| track.id);
    if let Some(detail) = &app.album_detail {
        let items = detail
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let marker = track_marker(app, track, current_id);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker} {:>3}  ", index + 1),
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(track.title.clone(), Style::default().fg(TEXT)),
                    Span::styled(format!("  /  {}", track.artist), Style::default().fg(MUTED)),
                    Span::styled(
                        format!("  {}", duration_label(track.duration_seconds)),
                        Style::default().fg(MUTED),
                    ),
                ]))
            })
            .collect();
        render_list(
            frame,
            area,
            format!(
                " LIBRARY / ALBUM / {} / {} TRACKS ",
                detail.album.title.to_uppercase(),
                detail.tracks.len()
            ),
            "[enter] play selected   [A] play album   [P] playlist   [esc] library",
            items,
            detail.index,
        );
        return;
    }

    let mut items: Vec<ListItem<'_>> = app
        .albums
        .iter()
        .map(|album| {
            let marker = if album.available_count < album.track_count {
                "!"
            } else {
                "▣"
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} ALBUM  "), Style::default().fg(ACCENT)),
                Span::styled(album.title.clone(), Style::default().fg(TEXT)),
                Span::styled(format!("  /  {}", album.artist), Style::default().fg(MUTED)),
                Span::styled(
                    format!("  ·  {} tracks", album.track_count),
                    Style::default().fg(MUTED),
                ),
            ]))
        })
        .collect();
    items.extend(app.library.iter().enumerate().map(|(index, track)| {
        let marker = track_marker(app, track, current_id);
        ListItem::new(Line::from(vec![
            Span::styled(
                format!("{marker} TRACK {:>3}  ", index + 1),
                Style::default().fg(ACCENT),
            ),
            Span::styled(track.title.clone(), Style::default().fg(TEXT)),
            Span::styled(format!("  /  {}", track.artist), Style::default().fg(MUTED)),
        ]))
    }));
    render_list(
        frame,
        area,
        format!(
            " LIBRARY / {} ALBUMS + {} TRACKS ",
            app.albums.len(),
            app.library.len()
        ),
        "[/] filter   [enter] open/play   [P] playlist   [I] import folder",
        items,
        app.library_index,
    );
}

fn track_marker(app: &App, track: &Track, current_id: Option<i64>) -> &'static str {
    if current_id == Some(track.id) {
        match app.player.state() {
            PlaybackState::Paused => "Ⅱ",
            PlaybackState::Playing => "▶",
            PlaybackState::Stopped => " ",
        }
    } else if track.available {
        " "
    } else {
        "!"
    }
}

fn render_queue(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app
        .queue
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let marker = if Some(index) == app.current_index {
                match app.player.state() {
                    PlaybackState::Paused => "Ⅱ",
                    PlaybackState::Playing => "▶",
                    PlaybackState::Stopped => " ",
                }
            } else {
                " "
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{marker} {:>3}  ", index + 1),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(track.title.clone(), Style::default().fg(TEXT)),
                Span::styled(format!("  /  {}", track.artist), Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    render_list(
        frame,
        area,
        format!(" PLAY QUEUE / {} TRACKS ", app.queue.len()),
        "[enter] play or pause current   [x] remove   [b/n] previous/next",
        items,
        app.queue_index,
    );
}

fn render_downloads(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app
        .jobs
        .iter()
        .map(|job| {
            let state_style = match job.state.as_str() {
                "concluido" => Style::default().fg(ACCENT),
                "erro" => Style::default().fg(Color::LightRed),
                _ => Style::default().fg(MUTED),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<12}", job.state.to_uppercase()), state_style),
                Span::styled(job.title.clone(), Style::default().fg(TEXT)),
                Span::styled(
                    job.error
                        .as_deref()
                        .map(|error| format!("  /  {error}"))
                        .unwrap_or_default(),
                    Style::default().fg(Color::LightRed),
                ),
            ]))
        })
        .collect();
    render_list(
        frame,
        area,
        format!(" TRANSFERS / {} JOBS ", app.jobs.len()),
        &format!(
            "[c] cancel   {} network worker(s)   1 conversion thread",
            app.config.max_parallel_downloads
        ),
        items,
        app.job_index,
    );
}

fn render_playlists(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(detail) = &app.playlist_detail {
        let current_id = app
            .current_index
            .and_then(|index| app.queue.get(index))
            .map(|track| track.id);
        let items = if detail.tracks.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "  Playlist vazia.",
                Style::default().fg(MUTED),
            )))]
        } else {
            detail
                .tracks
                .iter()
                .enumerate()
                .map(|(index, track)| {
                    let marker = track_marker(app, track, current_id);
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{marker} {:>3}  ", index + 1),
                            Style::default().fg(ACCENT),
                        ),
                        Span::styled(track.title.clone(), Style::default().fg(TEXT)),
                        Span::styled(format!("  /  {}", track.artist), Style::default().fg(MUTED)),
                    ]))
                })
                .collect()
        };
        render_list(
            frame,
            area,
            format!(
                " PLAYLIST / {} / {} TRACKS ",
                detail.name.to_uppercase(),
                detail.tracks.len()
            ),
            "[enter] play selected   [A] play all   [x/delete] remove   [esc] playlists",
            items,
            detail.index,
        );
        return;
    }
    let items = app
        .playlists
        .iter()
        .enumerate()
        .map(|(index, (_, name, count))| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>3}  ", index + 1), Style::default().fg(ACCENT)),
                Span::styled(name.clone(), Style::default().fg(TEXT)),
                Span::styled(format!("  /  {count} tracks"), Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    render_list(
        frame,
        area,
        format!(" PLAYLISTS / {} ", app.playlists.len()),
        "[enter] open   use [P] in library or album to add a track",
        items,
        app.playlist_index,
    );
}

fn render_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: String,
    hint: &str,
    items: Vec<ListItem<'_>>,
    selected: usize,
) {
    let has_items = !items.is_empty();
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(SELECTION)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(has_items.then_some(selected));
    frame.render_stateful_widget(list, rows[0], &mut state);
    frame.render_widget(
        Paragraph::new(format!("  {hint}")).style(Style::default().fg(MUTED)),
        rows[1],
    );
}

fn render_context(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let active_jobs = app
        .jobs
        .iter()
        .filter(|job| {
            matches!(job.state.as_str(), "na fila" | "baixando")
                || job.state.starts_with("convertendo")
        })
        .count();
    let library_tracks = app.library.len()
        + app
            .albums
            .iter()
            .map(|album| album.track_count as usize)
            .sum::<usize>();
    let lines = vec![
        Line::from(Span::styled("SYSTEM", Style::default().fg(ACCENT))),
        Line::from(""),
        metric_line("library", library_tracks.to_string()),
        metric_line("queue", app.queue.len().to_string()),
        metric_line("active jobs", active_jobs.to_string()),
        metric_line("audio underruns", app.player.underflows().to_string()),
        Line::from(""),
        Line::from(Span::styled("CONTROL SURFACE", Style::default().fg(ACCENT))),
        Line::from(""),
        Line::from(vec![
            key_span("SPACE/P"),
            Span::styled(" pause / resume", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("Z"),
            Span::styled(" stop", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("B/N"),
            Span::styled(" previous / next", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("←/→"),
            Span::styled(" seek 5 seconds", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("+/-"),
            Span::styled(" volume", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("?"),
            Span::styled(" command map", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("ESC"),
            Span::styled(" back from collection", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            key_span("A"),
            Span::styled(" full album", Style::default().fg(MUTED)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel(" SESSION / LIVE ".into())),
        area,
    );
}

fn render_player(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let track = app.current_index.and_then(|index| app.queue.get(index));
    let state = app.player.state();
    let (state_label, state_symbol, state_style) = match state {
        PlaybackState::Playing => ("PLAYING", "▶", Style::default().fg(ACCENT)),
        PlaybackState::Paused => ("PAUSED", "Ⅱ", Style::default().fg(Color::LightYellow)),
        PlaybackState::Stopped => ("STOPPED", "■", Style::default().fg(MUTED)),
    };
    let block = panel(format!(" NOW PLAYING / {state_label} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let title = track
        .map(|track| track.title.as_str())
        .unwrap_or("No active track");
    let artist = track.map(|track| track.artist.as_str()).unwrap_or("");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {state_symbol} "),
                state_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if artist.is_empty() {
                    String::new()
                } else {
                    format!("  /  {artist}")
                },
                Style::default().fg(MUTED),
            ),
        ])),
        rows[0],
    );
    let position = app.player.position().as_secs();
    let duration = track.and_then(|track| track.duration_seconds);
    let ratio = duration
        .filter(|duration| *duration > 0)
        .map(|duration| position.min(duration) as f64 / duration as f64)
        .unwrap_or(0.0);
    let time_label = format!(
        "{} / {}",
        duration_label(Some(position)),
        duration_label(duration)
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ACCENT).bg(SURFACE))
            .ratio(ratio)
            .label(time_label),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            " volume {:>3}%   shuffle {:<3}   repeat {:<5}   underruns {}",
            (app.player.volume() * 100.0).round() as u8,
            if app.shuffle { "on" } else { "off" },
            app.repeat.label(),
            app.player.underflows()
        ))
        .style(Style::default().fg(MUTED)),
        rows[2],
    );
    let action = if state == PlaybackState::Paused {
        "RESUME"
    } else {
        "PAUSE"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            key_span("SPACE/P"),
            Span::styled(format!(" {action}   "), Style::default().fg(TEXT)),
            key_span("Z"),
            Span::styled(" STOP   ", Style::default().fg(TEXT)),
            key_span("B/N"),
            Span::styled(" PREV/NEXT   ", Style::default().fg(TEXT)),
            key_span("←/→"),
            Span::styled(" SEEK", Style::default().fg(TEXT)),
        ])),
        rows[3],
    );
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (mode, text, style) = match app.input_mode {
        InputMode::None if app.busy => (
            "WORKING",
            app.status.clone(),
            Style::default().fg(Color::LightYellow),
        ),
        InputMode::None => ("READY", app.status.clone(), Style::default().fg(ACCENT)),
        InputMode::Search => (
            "QUERY",
            format!("{}█", app.input),
            Style::default().fg(ACCENT),
        ),
        InputMode::LibraryFilter => (
            "FILTER",
            format!("{}█", app.input),
            Style::default().fg(ACCENT),
        ),
        InputMode::Import => (
            "IMPORT",
            format!("{}█", app.input),
            Style::default().fg(ACCENT),
        ),
        InputMode::Playlist { .. } => (
            "PLAYLIST",
            format!("{}█", app.input),
            Style::default().fg(ACCENT),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {mode} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {text}"), Style::default().fg(TEXT)),
            Span::styled("   [?] commands", Style::default().fg(MUTED)),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>) {
    let area = centered_rect(86, 96, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "COMMAND MAP",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        help_line("SPACE / p", "pause or resume the active track"),
        help_line("z", "stop playback and release the audio stream"),
        help_line("Enter", "open album/playlist or play selected track"),
        help_line("A", "play an opened album or playlist from the beginning"),
        help_line("P", "add a library/album track to a playlist"),
        help_line("x / Delete", "remove selected queue/playlist entry"),
        help_line("b / n", "previous / next track"),
        help_line("← / →", "seek backward / forward 5 seconds"),
        help_line("+ / -", "change volume by 5%"),
        help_line("s / r", "cycle shuffle / repeat"),
        Line::from(""),
        help_line("1 ... 5", "switch workspace"),
        help_line("j / k", "move selection"),
        help_line("/", "search online; in Library, filter collections"),
        help_line("Esc / Backspace", "return from an opened album or playlist"),
        help_line("A", "download the complete album or playlist"),
        help_line("I", "import a local audio directory"),
        help_line("q / Ctrl-C", "quit EzMusic"),
        Line::from(""),
        Line::from(Span::styled(
            "Press ?, Esc or Enter to close",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel(" EZMUSIC / HELP ".into())),
        area,
    );
}

fn render_notice(frame: &mut Frame<'_>) {
    let area = centered_rect(70, 45, frame.area());
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(Span::styled(
            "RESPONSIBLE DOWNLOAD NOTICE",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Download only content you are authorized to obtain."),
        Line::from("EzMusic does not bypass DRM and accesses public content only."),
        Line::from("yt-dlp and FFmpeg are provisioned only on first online use."),
        Line::from(""),
        Line::from(vec![
            key_span("ENTER"),
            Span::styled(" ACCEPT    ", Style::default().fg(TEXT)),
            key_span("Q / ESC"),
            Span::styled(" EXIT", Style::default().fg(TEXT)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(panel(" FIRST RUN ".into())),
        area,
    );
}

fn panel(title: String) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
}

fn key_span(key: &str) -> Span<'static> {
    Span::styled(
        format!("[{key}]"),
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

fn metric_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<17}"), Style::default().fg(MUTED)),
        Span::styled(
            value,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn help_line(key: &str, action: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<13}"), Style::default().fg(ACCENT)),
        Span::styled(action.to_string(), Style::default().fg(TEXT)),
    ])
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn download_requests(items: Vec<SearchItem>) -> Vec<DownloadRequest> {
    let mut requests: Vec<DownloadRequest> = Vec::new();
    for item in items {
        if let Some(album) = item.album_identity.clone() {
            if let Some(request) = requests.iter_mut().find(|request| {
                request.album.as_ref().is_some_and(|existing| {
                    existing.provider == album.provider && existing.source_id == album.source_id
                })
            }) {
                request.items.push(item);
            } else {
                requests.push(DownloadRequest {
                    job_id: format!(
                        "{}-album-{}",
                        safe_component(&album.provider),
                        safe_component(&album.source_id)
                    ),
                    items: vec![item],
                    album: Some(album),
                });
            }
        } else {
            requests.push(DownloadRequest {
                job_id: format!(
                    "{}-{}",
                    safe_component(&item.provider),
                    safe_component(&item.source_id)
                ),
                items: vec![item],
                album: None,
            });
        }
    }
    requests
}

fn next_index(current: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        (current + 1).min(length - 1)
    }
}

fn short_label(value: &str, max_chars: usize) -> String {
    let mut label: String = value
        .chars()
        .take(max_chars)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if value.chars().count() > max_chars {
        label.push('…');
    }
    label
}

fn duration_label(duration: Option<u64>) -> String {
    duration
        .map(|seconds| format!("{:02}:{:02}", seconds / 60, seconds % 60))
        .unwrap_or_else(|| "--:--".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use tempfile::{TempDir, tempdir};

    fn test_app() -> (TempDir, App) {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            config_file: root.join("config/config.toml"),
            database_file: root.join("data/library.sqlite3"),
            tools_dir: root.join("data/tools"),
        };
        let config = AppConfig {
            library_path: root.join("music"),
            accepted_download_notice: true,
            ..AppConfig::default()
        };
        let app = App::new(paths, config, LibraryDb::in_memory().unwrap()).unwrap();
        (directory, app)
    }

    fn track(id: i64) -> Track {
        Track {
            id,
            provider: None,
            source_id: None,
            title: format!("Track {id}"),
            artist: "Artist".into(),
            album: None,
            path: PathBuf::from(format!("/tmp/track-{id}.opus")),
            duration_seconds: Some(180),
            available: true,
            imported: true,
        }
    }

    fn search_item(id: &str, kind: SearchKind) -> SearchItem {
        SearchItem {
            provider: "youtube".into(),
            source_id: id.into(),
            kind,
            title: format!("Item {id}"),
            artist: "Artist".into(),
            album: None,
            duration_seconds: Some(180),
            url: format!("https://www.youtube.com/watch?v={id}"),
            segment: None,
            album_identity: None,
        }
    }

    fn seed_album(app: &App, source_id: &str, titles: &[&str]) -> i64 {
        let album = crate::model::AlbumDraft {
            provider: "youtube".into(),
            source_id: source_id.into(),
            title: format!("Album {source_id}"),
            artist: "Album Artist".into(),
        };
        let tracks: Vec<_> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| crate::model::TrackDraft {
                provider: Some("youtube".into()),
                source_id: Some(format!("{source_id}-{index}")),
                title: (*title).into(),
                artist: "Track Artist".into(),
                album: Some(album.title.clone()),
                path: PathBuf::from(format!("/tmp/{source_id}-{index}.opus")),
                duration_seconds: Some(60),
                imported: false,
            })
            .collect();
        app.db.upsert_album(&album, &tracks).unwrap()
    }

    fn seed_playlist(app: &App, name: &str, titles: &[&str]) -> i64 {
        let playlist = app.db.create_playlist(name).unwrap();
        for (index, title) in titles.iter().enumerate() {
            let id = app
                .db
                .upsert_track(&crate::model::TrackDraft {
                    provider: None,
                    source_id: None,
                    title: (*title).into(),
                    artist: "Playlist Artist".into(),
                    album: None,
                    path: PathBuf::from(format!("/tmp/{name}-{index}.opus")),
                    duration_seconds: Some(60),
                    imported: true,
                })
                .unwrap();
            app.db.add_to_playlist(playlist, id).unwrap();
        }
        playlist
    }

    #[test]
    fn selection_stays_in_bounds() {
        assert_eq!(next_index(0, 0), 0);
        assert_eq!(next_index(0, 2), 1);
        assert_eq!(next_index(1, 2), 1);
    }

    #[test]
    fn formats_duration() {
        assert_eq!(duration_label(Some(125)), "02:05");
        assert_eq!(duration_label(None), "--:--");
    }

    #[test]
    fn dispatches_each_key_once() {
        let (_directory, mut app) = test_app();
        app.library = vec![track(1), track(2), track(3)];
        assert!(dispatch_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        ));
        assert_eq!(app.library_index, 1);
    }

    #[test]
    fn help_is_a_toggleable_overlay() {
        let (_directory, mut app) = test_app();
        app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.show_help);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.show_help);
    }

    #[test]
    fn renders_compact_and_wide_workspaces() {
        let (_directory, mut app) = test_app();
        app.library = vec![track(1), track(2)];
        app.screen = Screen::Search;
        for (width, height) in [(80, 24), (128, 36)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let output = terminal.backend().buffer().content().iter().fold(
                String::new(),
                |mut output, cell| {
                    output.push_str(cell.symbol());
                    output
                },
            );
            assert!(output.contains("EZMUSIC"));
            assert!(output.contains("SPACE/P"));
            assert!(output.contains("STOP"));
            assert!(output.contains("full album"));
        }
    }

    #[test]
    fn restores_search_page_with_selection_and_cursor() {
        let (_directory, mut app) = test_app();
        app.search_title = "original".into();
        app.search_results = vec![
            search_item("one", SearchKind::Track),
            search_item("two", SearchKind::Album),
        ];
        app.selected_results.insert(1);
        app.search_index = 1;
        app.save_search_page();
        app.search_title = "album".into();
        app.search_is_collection = true;
        app.search_results = vec![search_item("track", SearchKind::Track)];

        app.back_search();

        assert_eq!(app.search_title, "original");
        assert_eq!(app.search_index, 1);
        assert!(app.selected_results.contains(&1));
        assert_eq!(app.search_results.len(), 2);
        assert!(app.search_history.is_empty());
    }

    #[test]
    fn full_album_action_accepts_collection_or_album_result_only() {
        let (_directory, mut app) = test_app();
        app.search_results = vec![search_item("track", SearchKind::Track)];
        assert!(app.full_collection_items().is_none());

        app.search_results = vec![search_item("album", SearchKind::Album)];
        assert_eq!(app.full_collection_items().unwrap().len(), 1);

        app.search_is_collection = true;
        app.search_results = vec![
            search_item("one", SearchKind::Track),
            search_item("two", SearchKind::Track),
        ];
        assert_eq!(app.full_collection_items().unwrap().len(), 2);
    }

    #[test]
    fn groups_one_logical_album_into_one_download_request() {
        let album = crate::model::AlbumDraft {
            provider: "youtube".into(),
            source_id: "video".into(),
            title: "Record".into(),
            artist: "Artist".into(),
        };
        let mut one = search_item("one", SearchKind::Track);
        one.album_identity = Some(album.clone());
        let mut two = search_item("two", SearchKind::Track);
        two.album_identity = Some(album);
        let requests = download_requests(vec![one, two]);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].items.len(), 2);
        assert!(requests[0].album.is_some());
    }

    #[test]
    fn opens_album_without_playing_and_restores_parent_selection() {
        let (_directory, mut app) = test_app();
        seed_album(&app, "a", &["One"]);
        seed_album(&app, "b", &["Two"]);
        app.refresh_library();
        app.library_index = 1;

        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.album_detail.is_some());
        assert!(app.queue.is_empty());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.album_detail.is_none());
        assert_eq!(app.library_index, 1);
    }

    #[test]
    fn album_selection_builds_and_persists_ordered_queue() {
        let (_directory, mut app) = test_app();
        seed_album(&app, "record", &["One", "Two"]);
        app.refresh_library();
        app.open_library_selection();
        app.screen_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.screen_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            app.queue
                .iter()
                .map(|track| track.title.as_str())
                .collect::<Vec<_>>(),
            ["One", "Two"]
        );
        assert_eq!(app.queue_index, 1);
        assert_eq!(app.db.load_queue().unwrap().len(), 2);
        app.screen_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
        assert_eq!(app.queue_index, 0);
    }

    #[test]
    fn library_filter_matches_album_tracks_but_keeps_full_count() {
        let (_directory, mut app) = test_app();
        seed_album(&app, "record", &["First", "Needle"]);
        app.input = "Needle".into();
        app.apply_library_filter();
        assert_eq!(app.albums.len(), 1);
        assert_eq!(app.albums[0].track_count, 2);
        assert!(app.library.is_empty());
    }

    #[test]
    fn renders_album_detail_in_compact_and_wide_terminals() {
        let (_directory, mut app) = test_app();
        seed_album(&app, "record", &["One", "Two"]);
        app.refresh_library();
        app.open_library_selection();
        for (width, height) in [(80, 24), (128, 36)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let output = terminal.backend().buffer().content().iter().fold(
                String::new(),
                |mut output, cell| {
                    output.push_str(cell.symbol());
                    output
                },
            );
            assert!(output.contains("ALBUM RECORD"));
            assert!(output.contains("play album"));
        }
    }

    #[test]
    fn opens_playlist_without_playing_and_restores_parent_selection() {
        let (_directory, mut app) = test_app();
        seed_playlist(&app, "A", &["One"]);
        seed_playlist(&app, "B", &["Two"]);
        app.playlists = app.db.playlists().unwrap();
        app.screen = Screen::Playlists;
        app.playlist_index = 1;

        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.playlist_detail.is_some());
        assert!(app.queue.is_empty());
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.playlist_detail.is_none());
        assert_eq!(app.playlist_index, 1);
    }

    #[test]
    fn playlist_selection_persists_queue_and_a_starts_from_first() {
        let (_directory, mut app) = test_app();
        seed_playlist(&app, "Mix", &["One", "Two"]);
        app.playlists = app.db.playlists().unwrap();
        app.screen = Screen::Playlists;
        app.open_playlist();
        app.screen_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.screen_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.queue_index, 1);
        assert_eq!(app.db.load_queue().unwrap().len(), 2);
        app.screen_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
        assert_eq!(app.queue_index, 0);
    }

    #[test]
    fn removing_playlist_track_refreshes_count_and_preserves_queue_snapshot() {
        let (_directory, mut app) = test_app();
        let playlist = seed_playlist(&app, "Mix", &["One", "Two", "Three"]);
        app.playlists = app.db.playlists().unwrap();
        app.screen = Screen::Playlists;
        app.open_playlist();
        app.queue = app.playlist_detail.as_ref().unwrap().tracks.clone();
        app.persist_queue();
        app.playlist_detail.as_mut().unwrap().index = 1;

        app.remove_playlist_track();

        assert_eq!(app.queue.len(), 3);
        assert_eq!(app.db.load_queue().unwrap().len(), 3);
        assert_eq!(app.playlists[0].2, 2);
        assert_eq!(
            app.db
                .playlist_tracks(playlist)
                .unwrap()
                .into_iter()
                .map(|track| track.title)
                .collect::<Vec<_>>(),
            ["One", "Three"]
        );
        assert_eq!(app.playlist_detail.as_ref().unwrap().index, 1);
    }

    #[test]
    fn renders_playlist_detail_in_compact_and_wide_terminals() {
        let (_directory, mut app) = test_app();
        seed_playlist(&app, "Mix", &["One"]);
        app.playlists = app.db.playlists().unwrap();
        app.screen = Screen::Playlists;
        app.open_playlist();
        for (width, height) in [(80, 24), (128, 36)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let output = terminal.backend().buffer().content().iter().fold(
                String::new(),
                |mut output, cell| {
                    output.push_str(cell.symbol());
                    output
                },
            );
            assert!(output.contains("PLAYLIST / MIX"));
            assert!(output.contains("remove"));
        }
    }

    #[test]
    fn sanitizes_and_bounds_search_page_titles() {
        assert_eq!(short_label("album\nname", 20), "album name");
        assert_eq!(short_label("abcdef", 3), "abc…");
    }
}
