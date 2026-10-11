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
use egui::{Align, Color32, Layout, RichText};

use crate::audio::{resolver, AudioEngine};
use crate::auth::ResolvedAuth;
use crate::config::{AuthConfig, AuthMode, Config};
use crate::ytm::{Client, Page, SearchFilter, SearchItem, Track};
use crate::thumbs;

enum Msg {
    Search(Result<Vec<SearchItem>, String>),
    Page(Result<Page, String>),
    Downloaded {
        video_id: String,
        result: Result<PathBuf, String>,
    },
    Auth(Result<Option<ResolvedAuth>, String>),
    Image {
        uri: String,
        result: Result<Vec<u8>, String>,
    },
}

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
            self.status = Some(format!("Loading “{}”…", track.title));
            self.pending_play = Some(track.clone());
            self.spawn_download(ctx, track.video_id.clone());
        }

        // Warm the cache for the next track so the transition is instant.
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
                Msg::Image { uri, result } => {
                    self.thumb_inflight.remove(&uri);
                    match result {
                        Ok(bytes) => match thumbs::decode(&bytes) {
                            Some(image) => {
                                let texture = ctx.load_texture(
                                    uri.clone(),
                                    image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.thumbnails.insert(uri, texture);
                            }
                            None => {
                                self.thumb_failed.insert(uri);
                            }
                        },
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
                let result = thumbs::fetch(&http, &uri).await.map_err(|e| e.to_string());
                let _ = tx.send(Msg::Image { uri, result });
                ctx.request_repaint();
            });
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_search = false;
        let mut go_back = false;
        let mut open_library = false;
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("YouTube Music");
                ui.separator();
                if self.authenticated {
                    ui.label(
                        RichText::new("● signed in").small().color(Color32::from_rgb(120, 200, 120)),
                    );
                }
                if ui
                    .add_enabled(!matches!(self.view, View::Search), egui::Button::new("⏴ Back"))
                    .clicked()
                {
                    go_back = true;
                }
                if ui.button("Library").clicked() {
                    open_library = true;
                }
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search songs, albums, artists…")
                        .desired_width(320.0),
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

            if let Some(msg) = &self.status {
                ui.label(RichText::new(msg).color(Color32::from_rgb(230, 140, 120)));
            } else if self.ytdlp.is_none() {
                ui.label(
                    RichText::new(
                        "yt-dlp not found. Install it or set YTDLP_PATH to enable playback.",
                    )
                    .color(Color32::from_rgb(230, 180, 100)),
                );
            }
            ui.add_space(4.0);
        });

        if go_back {
            self.view = View::Search;
        }
        if open_library {
            self.open_page(ctx, "FEmusic_library_landing".to_string());
        }
        if do_search {
            self.run_search(ctx);
        }
    }

    fn central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut action: Option<Action> = None;
        let view = std::mem::replace(&mut self.view, View::Search);
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &view {
                    View::Search => {
                        if self.results.is_empty() {
                            ui.add_space(48.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(if self.busy {
                                        "Searching…"
                                    } else {
                                        "Search YouTube Music to get started."
                                    })
                                    .weak(),
                                );
                            });
                        }
                        for (idx, item) in self.results.iter().enumerate() {
                            let selected = item
                                .video_id
                                .as_deref()
                                .map(|vid| {
                                    self.current
                                        .and_then(|c| self.queue.get(c))
                                        .map(|t| t.video_id == vid)
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            let trailing = item.duration_secs.map(crate::ytm::domain::fmt_duration);
                            let row = list_row(
                                ui,
                                &self.thumbnails,
                                ("search", idx),
                                item.thumbnail.as_deref(),
                                &item.title,
                                &item.subtitle,
                                trailing.as_deref(),
                                selected,
                                item.playable(),
                                item.navigable(),
                            );
                            if row.clicked() {
                                if item.playable() {
                                    if let Some(track) = item.to_track() {
                                        action = Some(Action::PlaySingle(track));
                                    }
                                } else if let Some(id) = item.browse_id.clone() {
                                    action = Some(Action::Open(id));
                                }
                            }
                        }
                    }
                    View::Detail(page) => {
                        detail_header(ui, page, &self.thumbnails);
                        for (idx, track) in page.tracks.iter().enumerate() {
                            let selected = self.current == Some(idx);
                            let trailing = track.duration_text();
                            let row = list_row(
                                ui,
                                &self.thumbnails,
                                ("track", idx),
                                track.thumbnail.as_deref(),
                                &track.title,
                                &track.artist_line(),
                                trailing.as_deref(),
                                selected,
                                true,
                                false,
                            );
                            if row.clicked() {
                                action = Some(Action::PlayQueueIndex(idx));
                            }
                        }

                        // Browsable carousels (artist albums, related, library…).
                        for (shelf_idx, shelf) in page.shelves.iter().enumerate() {
                            if shelf.items.is_empty() {
                                continue;
                            }
                            ui.add_space(10.0);
                            if !shelf.title.is_empty() {
                                ui.label(RichText::new(&shelf.title).strong().size(17.0));
                                ui.add_space(2.0);
                            }
                            for (item_idx, item) in shelf.items.iter().enumerate() {
                                let row = list_row(
                                    ui,
                                    &self.thumbnails,
                                    ("shelf", shelf_idx, item_idx),
                                    item.thumbnail.as_deref(),
                                    &item.title,
                                    &item.subtitle,
                                    None,
                                    false,
                                    item.playable(),
                                    item.navigable(),
                                );
                                if row.clicked() {
                                    if item.playable() {
                                        if let Some(track) = item.to_track() {
                                            action = Some(Action::PlaySingle(track));
                                        }
                                    } else if let Some(id) = item.browse_id.clone() {
                                        action = Some(Action::Open(id));
                                    }
                                }
                            }
                        }
                    }
                });
        });
        self.view = view;
        if let Some(action) = action {
            self.handle_action(ctx, action);
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut toggle_pause = false;
        let mut step = 0i32;
        let mut seek_to: Option<f32> = None;
        let mut volume = self.volume;

        egui::Panel::bottom("player").show(ui, |ui| {
            ui.add_space(6.0);
            let Some(audio) = self.audio.as_ref() else {
                ui.label(
                    RichText::new(
                        self.audio_error
                            .clone()
                            .unwrap_or_else(|| "Audio output unavailable".to_string()),
                    )
                    .weak(),
                );
                ui.add_space(6.0);
                return;
            };

            ui.horizontal(|ui| {
                if let Some(track) = &audio.now_playing {
                    if track.thumbnail.is_some() {
                        draw_thumbnail(
                            ui,
                            track.thumbnail.as_deref(),
                            &self.thumbnails,
                            egui::vec2(46.0, 46.0),
                            6.0,
                        );
                    }
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&track.title).strong());
                        ui.label(RichText::new(track.artist_line()).weak().small());
                    });
                } else {
                    ui.label(RichText::new("Nothing playing").weak());
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_sized(
                        egui::vec2(96.0, 18.0),
                        egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                    );
                    ui.label("🔊");
                    ui.separator();
                    if ui.button("⏭").clicked() {
                        step = 1;
                    }
                    let label = if audio.is_paused() { "▶" } else { "⏸" };
                    if ui.button(label).clicked() {
                        toggle_pause = true;
                    }
                    if ui.button("⏮").clicked() {
                        step = -1;
                    }
                });
            });

            ui.add_space(2.0);
            let total = audio.total.map(|d| d.as_secs_f32()).unwrap_or(0.0);
            let pos = audio.position().as_secs_f32();
            let mut new_pos = pos;
            let width = ui.available_width();
            let slider = ui.add_sized(
                egui::vec2(width, 18.0),
                egui::Slider::new(&mut new_pos, 0.0..=total.max(1.0)).show_value(false),
            );
            if slider.changed() {
                seek_to = Some(new_pos);
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new(fmt_time(pos)).weak().small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(fmt_time(total)).weak().small());
                });
            });
            ui.add_space(6.0);
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

        self.drain_messages(&ctx);
        self.ensure_thumbnails(&ctx);
        self.tick_playback(&ctx);
        self.top_bar(ui, &ctx);
        self.bottom_bar(ui, &ctx);
        self.central(ui, &ctx);

        let playing = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        if self.busy || !self.downloading.is_empty() || playing {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn fmt_time(secs: f32) -> String {
    let secs = secs.max(0.0) as u32;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Draw a cached thumbnail, or a neutral placeholder while it loads.
fn draw_thumbnail(
    ui: &mut egui::Ui,
    uri: Option<&str>,
    thumbs: &HashMap<String, egui::TextureHandle>,
    size: egui::Vec2,
    radius: f32,
) {
    match uri.and_then(|u| thumbs.get(u)) {
        Some(texture) => {
            ui.add(
                egui::Image::from_texture(texture)
                    .fit_to_exact_size(size)
                    .corner_radius(radius),
            );
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, radius, ui.visuals().widgets.inactive.bg_fill);
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
            draw_thumbnail(
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

/// One clickable list row with an optional thumbnail.
#[allow(clippy::too_many_arguments)]
fn list_row(
    ui: &mut egui::Ui,
    thumbs: &HashMap<String, egui::TextureHandle>,
    id: impl std::hash::Hash + std::fmt::Debug,
    thumb: Option<&str>,
    title: &str,
    subtitle: &str,
    trailing: Option<&str>,
    selected: bool,
    playable: bool,
    navigable: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 52.0), egui::Sense::click());
    let hovered = response.hovered();
    let visuals = ui.visuals();
    if selected {
        ui.painter()
            .rect_filled(rect, 6.0, visuals.selection.bg_fill);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 6.0, visuals.widgets.hovered.bg_fill);
    }

    let inner = rect.shrink2(egui::vec2(6.0, 4.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .id_salt(id)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            draw_thumbnail(ui, thumb, thumbs, egui::vec2(40.0, 40.0), 4.0);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong());
                if !subtitle.is_empty() {
                    ui.label(RichText::new(subtitle).weak().small());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(t) = trailing {
                    ui.label(RichText::new(t).weak().small());
                }
                if hovered && playable {
                    ui.label(RichText::new("▶").weak());
                } else if hovered && navigable {
                    ui.label(RichText::new("›").weak().size(18.0));
                }
            });
        },
    );

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}
