//! Application state and egui UI.
//!
//! Network work runs on a background tokio runtime and reports back over an
//! `mpsc` channel, which the UI drains each frame. Nothing blocks the UI thread.
//!
//! Note: egui 0.36's `eframe::App` trait is `ui(&mut self, ui, frame)` — the
//! root `Ui` is handed to us, and panels are added to it with
//! `egui::Panel::top(...)` / `egui::CentralPanel`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use egui::{Align, Layout, RichText};

use crate::audio::{AudioEngine, resolver};
use crate::auth::ResolvedAuth;
use crate::config::{AuthConfig, AuthMode, Config};
use crate::thumbs;
use crate::ui;
use crate::ytm::{Client, Page, SearchFilter, SearchItem, Track};

enum Msg {
    Search(Result<Vec<SearchItem>, String>),
    Page(Result<Page, String>),
    Downloaded {
        video_id: String,
        result: Result<PathBuf, String>,
    },
    Auth(Result<Option<ResolvedAuth>, String>),
    /// A stream has been opened and decoded (off the UI thread).
    Decoded {
        track: Track,
        result: Result<rodio::Decoder<crate::audio::stream::RemoteSource>, String>,
    },
    /// A stream opened ahead of time, ready to play instantly.
    Prefetched {
        track: Track,
        result: Result<rodio::Decoder<crate::audio::stream::RemoteSource>, String>,
    },
    /// Further shelves loaded via a continuation token.
    MoreShelves {
        browse_id: String,
        result: Result<crate::ytm::client::MoreShelves, String>,
    },
    /// The account's listening history, used to warm likely tracks.
    History(Result<Vec<Track>, String>),
    Image {
        uri: String,
        result: Result<egui::ColorImage, String>,
    },
}

/// Simultaneous artwork downloads. Decoding happens off the UI thread, but this
/// still avoids a thundering herd right after a page loads.
const MAX_THUMB_INFLIGHT: usize = 6;

/// How many times we page in more shelves for a feed (home/explore/library).
const MAX_MORE_ROUNDS: u32 = 5;

/// Artwork textures kept in memory (~160x160 px each). Feeds list many covers,
/// so this bounds growth as the user browses.
const MAX_THUMBS: usize = 200;

/// Artwork is fetched with reqwest (the same stack used for the API) and
/// uploaded as a texture, rather than relying on egui's built-in HTTP loader.
const IMAGE_UA: &str = thumbs::USER_AGENT;

enum View {
    Search,
    Detail(Page),
}

/// A UI interaction collected while rendering, applied once the frame's
/// borrows have ended.
enum Action {
    Open(String),
    PlayQueueIndex(usize),
    /// Play within a surrounding list so next/prev move through it.
    PlayList {
        tracks: Vec<Track>,
        index: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Nav {
    Home,
    Search,
    Library,
}

/// A stream opened before the user pressed play, so starting is instant.
struct PrefetchedStream {
    video_id: String,
    decoder: rodio::Decoder<crate::audio::stream::RemoteSource>,
}

/// Where per-track play counts live.
fn play_counts_path() -> PathBuf {
    crate::config::config_dir().join("plays.tsv")
}

fn load_play_counts() -> HashMap<String, u32> {
    let Ok(text) = std::fs::read_to_string(play_counts_path()) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (video_id, count) = line.split_once('\t')?;
            Some((video_id.to_string(), count.trim().parse().ok()?))
        })
        .collect()
}

fn save_play_counts(counts: &HashMap<String, u32>) {
    let path = play_counts_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text: String = counts
        .iter()
        .map(|(video_id, count)| format!("{video_id}\t{count}\n"))
        .collect();
    let _ = std::fs::write(path, text);
}

/// Turn a clicked result/shelf item into an action.
fn activate(items: &[SearchItem], idx: usize) -> Option<Action> {
    let item = items.get(idx)?;
    if item.playable() {
        // Play inside the surrounding list, so next/prev walk it instead of
        // looping on a single track.
        let tracks: Vec<Track> = items.iter().filter_map(|i| i.to_track()).collect();
        let video_id = item.video_id.as_deref()?;
        let index = tracks.iter().position(|t| t.video_id == video_id)?;
        Some(Action::PlayList { tracks, index })
    } else {
        item.browse_id.clone().map(Action::Open)
    }
}

fn empty_state(ui: &mut egui::Ui, busy: bool) {
    ui.add_space(72.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(if busy {
                "Searching…"
            } else {
                "Search YouTube Music to get started."
            })
            .weak(),
        );
    });
}

fn nav_button(ui: &mut egui::Ui, current: &mut Nav, value: Nav, label: &str) {
    let selected = *current == value;
    let text = RichText::new(label).size(15.0);
    let text = if selected { text.strong() } else { text };
    let width = ui.available_width();
    if ui
        .add_sized([width, 30.0], egui::Button::selectable(selected, text))
        .clicked()
    {
        *current = value;
    }
}

pub struct App {
    rt: tokio::runtime::Runtime,
    client: Arc<Client>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    ytdlp: Option<PathBuf>,
    /// Effective cookie source for yt-dlp (auto-detected source included).
    auth: AuthConfig,
    authenticated: bool,
    /// A track the user asked to play before we knew a cookie source.
    deferred_play: Option<usize>,

    query: String,
    filter: SearchFilter,
    /// Last query run, so it can be repeated once sign-in completes.
    last_search: Option<(String, SearchFilter)>,
    nav: Nav,
    theme_light: bool,
    theme_applied: bool,
    view: View,
    results: Vec<SearchItem>,

    status: Option<String>,
    busy: bool,

    queue: Vec<Track>,
    current: Option<usize>,
    pending_play: Option<Track>,
    downloading: HashSet<String>,
    /// Streams opened ahead of the click (most recent last).
    prefetched: Vec<PrefetchedStream>,
    /// How often each track has been played here, so favourites get warmed.
    play_counts: HashMap<String, u32>,
    /// The account's listening history (most recent first).
    history: Vec<Track>,
    prefetch_inflight: HashSet<String>,
    /// Debounce state for hover-triggered prefetching.
    hover_target: Option<(String, Instant)>,
    /// Rounds of continuation loading for the current feed page.
    more_rounds: u32,
    /// Whether the home feed has been reloaded after sign-in.
    home_reloaded: bool,
    advance_armed: bool,

    audio: Option<AudioEngine>,
    audio_error: Option<String>,
    volume: f32,
    /// Last non-zero volume, for the mute toggle.
    last_volume: f32,

    img_http: reqwest::Client,
    thumbnails: HashMap<String, egui::TextureHandle>,
    /// Insertion order of `thumbnails`, for eviction.
    thumb_order: VecDeque<String>,
    thumb_inflight: HashSet<String>,
    thumb_failed: HashSet<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let cfg = Config::load();
        // Start anonymous; cookies are resolved in the background below.
        let client = Arc::new(Client::new("en", "US", None)?);

        let (audio, audio_error) = match AudioEngine::new() {
            Ok(engine) => (Some(engine), None),
            Err(e) => (None, Some(format!("Audio output unavailable: {e}"))),
        };
        let volume = cfg.ui.volume;
        let img_http = reqwest::Client::builder()
            .user_agent(IMAGE_UA)
            .build()
            .context("building image HTTP client")?;

        let ytdlp = resolver::locate_ytdlp();
        resolver::clean_stale_parts();
        let status = if ytdlp.is_none() {
            Some(
                "yt-dlp not found — playback is disabled. Install it or set YTDLP_PATH."
                    .to_string(),
            )
        } else {
            None
        };

        let (tx, rx) = channel();

        // Resolve browser cookies off the UI thread so startup stays instant.
        {
            let cfg = cfg.clone();
            let tx = tx.clone();
            let ctx = cc.egui_ctx.clone();
            rt.spawn(async move {
                let started = std::time::Instant::now();
                let mut resolved = crate::auth::resolve(&cfg).await;

                // A reused jar is only trusted if InnerTube still accepts it.
                let stale = match &resolved {
                    Ok(Some(auth)) => {
                        match crate::ytm::Client::new("en", "US", Some(auth.jar.clone())) {
                            Ok(probe) => !probe.logged_in().await.unwrap_or(false),
                            Err(_) => false,
                        }
                    }
                    _ => false,
                };
                if stale {
                    eprintln!("[auth] cached cookies rejected — re-extracting");
                    if let Ok(Some(fresh)) = crate::auth::resolve_fresh(&cfg).await {
                        resolved = Ok(Some(fresh));
                    }
                }
                match &resolved {
                    Ok(Some(auth)) => eprintln!(
                        "[auth] ready in {:?} via {} (session={})",
                        started.elapsed(),
                        auth.source.browser,
                        auth.jar.has_youtube_session()
                    ),
                    Ok(None) => eprintln!("[auth] none in {:?}", started.elapsed()),
                    Err(e) => eprintln!("[auth] failed in {:?}: {e}", started.elapsed()),
                }
                let result = resolved.map_err(|e| e.to_string());
                let _ = tx.send(Msg::Auth(result));
                ctx.request_repaint();
            });
        }

        // Playback is handled by yt-dlp reading the browser directly, so a
        // previously-discovered source lets it start immediately without
        // waiting for the (slower) InnerTube cookie extraction.
        let auth = crate::auth::cached_source().unwrap_or_else(|| cfg.auth.clone());
        let theme_light = cfg.ui.theme == "light";
        let mut app = Self {
            rt,
            client,
            tx,
            rx,
            ytdlp,
            auth,
            authenticated: false,
            deferred_play: None,
            query: String::new(),
            filter: SearchFilter::All,
            last_search: None,
            nav: Nav::Home,
            theme_light,
            theme_applied: false,
            view: View::Search,
            results: Vec::new(),
            status,
            busy: false,
            queue: Vec::new(),
            current: None,
            pending_play: None,
            downloading: HashSet::new(),
            prefetched: Vec::new(),
            play_counts: load_play_counts(),
            history: Vec::new(),
            prefetch_inflight: HashSet::new(),
            hover_target: None,
            more_rounds: 0,
            home_reloaded: false,
            advance_armed: false,
            audio,
            audio_error,
            volume,
            last_volume: if volume > 0.001 { volume } else { 0.8 },
            img_http,
            thumbnails: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
        };
        // Open on the home feed rather than a blank search page.
        app.open_page(&cc.egui_ctx, "FEmusic_home".to_string());
        Ok(app)
    }

    // ---------------------------------------------------------------- actions

    fn run_search(&mut self, ctx: &egui::Context) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.busy = true;
        self.status = Some(format!("Searching “{query}”…"));
        self.last_search = Some((query.clone(), self.filter));
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let filter = self.filter;
        self.rt.spawn(async move {
            let result = client
                .search(&query, filter)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Search(result));
            ctx.request_repaint();
        });
    }

    fn open_page(&mut self, ctx: &egui::Context, browse_id: String) {
        self.busy = true;
        self.status = Some("Loading…".to_string());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client.browse(&browse_id).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Page(result));
            ctx.request_repaint();
        });
    }

    /// Queue stream-URL resolution for tracks the user is likely to click, so
    /// playback can start without waiting on yt-dlp.
    /// Start playing an already-opened decoder.
    fn play_decoder(
        &mut self,
        decoder: rodio::Decoder<crate::audio::stream::RemoteSource>,
        track: Track,
    ) {
        self.pending_play = None;
        let video_id = track.video_id.clone();
        let started = match self.audio.as_mut() {
            Some(audio) => match audio.play_decoder(decoder, track) {
                Ok(()) => {
                    self.status = None;
                    self.advance_armed = true;
                    true
                }
                Err(e) => {
                    self.status = Some(format!("Playback failed: {e}"));
                    false
                }
            },
            None => {
                self.status = Some(
                    self.audio_error
                        .clone()
                        .unwrap_or_else(|| "Audio output unavailable".to_string()),
                );
                false
            }
        };
        if started {
            self.note_play(&video_id);
        }
    }

    /// Record a play so favourites can be warmed on the next launch.
    fn note_play(&mut self, video_id: &str) {
        *self.play_counts.entry(video_id.to_string()).or_insert(0) += 1;
        save_play_counts(&self.play_counts);
    }

    fn spawn_history(&mut self, ctx: &egui::Context) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client.history().await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::History(result));
            ctx.request_repaint();
        });
    }

    /// Warm the tracks this account plays most.
    ///
    /// Local play counts decide the order; with none recorded yet the history
    /// order (most recent first) is used, which is a good guess anyway.
    fn prefetch_favorites(&mut self, ctx: &egui::Context) {
        let mut ranked = self.history.clone();
        ranked.sort_by_key(|track| {
            std::cmp::Reverse(self.play_counts.get(&track.video_id).copied().unwrap_or(0))
        });
        for track in ranked.iter().take(2) {
            self.prefetch(ctx, track);
        }
    }

    /// Open a stream ahead of the user pressing play.
    fn prefetch(&mut self, ctx: &egui::Context, track: &Track) {
        let video_id = track.video_id.clone();
        if resolver::cache_path(&video_id).exists() {
            return; // already on disk, so playback is instant anyway
        }
        if self.prefetched.iter().any(|p| p.video_id == video_id)
            || self.prefetch_inflight.contains(&video_id)
            || self
                .pending_play
                .as_ref()
                .is_some_and(|t| t.video_id == video_id)
        {
            return;
        }
        // Don't pile up yt-dlp processes while sweeping the mouse.
        if self.prefetch_inflight.len() >= 2 {
            return;
        }

        self.prefetch_inflight.insert(video_id.clone());
        let args = resolver::stream_args(&video_id, &self.auth);
        let cache = Some(resolver::cache_path(&video_id));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let track = track.clone();
        std::thread::Builder::new()
            .name("ytm-prefetch".into())
            .spawn(move || {
                let result = match crate::audio::stream::RemoteSource::new(args, cache) {
                    Ok(source) => rodio::Decoder::new(source).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Msg::Prefetched { track, result });
                ctx.request_repaint();
            })
            .ok();
    }

    /// Prefetch the row under the pointer once it has rested there briefly.
    fn hover_prefetch(&mut self, ctx: &egui::Context, hovered: Option<Track>) {
        let Some(track) = hovered else {
            self.hover_target = None;
            return;
        };
        match &self.hover_target {
            Some((video_id, since)) if video_id == &track.video_id => {
                if since.elapsed() >= Duration::from_millis(150) {
                    self.prefetch(ctx, &track);
                }
            }
            _ => self.hover_target = Some((track.video_id.clone(), Instant::now())),
        }
    }

    /// Open a stream for `track` and decode it, off the UI thread.
    ///
    /// Starting a stream means waiting on `yt-dlp` (a few seconds), so the
    /// blocking work happens on a plain thread and the decoded source is handed
    /// back over the channel.
    fn start_stream(&mut self, ctx: &egui::Context, track: Track) {
        let args = resolver::stream_args(&track.video_id, &self.auth);
        let cache = Some(resolver::cache_path(&track.video_id));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("ytm-decode".into())
            .spawn(move || {
                let result = match crate::audio::stream::RemoteSource::new(args, cache) {
                    Ok(source) => rodio::Decoder::new(source).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Msg::Decoded { track, result });
                ctx.request_repaint();
            })
            .ok();
    }

    fn spawn_more_shelves(&mut self, ctx: &egui::Context, browse_id: String, token: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .browse_continuation(&token)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::MoreShelves { browse_id, result });
            ctx.request_repaint();
        });
    }

    fn spawn_download(&mut self, ctx: &egui::Context, video_id: String) {
        if !self.downloading.insert(video_id.clone()) {
            return; // already in flight
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let auth = self.auth.clone();
        self.rt.spawn(async move {
            let result = resolver::download_track(&video_id, &auth)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Downloaded { video_id, result });
            ctx.request_repaint();
        });
    }

    fn play_index(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(track) = self.queue.get(idx).cloned() else {
            return;
        };
        let path = resolver::cache_path(&track.video_id);
        // Cached tracks need no network, so they can play immediately. Anything
        // else must wait until we know a concrete cookie source, or the
        // anonymous download would be bot-blocked.
        if !path.exists() && matches!(self.auth.mode, AuthMode::Auto) {
            self.deferred_play = Some(idx);
            self.status = Some("Signing in…".to_string());
            return;
        }
        self.current = Some(idx);
        self.advance_armed = false;

        if path.exists() {
            self.pending_play = None;
            self.play_path(&path, track);
        } else if let Some(index) = self
            .prefetched
            .iter()
            .position(|p| p.video_id == track.video_id)
        {
            // Opened while hovering / when the page loaded: play immediately.
            let prepared = self.prefetched.remove(index);
            self.play_decoder(prepared.decoder, track);
        } else if self.prefetch_inflight.contains(&track.video_id) {
            // Already opening for this very track — just wait for it.
            self.status = Some(format!("Starting stream for “{}”…", track.title));
            self.pending_play = Some(track);
        } else {
            self.status = Some(format!("Starting stream for “{}”…", track.title));
            self.pending_play = Some(track.clone());
            self.start_stream(ctx, track);
        }

        // Open the next track too, so advancing is quick.
        if let Some(next) = self.queue.get(idx + 1).cloned() {
            self.prefetch(ctx, &next);
        }
    }

    fn play_path(&mut self, path: &Path, track: Track) {
        let video_id = track.video_id.clone();
        let started = match self.audio.as_mut() {
            Some(audio) => match audio.play_file(path, track) {
                Ok(()) => {
                    self.status = None;
                    self.advance_armed = true;
                    true
                }
                Err(e) => {
                    self.status = Some(format!("Playback failed: {e}"));
                    false
                }
            },
            None => {
                self.status = Some(
                    self.audio_error
                        .clone()
                        .unwrap_or_else(|| "Audio output unavailable".to_string()),
                );
                false
            }
        };
        if started {
            self.note_play(&video_id);
        }
    }

    fn step_track(&mut self, ctx: &egui::Context, delta: i32) {
        if self.queue.is_empty() {
            return;
        }
        let n = self.queue.len() as i32;
        let next = match self.current {
            Some(cur) => (cur as i32 + delta).rem_euclid(n) as usize,
            None => 0,
        };
        self.play_index(ctx, next);
    }

    fn handle_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Open(id) => self.open_page(ctx, id),
            Action::PlayQueueIndex(idx) => self.play_index(ctx, idx),
            Action::PlayList { tracks, index } => {
                self.queue = tracks;
                self.play_index(ctx, index);
            }
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Search(Ok(items)) => {
                    self.results = items;
                    self.busy = false;
                    self.status = None;
                    self.view = View::Search;
                    self.nav = Nav::Search;
                    let warm: Vec<Track> = self
                        .results
                        .iter()
                        .filter_map(|item| item.to_track())
                        .take(2)
                        .collect();
                    for track in &warm {
                        self.prefetch(ctx, track);
                    }
                }
                Msg::Search(Err(e)) => {
                    self.busy = false;
                    self.status = Some(format!("Search failed: {e}"));
                }
                Msg::Page(Ok(page)) => {
                    self.queue = page.tracks.clone();
                    self.current = None;
                    self.advance_armed = false;
                    let warm: Vec<Track> = page.tracks.iter().take(2).cloned().collect();
                    // Feeds page in their shelves; album/playlist pages don't.
                    let next = if page.browse_id.starts_with("FEmusic") {
                        page.continuation.clone()
                    } else {
                        None
                    };
                    let browse_id = page.browse_id.clone();
                    self.view = View::Detail(page);
                    self.busy = false;
                    self.status = None;
                    self.more_rounds = 0;
                    // Warm the first couple of tracks of the page.
                    for track in &warm {
                        self.prefetch(ctx, track);
                    }
                    if let Some(token) = next {
                        self.more_rounds = 1;
                        self.spawn_more_shelves(ctx, browse_id, token);
                    }
                }
                Msg::Page(Err(e)) => {
                    self.busy = false;
                    self.status = Some(format!("Could not open page: {e}"));
                }
                Msg::Downloaded { video_id, result } => {
                    self.downloading.remove(&video_id);
                    let is_pending = self
                        .pending_play
                        .as_ref()
                        .map(|t| t.video_id == video_id)
                        .unwrap_or(false);
                    match result {
                        Ok(path) => {
                            if is_pending && let Some(track) = self.pending_play.take() {
                                self.play_path(&path, track);
                            }
                        }
                        Err(e) => {
                            if is_pending {
                                self.pending_play = None;
                                self.status = Some(e);
                            }
                        }
                    }
                }
                Msg::Auth(result) => {
                    match &result {
                        Ok(Some(resolved)) => eprintln!(
                            "[auth] chose browser={:?} session={}",
                            resolved.source.browser,
                            resolved.jar.has_youtube_session()
                        ),
                        Ok(None) => eprintln!("[auth] no cookie source available"),
                        Err(e) => eprintln!("[auth] failed: {e}"),
                    }
                    match result {
                        Ok(Some(resolved)) => {
                            let youtube = resolved.jar.has_youtube_session();
                            // Remember the source that worked so yt-dlp uses it too.
                            self.auth = resolved.source.clone();
                            match Client::new("en", "US", Some(resolved.jar)) {
                                Ok(client) => {
                                    self.client = Arc::new(client);
                                    self.authenticated = true;
                                    if !youtube {
                                        self.status = Some(
                                            "Signed into Google but no YouTube session — open \
                                             music.youtube.com once to unlock your library."
                                                .to_string(),
                                        );
                                    }
                                }
                                Err(e) => self.status = Some(format!("Auth setup failed: {e}")),
                            }
                        }
                        Ok(None) => {
                            self.authenticated = false;
                            self.auth = AuthConfig {
                                mode: AuthMode::None,
                                ..Default::default()
                            };
                        }
                        Err(e) => self.status = Some(format!("Cookie import failed: {e}")),
                    }
                    // Reload the home feed now that we're signed in, so the
                    // recommendations match the account.
                    if !self.home_reloaded
                        && let View::Detail(page) = &self.view
                        && page.browse_id == "FEmusic_home"
                    {
                        self.home_reloaded = true;
                        self.open_page(ctx, "FEmusic_home".to_string());
                    }
                    // Repeat the last search: anonymous ranking differs a lot
                    // from signed-in ranking (the top result changes).
                    if self.authenticated
                        && matches!(self.view, View::Search)
                        && let Some((query, filter)) = self.last_search.clone()
                    {
                        self.query = query;
                        self.filter = filter;
                        self.run_search(ctx);
                    }
                    // Pull the listening history so favourites can be warmed.
                    if self.authenticated {
                        self.spawn_history(ctx);
                    }
                    // Play whatever the user asked for before sign-in finished.
                    if let Some(idx) = self.deferred_play.take() {
                        self.play_index(ctx, idx);
                    }
                }
                Msg::Decoded { track, result } => {
                    let video_id = track.video_id.clone();
                    let is_pending = self
                        .pending_play
                        .as_ref()
                        .map(|t| t.video_id == video_id)
                        .unwrap_or(false);
                    if is_pending {
                        match result {
                            Ok(decoder) => self.play_decoder(decoder, track),
                            Err(e) => {
                                // No streamable m4a, or yt-dlp/YouTube refused:
                                // fall back to the download+transcode path.
                                self.status =
                                    Some(format!("Streaming unavailable ({e}) — downloading…"));
                                self.spawn_download(ctx, video_id);
                            }
                        }
                    }
                }
                Msg::Prefetched { track, result } => {
                    self.prefetch_inflight.remove(&track.video_id);
                    let is_pending = self
                        .pending_play
                        .as_ref()
                        .is_some_and(|t| t.video_id == track.video_id);
                    match result {
                        Ok(decoder) if is_pending => {
                            // The user clicked while it was still opening.
                            self.play_decoder(decoder, track);
                        }
                        Ok(decoder) => {
                            self.prefetched.push(PrefetchedStream {
                                video_id: track.video_id,
                                decoder,
                            });
                            // Hold only the two most recent; dropping one stops
                            // its yt-dlp process.
                            while self.prefetched.len() > 2 {
                                self.prefetched.remove(0);
                            }
                        }
                        Err(e) => {
                            if is_pending {
                                self.status =
                                    Some(format!("Streaming unavailable ({e}) — downloading…"));
                                self.spawn_download(ctx, track.video_id);
                            }
                        }
                    }
                }
                Msg::MoreShelves { browse_id, result } => {
                    let mut next: Option<String> = None;
                    if let View::Detail(page) = &mut self.view
                        && page.browse_id == browse_id
                        && let Ok(more) = result
                    {
                        page.shelves.extend(more.shelves);
                        page.continuation = more.continuation;
                        if self.more_rounds < MAX_MORE_ROUNDS {
                            next = page.continuation.clone();
                        }
                    }
                    if let Some(token) = next {
                        self.more_rounds += 1;
                        self.spawn_more_shelves(ctx, browse_id, token);
                    }
                }
                Msg::History(Ok(tracks)) => {
                    self.history = tracks;
                    self.prefetch_favorites(ctx);
                }
                Msg::History(Err(e)) => {
                    eprintln!("[history] unavailable: {e}");
                }
                Msg::Image { uri, result } => {
                    self.thumb_inflight.remove(&uri);
                    match result {
                        Ok(image) => {
                            // Cheap now: the pixels are already decoded.
                            let texture =
                                ctx.load_texture(uri.clone(), image, egui::TextureOptions::LINEAR);
                            self.thumbnails.insert(uri.clone(), texture);
                            self.thumb_order.push_back(uri);
                            // Evict the oldest so memory stays bounded.
                            while self.thumb_order.len() > MAX_THUMBS {
                                if let Some(oldest) = self.thumb_order.pop_front() {
                                    self.thumbnails.remove(&oldest);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[thumb] failed {uri}: {e}");
                            self.thumb_failed.insert(uri);
                        }
                    }
                }
            }
        }
    }

    fn tick_playback(&mut self, ctx: &egui::Context) {
        let finished = self.audio.as_ref().map(|a| a.finished()).unwrap_or(false);
        if !(self.advance_armed && finished) {
            return;
        }
        self.advance_armed = false;
        match self.current {
            Some(cur) if cur + 1 < self.queue.len() => self.play_index(ctx, cur + 1),
            _ => {
                self.current = None;
                if let Some(audio) = self.audio.as_mut() {
                    audio.stop();
                }
            }
        }
    }

    // ------------------------------------------------------------------- views

    /// Fetch any artwork that will be drawn this frame.
    fn ensure_thumbnails(&mut self, ctx: &egui::Context) {
        let mut uris: Vec<String> = Vec::new();
        {
            let mut push = |slot: &Option<String>| {
                if let Some(uri) = slot {
                    uris.push(uri.clone());
                }
            };
            for item in &self.results {
                push(&item.thumbnail);
            }
            if let View::Detail(page) = &self.view {
                for track in &page.tracks {
                    push(&track.thumbnail);
                }
                for shelf in &page.shelves {
                    for item in &shelf.items {
                        push(&item.thumbnail);
                    }
                }
            }
            if let Some(audio) = &self.audio
                && let Some(track) = &audio.now_playing
            {
                push(&track.thumbnail);
            }
        }

        for uri in uris {
            if self.thumb_inflight.len() >= MAX_THUMB_INFLIGHT {
                break;
            }
            if self.thumbnails.contains_key(&uri) || self.thumb_failed.contains(&uri) {
                continue;
            }
            if !self.thumb_inflight.insert(uri.clone()) {
                continue; // already in flight
            }
            let http = self.img_http.clone();
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            self.rt.spawn(async move {
                // Decoding is CPU-heavy, so keep it off the UI thread (and off
                // the async workers) to avoid freezing the window.
                let result = match thumbs::fetch(&http, &uri).await {
                    Ok(bytes) => tokio::task::spawn_blocking(move || thumbs::decode(&bytes))
                        .await
                        .ok()
                        .flatten()
                        .ok_or_else(|| "unsupported image format".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Msg::Image { uri, result });
                ctx.request_repaint();
            });
        }
    }

    /// Left navigation rail.
    fn sidebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut nav = self.nav;
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(176.0)
            .show(ui, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("♪  YouTube Music").size(15.0).strong());
                });
                ui.add_space(12.0);
                nav_button(ui, &mut nav, Nav::Home, "🏠   Home");
                nav_button(ui, &mut nav, Nav::Search, "🔍   Search");
                nav_button(ui, &mut nav, Nav::Library, "📚   Library");
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(12.0);
                    if let Some(msg) = &self.status {
                        ui.label(RichText::new(msg).small().weak());
                        ui.add_space(8.0);
                    }
                    if self.ytdlp.is_none() {
                        ui.label(RichText::new("⚠  yt-dlp not found").small().color(ui::WARN));
                        ui.add_space(6.0);
                    }
                    let (label, color) = if self.authenticated {
                        ("●  signed in", ui::ACCENT)
                    } else {
                        ("○  anonymous", ui.visuals().weak_text_color())
                    };
                    ui.label(RichText::new(label).small().color(color));
                });
            });

        if nav != self.nav {
            self.nav = nav;
            match nav {
                Nav::Home => self.open_page(ctx, "FEmusic_home".to_string()),
                Nav::Search => self.view = View::Search,
                Nav::Library => self.open_page(ctx, "FEmusic_library_landing".to_string()),
            }
        }
    }

    /// Search box, filter and navigation controls.
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_search = false;
        let mut go_back = false;
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // A single interactive height keeps every control level.
                ui.spacing_mut().interact_size.y = 30.0;
                // Three fixed regions so the centre really is centred and both
                // edges get the same padding.
                let total = ui.available_width();
                let left_width = 104.0;
                let right_width = 48.0;
                let centre_width = (total - left_width - right_width).max(240.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(left_width, 30.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add_space(10.0);
                        if ui
                            .add_enabled(
                                !matches!(self.view, View::Search),
                                egui::Button::new("⏴  Back"),
                            )
                            .clicked()
                        {
                            go_back = true;
                        }
                    },
                );

                ui.allocate_ui_with_layout(
                    egui::vec2(centre_width, 30.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        let cluster = 360.0 + 46.0 + 140.0 + 92.0 + 30.0;
                        ui.add_space(((centre_width - cluster) / 2.0).max(0.0));
                        if ui::search_bar(ui, &mut self.query, 360.0) {
                            do_search = true;
                        }
                        ui.add_space(6.0);
                        egui::ComboBox::from_id_salt("search_filter")
                            .width(132.0)
                            .selected_text(self.filter.label())
                            .show_ui(ui, |ui| {
                                for option in SearchFilter::ALL_FILTERS {
                                    ui.selectable_value(&mut self.filter, option, option.label());
                                }
                            });
                        if ui.button("Search").clicked() {
                            do_search = true;
                        }
                    },
                );

                ui.allocate_ui_with_layout(
                    egui::vec2(right_width, 30.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        ui.add_space(10.0);
                        ui.menu_button("⚙", |ui| {
                            let theme_label = if self.theme_light {
                                "☾  Dark theme"
                            } else {
                                "☀  Light theme"
                            };
                            if ui.button(theme_label).clicked() {
                                self.theme_light = !self.theme_light;
                                self.theme_applied = false;
                                self.save_ui_settings();
                                ui.close();
                            }
                            ui.separator();
                            ui.label(
                                RichText::new(if self.authenticated {
                                    "signed in"
                                } else {
                                    "anonymous"
                                })
                                .small()
                                .weak(),
                            );
                        });
                        if self.busy {
                            ui.spinner();
                        }
                    },
                );
            });
            ui.add_space(6.0);
        });

        if go_back {
            self.nav = Nav::Search;
            self.view = View::Search;
        }
        if do_search {
            self.nav = Nav::Search;
            self.run_search(ctx);
        }
    }

    fn save_ui_settings(&self) {
        let mut cfg = Config::load();
        cfg.ui.theme = if self.theme_light { "light" } else { "dark" }.to_string();
        cfg.ui.volume = self.volume;
        let _ = cfg.save();
    }

    fn central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut action: Option<Action> = None;
        let mut hovered: Option<Track> = None;
        let view = std::mem::replace(&mut self.view, View::Search);
        let current_video = self
            .current
            .and_then(|c| self.queue.get(c))
            .map(|t| t.video_id.clone());

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &view {
                    View::Search => {
                        self.results_view(ui, &current_video, &mut action, &mut hovered)
                    }
                    View::Detail(page) => {
                        self.detail_view(ui, page, &current_video, &mut action, &mut hovered)
                    }
                });
        });

        self.view = view;
        self.hover_prefetch(ctx, hovered);
        if let Some(action) = action {
            self.handle_action(ctx, action);
        }
    }

    fn results_view(
        &mut self,
        ui: &mut egui::Ui,
        current_video: &Option<String>,
        action: &mut Option<Action>,
        hovered: &mut Option<Track>,
    ) {
        if self.results.is_empty() {
            empty_state(ui, self.busy);
            return;
        }
        // Songs read better as rows; everything else is cover art.
        if self.filter == SearchFilter::Songs {
            for (idx, item) in self.results.iter().enumerate() {
                let selected = item.video_id.as_deref() == current_video.as_deref();
                let trailing = item.duration_secs.map(crate::ytm::domain::fmt_duration);
                let row = ui::list_row(
                    ui,
                    &self.thumbnails,
                    ("search", idx),
                    item.thumbnail.as_deref(),
                    &item.title,
                    &item.subtitle,
                    trailing.as_deref(),
                    Some(idx),
                    selected,
                    item.playable(),
                    item.navigable(),
                );
                if row.hovered() {
                    *hovered = item.to_track();
                }
                if row.clicked() {
                    *action = activate(&self.results, idx);
                }
            }
        } else if let Some(idx) = ui::cover_grid(
            ui,
            &self.thumbnails,
            &self.results,
            196.0,
            current_video.as_deref(),
        ) {
            *action = activate(&self.results, idx);
        }
    }

    fn detail_view(
        &mut self,
        ui: &mut egui::Ui,
        page: &Page,
        current_video: &Option<String>,
        action: &mut Option<Action>,
        hovered: &mut Option<Track>,
    ) {
        detail_header(ui, page, &self.thumbnails);

        for (idx, track) in page.tracks.iter().enumerate() {
            let selected = self.current == Some(idx)
                && current_video.as_deref() == Some(track.video_id.as_str());
            let trailing = track.duration_text();
            let row = ui::list_row(
                ui,
                &self.thumbnails,
                ("track", idx),
                track.thumbnail.as_deref(),
                &track.title,
                &track.artist_line(),
                trailing.as_deref(),
                Some(idx),
                selected,
                true,
                false,
            );
            if row.hovered() {
                *hovered = Some(track.clone());
            }
            if row.clicked() {
                *action = Some(Action::PlayQueueIndex(idx));
            }
        }

        // Browsable carousels (artist albums, related, the home feed…).
        for shelf in &page.shelves {
            if shelf.items.is_empty() {
                continue;
            }
            ui.add_space(18.0);
            if !shelf.title.is_empty() {
                ui.label(RichText::new(&shelf.title).size(18.0).strong());
                ui.add_space(6.0);
            }
            if let Some(idx) = ui::cover_grid(
                ui,
                &self.thumbnails,
                &shelf.items,
                196.0,
                current_video.as_deref(),
            ) {
                *action = activate(&shelf.items, idx);
            }
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut toggle_pause = false;
        let mut step = 0i32;
        let mut seek_to: Option<f32> = None;
        let mut volume = self.volume;

        egui::Panel::bottom("player")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(8.0);
                let Some(audio) = self.audio.as_ref() else {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(
                                self.audio_error
                                    .clone()
                                    .unwrap_or_else(|| "Audio output unavailable".to_string()),
                            )
                            .weak(),
                        );
                    });
                    ui.add_space(8.0);
                    return;
                };

                let total = audio.total.map(|d| d.as_secs_f32()).unwrap_or(0.0);
                let pos = audio.position().as_secs_f32();
                let paused = audio.is_paused();
                let has_track = audio.now_playing.is_some();
                let row_height = 56.0;

                ui.horizontal(|ui| {
                    // Left: artwork and titles.
                    ui.allocate_ui_with_layout(
                        egui::vec2(320.0, row_height),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if let Some(track) = &audio.now_playing {
                                ui::draw_thumbnail(
                                    ui,
                                    track.thumbnail.as_deref(),
                                    &self.thumbnails,
                                    egui::vec2(54.0, 54.0),
                                    6.0,
                                );
                                ui.add_space(10.0);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(238.0, row_height),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        ui.add_space(12.0);
                                        ui.add(
                                            egui::Label::new(RichText::new(&track.title).strong())
                                                .truncate(),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(track.artist_line()).weak().small(),
                                            )
                                            .truncate(),
                                        );
                                    },
                                );
                            } else {
                                ui.label(RichText::new("Nothing playing").weak());
                            }
                        },
                    );

                    // Centre: transport, centred in the middle of the bar.
                    let centre_width = (ui.available_width() - 210.0).max(200.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(centre_width, row_height),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            let spacing = ui.spacing().item_spacing.x;
                            let buttons = 34.0 + 48.0 + 34.0 + spacing * 2.0;
                            ui.add_space(((centre_width - buttons) / 2.0).max(0.0));
                            // Transport is inert until something is loaded.
                            ui.add_enabled_ui(has_track, |ui| {
                                if ui::round_button(ui, "⏮", 34.0, false).clicked() {
                                    step = -1;
                                }
                                let glyph = if !has_track || paused { "▶" } else { "⏸" };
                                if ui::round_button(ui, glyph, 48.0, true).clicked() {
                                    toggle_pause = true;
                                }
                                if ui::round_button(ui, "⏭", 34.0, false).clicked() {
                                    step = 1;
                                }
                            });
                        },
                    );

                    // Right: volume.
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), row_height),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            // Right padding so the slider doesn't touch the edge.
                            ui.add_space(14.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(116.0, 16.0),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    if let Some(v) = ui::progress_bar(ui, volume, 1.0) {
                                        volume = v;
                                    }
                                },
                            );
                            ui.add_space(6.0);
                            let muted = volume <= 0.001;
                            if ui.button(if muted { "🔇" } else { "🔊" }).clicked() {
                                volume = if muted { self.last_volume } else { 0.0 };
                            }
                        },
                    );
                });

                // Progress row.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(RichText::new(ui::fmt_time(pos)).weak().small());
                    ui.add_space(10.0);
                    let bar_width = (ui.available_width() - 78.0).max(80.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(bar_width, 16.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if let Some(p) = ui::progress_bar(ui, pos, total) {
                                seek_to = Some(p);
                            }
                        },
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new(ui::fmt_time(total)).weak().small());
                    ui.add_space(14.0);
                });
                ui.add_space(10.0);
            });

        self.volume = volume;
        if volume > 0.001 {
            self.last_volume = volume;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.set_volume(volume);
        }
        if toggle_pause && let Some(audio) = self.audio.as_ref() {
            audio.toggle_pause();
        }
        if let Some(pos) = seek_to
            && let Some(audio) = self.audio.as_ref()
        {
            audio.seek(Duration::from_secs_f32(pos.max(0.0)));
        }
        if step != 0 {
            self.step_track(ctx, step);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if !self.theme_applied {
            ui::apply_theme(&ctx, self.theme_light);
            self.theme_applied = true;
        }

        self.drain_messages(&ctx);
        self.ensure_thumbnails(&ctx);
        self.tick_playback(&ctx);
        self.sidebar(ui, &ctx);
        self.top_bar(ui, &ctx);
        self.bottom_bar(ui, &ctx);
        self.central(ui, &ctx);

        let playing = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        if self.busy || !self.downloading.is_empty() || playing {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        // Keep pulling in artwork that is still on its way.
        if !self.thumb_inflight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
        // Keep the hover debounce and prefetches ticking.
        if self.hover_target.is_some() || !self.prefetch_inflight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }
}

fn detail_header(ui: &mut egui::Ui, page: &Page, thumbs: &HashMap<String, egui::TextureHandle>) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if page.thumbnail.is_some() {
            ui::draw_thumbnail(
                ui,
                page.thumbnail.as_deref(),
                thumbs,
                egui::vec2(160.0, 160.0),
                8.0,
            );
            ui.add_space(4.0);
        }
        ui.vertical(|ui| {
            let kind = page.kind.label();
            if !kind.is_empty() {
                ui.label(RichText::new(kind.to_uppercase()).weak().small());
            }
            ui.label(RichText::new(&page.title).size(26.0).strong());
            if !page.subtitle.is_empty() {
                ui.label(RichText::new(&page.subtitle).weak());
            }
            if !page.description.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&page.description).weak().small());
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} tracks", page.tracks.len()))
                    .weak()
                    .small(),
            );
        });
    });
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(4.0);
}
