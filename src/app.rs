//! Application state and egui UI.
//!
//! Network work runs on a background tokio runtime and reports back over an
//! `mpsc` channel, which the UI drains each frame. Nothing blocks the UI thread.
//!
//! Note: egui 0.36's `eframe::App` trait is `ui(&mut self, ui, frame)` — the
//! root `Ui` is handed to us, and panels are added to it with
//! `egui::Panel::top(...)` / `egui::CentralPanel`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use egui::{Align, Layout, RichText};

use crate::audio::{resolver, AudioEngine};
use crate::auth::ResolvedAuth;
use crate::config::{AuthConfig, AuthMode, Config};
use crate::ytm::{Client, Page, SearchFilter, SearchItem, Track};
use crate::thumbs;
use crate::ui;

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
    Image {
        uri: String,
        result: Result<egui::ColorImage, String>,
    },
}

/// Simultaneous artwork downloads. Decoding happens off the UI thread, but this
/// still avoids a thundering herd right after a page loads.
const MAX_THUMB_INFLIGHT: usize = 6;

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
    PlaySingle(Track),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Nav {
    Home,
    Search,
    Library,
}

/// Turn a clicked result/shelf item into an action.
fn activate(items: &[SearchItem], idx: usize) -> Option<Action> {
    let item = items.get(idx)?;
    if item.playable() {
        item.to_track().map(Action::PlaySingle)
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
    advance_armed: bool,

    audio: Option<AudioEngine>,
    audio_error: Option<String>,
    volume: f32,

    img_http: reqwest::Client,
    thumbnails: HashMap<String, egui::TextureHandle>,
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
                eprintln!(
                    "[auth] resolving: mode={:?} browser={:?} profile={:?}",
                    cfg.auth.mode, cfg.auth.browser, cfg.auth.profile
                );
                let result = crate::auth::resolve(&cfg).await.map_err(|e| e.to_string());
                let _ = tx.send(Msg::Auth(result));
                ctx.request_repaint();
            });
        }

        // Playback is handled by yt-dlp reading the browser directly, so a
        // previously-discovered source lets it start immediately without
        // waiting for the (slower) InnerTube cookie extraction.
        let auth = crate::auth::cached_source().unwrap_or_else(|| cfg.auth.clone());
        let theme_light = cfg.ui.theme == "light";
        Ok(Self {
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
            nav: Nav::Search,
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
            advance_armed: false,
            audio,
            audio_error,
            volume,
            img_http,
            thumbnails: HashMap::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
        })
    }

    // ---------------------------------------------------------------- actions

    fn run_search(&mut self, ctx: &egui::Context) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.busy = true;
        self.status = Some(format!("Searching “{query}”…"));
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let filter = self.filter;
        self.rt.spawn(async move {
            let result = client.search(&query, filter).await.map_err(|e| e.to_string());
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
        } else {
            self.status = Some(format!("Starting stream for “{}”…", track.title));
            self.pending_play = Some(track.clone());
            self.start_stream(ctx, track);
        }

        // Fetch the next track in the background so advancing is instant.
        if let Some(next) = self.queue.get(idx + 1).cloned() {
            if !resolver::cache_path(&next.video_id).exists() {
                self.spawn_download(ctx, next.video_id.clone());
            }
        }
    }

    fn play_path(&mut self, path: &Path, track: Track) {
        let Some(audio) = self.audio.as_mut() else {
            self.status = Some(
                self.audio_error
                    .clone()
                    .unwrap_or_else(|| "Audio output unavailable".to_string()),
            );
            return;
        };
        match audio.play_file(path, track) {
            Ok(()) => {
                self.status = None;
                self.advance_armed = true;
            }
            Err(e) => self.status = Some(format!("Playback failed: {e}")),
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
            Action::PlaySingle(track) => {
                self.queue = vec![track];
                self.play_index(ctx, 0);
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
                }
                Msg::Search(Err(e)) => {
                    self.busy = false;
                    self.status = Some(format!("Search failed: {e}"));
                }
                Msg::Page(Ok(page)) => {
                    self.queue = page.tracks.clone();
                    self.current = None;
                    self.advance_armed = false;
                    self.view = View::Detail(page);
                    self.busy = false;
                    self.status = None;
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
                            if is_pending {
                                if let Some(track) = self.pending_play.take() {
                                    self.play_path(&path, track);
                                }
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
                            Ok(decoder) => {
                                self.pending_play = None;
                                match self.audio.as_mut() {
                                    Some(audio) => match audio.play_decoder(decoder, track) {
                                        Ok(()) => {
                                            self.status = None;
                                            self.advance_armed = true;
                                        }
                                        Err(e) => {
                                            self.status = Some(format!("Playback failed: {e}"))
                                        }
                                    },
                                    None => {
                                        self.status = Some(
                                            self.audio_error
                                                .clone()
                                                .unwrap_or_else(|| {
                                                    "Audio output unavailable".to_string()
                                                }),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                // No streamable m4a, or yt-dlp/youtube refused:
                                // fall back to the download+transcode path.
                                self.status =
                                    Some(format!("Streaming unavailable ({e}) — downloading…"));
                                self.spawn_download(ctx, video_id);
                            }
                        }
                    }
                }
                Msg::Image { uri, result } => {
                    self.thumb_inflight.remove(&uri);
                    match result {
                        Ok(image) => {
                            // Cheap now: the pixels are already decoded.
                            let texture =
                                ctx.load_texture(uri.clone(), image, egui::TextureOptions::LINEAR);
                            self.thumbnails.insert(uri, texture);
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
            if let Some(audio) = &self.audio {
                if let Some(track) = &audio.now_playing {
                    push(&track.thumbnail);
                }
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
            .exact_size(190.0)
            .show(ui, |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("♪  YouTube Music").size(16.0).strong());
                });
                ui.add_space(14.0);
                nav_button(ui, &mut nav, Nav::Home, "🏠   Home");
                nav_button(ui, &mut nav, Nav::Search, "🔍   Search");
                nav_button(ui, &mut nav, Nav::Library, "📚   Library");
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                let theme_label = if self.theme_light {
                    "☾   Dark theme"
                } else {
                    "☀   Light theme"
                };
                if ui.button(theme_label).clicked() {
                    self.theme_light = !self.theme_light;
                    self.theme_applied = false;
                    self.save_ui_settings();
                }

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(12.0);
                    if let Some(msg) = &self.status {
                        ui.label(RichText::new(msg).small().weak());
                        ui.add_space(8.0);
                    }
                    if self.ytdlp.is_none() {
                        ui.label(
                            RichText::new("⚠  yt-dlp not found")
                                .small()
                                .color(ui::WARN),
                        );
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
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                if ui
                    .add_enabled(
                        !matches!(self.view, View::Search),
                        egui::Button::new("⏴  Back"),
                    )
                    .clicked()
                {
                    go_back = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("🔍").weak());
                let edit = ui.add_sized(
                    egui::vec2(430.0, 30.0),
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search songs, albums, artists…"),
                );
                let submitted =
                    edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                egui::ComboBox::from_id_salt("search_filter")
                    .selected_text(self.filter.label())
                    .show_ui(ui, |ui| {
                        for option in SearchFilter::ALL_FILTERS {
                            ui.selectable_value(&mut self.filter, option, option.label());
                        }
                    });

                if ui.button("Search").clicked() || submitted {
                    do_search = true;
                }
                if self.busy {
                    ui.spinner();
                }
            });
            ui.add_space(8.0);
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
        let view = std::mem::replace(&mut self.view, View::Search);
        let current_video = self
            .current
            .and_then(|c| self.queue.get(c))
            .map(|t| t.video_id.clone());

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &view {
                    View::Search => self.results_view(ui, &current_video, &mut action),
                    View::Detail(page) => self.detail_view(ui, page, &current_video, &mut action),
                });
        });

        self.view = view;
        if let Some(action) = action {
            self.handle_action(ctx, action);
        }
    }

    fn results_view(
        &mut self,
        ui: &mut egui::Ui,
        current_video: &Option<String>,
        action: &mut Option<Action>,
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
                if row.clicked() {
                    *action = activate(&self.results, idx);
                }
            }
        } else if let Some(idx) = ui::cover_grid(
            ui,
            &self.thumbnails,
            &self.results,
            156.0,
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
                156.0,
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
                                    egui::vec2(48.0, 48.0),
                                    6.0,
                                );
                                ui.add_space(10.0);
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&track.title).strong());
                                    ui.label(RichText::new(track.artist_line()).weak().small());
                                });
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
                            if ui::round_button(ui, "⏮", 34.0, false).clicked() {
                                step = -1;
                            }
                            let glyph = if paused { "▶" } else { "⏸" };
                            if ui::round_button(ui, glyph, 48.0, true).clicked() {
                                toggle_pause = true;
                            }
                            if ui::round_button(ui, "⏭", 34.0, false).clicked() {
                                step = 1;
                            }
                        },
                    );

                    // Right: volume.
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), row_height),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.add_sized(
                                egui::vec2(96.0, 16.0),
                                egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                            );
                            ui.add_space(2.0);
                            ui.label(RichText::new("🔊").weak());
                        },
                    );
                });

                // Progress row.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(ui::fmt_time(pos)).weak().small());
                    ui.add_space(10.0);
                    let bar_width = (ui.available_width() - 70.0).max(80.0);
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
                });
                ui.add_space(10.0);
            });

        self.volume = volume;
        if let Some(audio) = self.audio.as_mut() {
            audio.set_volume(volume);
        }
        if toggle_pause {
            if let Some(audio) = self.audio.as_ref() {
                audio.toggle_pause();
            }
        }
        if let Some(pos) = seek_to {
            if let Some(audio) = self.audio.as_ref() {
                audio.seek(Duration::from_secs_f32(pos.max(0.0)));
            }
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
    }
}

fn detail_header(
    ui: &mut egui::Ui,
    page: &Page,
    thumbs: &HashMap<String, egui::TextureHandle>,
) {
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


