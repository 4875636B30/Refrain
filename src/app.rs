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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use egui::{Align, Layout, RichText};

use crate::audio::{AudioEngine, resolver};
use crate::auth::ResolvedAuth;
use crate::config::{AuthConfig, AuthMode, Config};
use crate::config::{KNOWN_BROWSERS, KNOWN_KEYRINGS, keyring_label};
use crate::media;
use crate::thumbs;
use crate::ui;
use crate::update;
use crate::ytm::domain::LikeStatus;
use crate::ytm::{Client, ItemKind, Lyrics, Page, SearchFilter, SearchItem, Track};

enum Msg {
    Search(Result<SearchResults, String>),
    Page(Result<Page, String>),
    Downloaded {
        video_id: String,
        url: Option<String>,
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
    /// Further tracks loaded for a long playlist.
    MoreTracks {
        browse_id: String,
        result: Result<Vec<Track>, String>,
    },
    /// The background cache worker drained its queue.
    CacheIdle,
    /// A request from the desktop's media controls (media keys, panel, MPRIS).
    Media(media::Command),
    /// The user's playlists, for the picker.
    Playlists(Result<Vec<(String, String)>, String>),
    /// A playlist edit finished; the string is what to tell the user.
    PlaylistEdited(Result<String, String>),
    /// The version the chosen yt-dlp reports, for the settings window.
    YtdlpVersion(Option<String>),
    /// The lyrics for the current track, or why there are none.
    Lyrics(Result<Option<Lyrics>, String>),
    /// More of the same kind of music, for when a queue runs out.
    Radio(Result<Option<Vec<Track>>, String>),
    /// The answer to "is there a newer release", or why we could not ask.
    UpdateChecked(Result<Option<update::Release>, String>),
    /// The result of installing one, with the version it was.
    UpdateInstalled(Result<(String, update::Outcome), String>),
    /// Further search results, appended to the current ones.
    MoreResults(Result<(Vec<SearchItem>, Option<String>), String>),
    /// A playlist was deleted. Its page has to go away if it is open.
    DeletedPlaylist {
        playlist_id: String,
        result: Result<String, String>,
    },
    /// The rating for a track, read from the watch-next response.
    LikeStatus {
        video_id: String,
        result: Result<(LikeStatus, bool), String>,
    },
    /// A rating change came back from the server.
    LikeSet {
        video_id: String,
        result: Result<(), String>,
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

/// Resolve-and-cache tasks allowed to run at once when warming tracks in the
/// background. Each one is mostly network plus a `node` subprocess, so a pool
/// is the difference between warming a page in ~40s and ~20s.
///
/// Two rather than three: measured throughput was 1.58x versus 1.67x, which is
/// not worth a third concurrent `node`. Those processes are CPU-heavy enough to
/// starve the audio reader thread, and a buffer underrun is far more noticeable
/// than warming the next track a second later.
const MAX_CACHE_WORKERS: usize = 2;

/// How often the cache budget is enforced, at the quiet moment after the warm
/// queue drains. Startup prunes unconditionally.
const PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How often the desktop's media widget is refreshed. MPRIS has no
/// position-changed signal, so clients poll; once a second is plenty.
const MEDIA_PUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Beyond this, the chosen yt-dlp is old enough to warn about: YouTube changes
/// and yt-dlp has to keep up, so a stale copy is the likeliest reason playback
/// would stop working.
const STALE_YTDLP_DAYS: u64 = 90;

/// How long a click waits for sign-in before playing without it.
const DEFERRED_PLAY_TIMEOUT: Duration = Duration::from_secs(8);

/// How many times we page in more shelves for a feed (home/explore/library).
const MAX_MORE_ROUNDS: u32 = 5;

/// Rounds when paging the rest of a long playlist (~50 tracks each).
const MAX_TRACK_ROUNDS: u32 = 10;

/// Artwork textures kept in memory (~160x160 px each). Feeds list many covers,
/// so this bounds growth as the user browses.
const MAX_THUMBS: usize = 200;

/// Artwork is fetched with reqwest (the same stack used for the API) and
/// uploaded as a texture, rather than relying on egui's built-in HTTP loader.
const IMAGE_UA: &str = thumbs::USER_AGENT;

// `Page` is a few hundred bytes; boxing it would churn every match site for no
// real benefit, and only one page is held at a time.
#[allow(clippy::large_enum_variant)]
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
    /// Page in the rest of a long track list.
    LoadMoreTracks,
    /// Open the "add to playlist" picker for a track.
    AddToPlaylist {
        video_id: String,
        title: String,
    },
    /// Take a track out of the playlist being viewed.
    RemoveFromPlaylist {
        playlist_id: String,
        video_id: String,
        /// The entry's id within the playlist; without it the server refuses.
        set_video_id: Option<String>,
        title: String,
    },
    /// Open the rename dialog for a playlist.
    RenamePlaylist {
        playlist_id: String,
        title: String,
    },
    /// Ask before deleting a playlist.
    DeletePlaylist {
        playlist_id: String,
        title: String,
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

/// How the queue advances when a track ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Off,
    All,
    One,
}

/// The window handle souvlaki needs on Windows, and nowhere else.
///
/// Windows is the only platform that requires one; asking for it elsewhere
/// would be a dependency and an unsafe cast for nothing.
fn window_handle(cc: &eframe::CreationContext<'_>) -> Option<*mut std::ffi::c_void> {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        if let Ok(handle) = cc.window_handle()
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            return Some(win32.hwnd.get() as *mut std::ffi::c_void);
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cc;
        None
    }
}

/// Resolve browser cookies in the background and report back on the channel.
///
/// `fresh` forces a new extraction instead of reusing the cached jar, which is
/// what the settings panel's "refresh sign-in" needs.
fn spawn_auth(
    rt: &tokio::runtime::Runtime,
    cfg: Config,
    tx: Sender<Msg>,
    ctx: egui::Context,
    fresh: bool,
) {
    rt.spawn(async move {
        let (lang, country) = Config::locale();
        let started = std::time::Instant::now();
        let mut resolved = if fresh {
            crate::auth::resolve_fresh(&cfg).await
        } else {
            crate::auth::resolve(&cfg).await
        };

        // A reused jar is only trusted if InnerTube still accepts it.
        let stale = match &resolved {
            Ok(Some(auth)) => {
                match crate::ytm::Client::new(&lang, &country, Some(auth.jar.clone())) {
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

/// The user's playlists, as (browse id, title), from the library page.
///
/// "Liked Music" is an auto playlist driven by ratings rather than a real list,
/// so it is left out — adding to it is not how a track gets liked.
fn playlists_from(page: &Page) -> Vec<(String, String)> {
    page.shelves
        .iter()
        .flat_map(|shelf| shelf.items.iter())
        .filter(|item| item.kind == ItemKind::Playlist)
        .filter(|item| !item.subtitle.to_lowercase().contains("auto playlist"))
        .filter_map(|item| {
            item.browse_id
                .as_ref()
                .map(|id| (id.clone(), item.title.clone()))
        })
        .collect()
}

/// What the scripted demo searches for. A well-known artist, so the results are
/// the same for everyone — the point of the demo is that it shows *nobody's*
/// library.
const DEMO_QUERY: &str = "daft punk";

/// One step of the scripted demo.
#[derive(Clone, Copy)]
enum DemoStep {
    Search,
    OpenAlbum,
    Play,
    ShowQueue,
}

/// A scripted run, for screenshots and recordings.
///
/// It exists so a demo can be reproduced on demand, and so the screenshot in the
/// README does not have to be someone's home feed — which is personalised, and
/// therefore publishes their taste. Everything it shows is a search for a
/// well-known artist: real data, nobody's history.
struct Demo {
    step: usize,
    /// When the current step began waiting, for the timeout above.
    step_started: Instant,
}

impl Demo {
    fn new() -> Self {
        Self {
            step: 0,
            step_started: Instant::now(),
        }
    }

    /// The steps, in order.
    const STEPS: [DemoStep; 4] = [
        DemoStep::Search,
        DemoStep::OpenAlbum,
        DemoStep::Play,
        DemoStep::ShowQueue,
    ];

    /// A step is skipped if its condition never arrives, so the demo cannot
    /// stall on a search that came back without an album.
    const STEP_TIMEOUT: Duration = Duration::from_secs(20);

    /// The shortest gap between steps, so they are visible rather than
    /// instantaneous — this is a recording, after all.
    const STEP_GAP: Duration = Duration::from_millis(1200);
}

/// The "add to playlist" dialog.
struct Picker {
    video_id: String,
    title: String,
    /// The user's playlists as (browse id, title); `None` until loaded.
    playlists: Option<Vec<(String, String)>>,
    /// What is being typed into the "new playlist" field.
    new_name: String,
    /// Progress or failure, shown in the dialog.
    note: Option<String>,
    busy: bool,
}

/// What should follow the current track when it ends.
#[derive(Debug, PartialEq, Eq)]
enum Advance {
    /// Play this index in the queue.
    Play(usize),
    /// Nothing left to play.
    Stop,
}

/// Decide what plays after the current track.
///
/// Kept apart from the UI loop so the rules are actually testable: repeat-one
/// always wins, shuffle supplies its own index, and repeat-all wraps at the
/// end of the queue. Getting this wrong is invisible until a queue ends.
/// What the lyrics panel has for the track that is playing.
#[derive(Clone, Debug, PartialEq)]
enum LyricsState {
    /// Not asked for yet.
    Idle,
    Loading,
    /// The track has none — instrumentals mostly.
    None,
    Ready(Lyrics),
    Failed(String),
}

/// Seconds since the epoch, for the update-check throttle.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// What is known about a newer release.
#[derive(Clone, Debug, PartialEq)]
enum UpdateState {
    /// Nothing asked yet, or the check is not due.
    Idle,
    Checking,
    /// A newer release exists.
    Available(update::Release),
    /// This is the newest release.
    Current,
    /// Downloaded, but not installed, and why.
    Saved(std::path::PathBuf, String),
    Failed(String),
    Installing,
    /// In place; it takes effect on the next start.
    Installed(String),
}

/// Which queue entry a track belongs at after the output device was rebuilt.
///
/// The device dying does not invalidate the run: the queue and the index into
/// it are still good, so the recovered track keeps its place and the rest of
/// the queue still plays after it. Rebuilding around a single track instead —
/// which is what this used to do — meant that when the recovered track ended,
/// there was nothing left to advance to and playback stopped for good.
///
/// `None` means the queue no longer holds this track, and the caller has to
/// start a new run.
fn recovery_index(queue: &[Track], current: Option<usize>, video_id: &str) -> Option<usize> {
    match current {
        Some(index) if queue.get(index).is_some_and(|t| t.video_id == video_id) => Some(index),
        _ => None,
    }
}

fn advance_decision(
    repeat: Repeat,
    shuffle_index: Option<usize>,
    queue_len: usize,
    current: Option<usize>,
) -> Advance {
    if repeat == Repeat::One
        && let Some(current) = current
    {
        return Advance::Play(current);
    }
    if let Some(index) = shuffle_index {
        return Advance::Play(index);
    }
    match current {
        Some(index) if index + 1 < queue_len => Advance::Play(index + 1),
        Some(_) if repeat == Repeat::All && queue_len > 0 => Advance::Play(0),
        _ => Advance::Stop,
    }
}

/// A pseudo-random queue index, without pulling in a dependency.
fn random_index(len: usize, avoid: Option<usize>) -> usize {
    if len <= 1 {
        return 0;
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) ^ d.as_secs())
        .unwrap_or(1);
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    let mut index = (seed % len as u64) as usize;
    if Some(index) == avoid {
        index = (index + 1) % len;
    }
    index
}

/// One category of search results.
struct SearchSection {
    filter: SearchFilter,
    items: Vec<SearchItem>,
}

/// Which item kind a category holds.
fn kind_for_filter(filter: SearchFilter) -> Option<ItemKind> {
    match filter {
        SearchFilter::Songs => Some(ItemKind::Song),
        SearchFilter::Videos => Some(ItemKind::Video),
        SearchFilter::Albums => Some(ItemKind::Album),
        SearchFilter::Artists => Some(ItemKind::Artist),
        SearchFilter::Playlists => Some(ItemKind::Playlist),
        SearchFilter::All => None,
    }
}

/// Run a search: one request for a chosen category, or a single "All" request
/// that is then grouped into categories.
///
/// Grouping the "All" response preserves YouTube's own relevance order, which is
/// noticeably better than asking for each category separately.
/// A search response: grouped for display, the raw list, and the next page.
struct SearchResults {
    sections: Vec<SearchSection>,
    /// Kept so more results can be folded into the grouping.
    items: Vec<SearchItem>,
    more: Option<String>,
}

/// Group a flat search response into the categories the UI shows.
fn group_items(items: &[SearchItem]) -> Vec<SearchSection> {
    let mut sections = Vec::new();
    for category in [
        SearchFilter::Songs,
        SearchFilter::Videos,
        SearchFilter::Albums,
        SearchFilter::Artists,
        SearchFilter::Playlists,
    ] {
        let Some(kind) = kind_for_filter(category) else {
            continue;
        };
        let picked: Vec<SearchItem> = items
            .iter()
            .filter(|item| item.kind == kind)
            .cloned()
            .collect();
        if !picked.is_empty() {
            sections.push(SearchSection {
                filter: category,
                items: picked,
            });
        }
    }
    sections
}

async fn search_sections(
    client: &Client,
    query: &str,
    filter: SearchFilter,
) -> Result<SearchResults, String> {
    let (items, more) = client
        .search(query, filter)
        .await
        .map_err(|e| e.to_string())?;

    // One request when a single category is asked for; the "All" view groups a
    // single response client-side, which measured better than per-category
    // requests.
    let sections = if filter == SearchFilter::All {
        group_items(&items)
    } else {
        vec![SearchSection {
            filter,
            items: items.clone(),
        }]
    };
    Ok(SearchResults {
        sections,
        items,
        more,
    })
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
                "Search for music to get started."
            })
            .weak(),
        );
    });
}

fn nav_button(ui: &mut egui::Ui, current: &mut Nav, value: Nav, label: &str) -> bool {
    let selected = *current == value;
    let text = RichText::new(label).size(15.0);
    let text = if selected { text.strong() } else { text };
    let width = ui.available_width();
    if ui
        .add_sized([width, 30.0], egui::Button::selectable(selected, text))
        .clicked()
    {
        *current = value;
        return true;
    }
    false
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
    /// When the current deferral started, so a sign-in that never completes
    /// can't strand the click forever.
    deferred_since: Option<Instant>,

    query: String,
    filter: SearchFilter,
    /// Last query run, so it can be repeated once sign-in completes.
    last_search: Option<(String, SearchFilter)>,
    nav: Nav,
    theme_light: bool,
    theme_applied: bool,
    view: View,
    /// Search results, grouped by category.
    sections: Vec<SearchSection>,
    /// The multi-category view, kept so Back can return to it from "See more".
    prev_sections: Option<Vec<SearchSection>>,

    status: Option<String>,
    busy: bool,

    queue: Vec<Track>,
    current: Option<usize>,
    pending_play: Option<Track>,
    downloading: HashSet<String>,
    /// Resolved stream URLs, reusable for a few hours.
    url_cache: HashMap<String, (String, Instant)>,
    /// Tracks queued for background resolve-and-cache.
    cache_queue: Arc<Mutex<VecDeque<String>>>,
    /// Tracks a worker has claimed but not finished (prevents duplicate work).
    cache_inflight: Arc<Mutex<HashSet<String>>>,
    /// When the cache budget was last enforced.
    last_prune: Instant,
    /// Whether the "Up next" queue panel is showing.
    show_queue: bool,
    /// Whether the panel is showing lyrics rather than the queue.
    show_lyrics: bool,
    lyrics: LyricsState,
    /// Which track the lyrics in hand belong to, so they are fetched once.
    lyrics_for: Option<String>,
    /// Keep playing similar music when the queue runs out.
    autoplay: bool,
    /// `--fps`: report the frame rate the platform actually delivers.
    fps_probe: bool,
    fps_frames: u32,
    fps_since: Instant,
    /// A radio fetch is on its way.
    radio_inflight: bool,
    /// The track a radio fetch was last started for, so a queue that ends
    /// twice does not ask twice.
    radio_anchored: Option<String>,
    /// Whether the sign-in attempt has finished, successfully or not. The demo
    /// waits for this: signing in re-runs the last search, which would otherwise
    /// yank the view back while the script is navigating.
    auth_settled: bool,
    /// The sign-in *preference* from the config, as opposed to `auth`, which is
    /// the source that actually resolved. Editing the resolved one would
    /// silently turn "detect automatically" into a pinned browser.
    auth_pref: AuthConfig,
    /// The "add to playlist" picker, when it is open.
    picker: Option<Picker>,
    /// A pending rename: (playlist id, name being edited).
    renaming: Option<(String, String)>,
    /// A pending delete: (playlist id, title), waiting for confirmation.
    deleting: Option<(String, String)>,
    /// The flat result list and the token for the next page, so "load more"
    /// can fold new results into the same grouping.
    search_items: Vec<SearchItem>,
    search_more: Option<String>,
    want_more_results: bool,
    /// The scripted demo, when one is running (see `Demo`).
    demo: Option<Demo>,
    /// Whether the settings window is showing, and what the cache holds.
    show_settings: bool,
    update_state: UpdateState,
    /// What an update may do to this installation, worked out once at startup.
    update_kind: update::InstallKind,
    /// When the last check happened, for the once-a-day rule.
    last_update_check: Option<u64>,
    cache_usage: (usize, u64),
    /// yt-dlp's self-reported version, asked for on demand.
    ytdlp_version: Option<String>,
    /// Desktop media controls, if the desktop has any.
    media: media::Controls,
    /// The track the desktop is currently showing.
    media_for: Option<String>,
    last_media_push: Instant,
    /// The volume the desktop was last told about.
    media_volume: f32,
    /// The rating shown for the playing track, and whether rating is allowed.
    like_status: LikeStatus,
    likes_allowed: bool,
    /// The track the rating belongs to, so it is refetched when that changes.
    like_for: Option<String>,
    like_inflight: bool,
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
    /// Rounds of playlist paging for the current page.
    track_rounds: u32,
    shuffle: bool,
    repeat: Repeat,
    /// Visited views, so Back returns where the user came from.
    view_history: Vec<(Nav, View)>,
    /// Whether the home feed still needs reloading now that we're signed in.
    home_needs_reload: bool,
    advance_armed: bool,

    audio: Option<AudioEngine>,
    audio_error: Option<String>,
    volume: f32,
    /// Last non-zero volume, for the mute toggle.
    last_volume: f32,
    /// A setting changed and the config has not caught up yet.
    pending_save: bool,
    /// When it changed, so one drag does not write the file every frame.
    pending_save_at: Instant,
    /// When we last rebuilt the audio device, to avoid hot-looping.
    last_audio_recovery: Option<Instant>,
    /// Count of transient buffer underruns, for throttled logging.
    underruns: u32,
    /// Last artwork scan, so per-frame work stays cheap.
    last_thumb_scan: Instant,

    img_http: reqwest::Client,
    thumbnails: HashMap<String, egui::TextureHandle>,
    /// Insertion order of `thumbnails`, for eviction.
    thumb_order: VecDeque<String>,
    /// Artwork the current view needs, so eviction leaves it alone.
    wanted_thumbs: HashSet<String>,
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
        let (lang, country) = Config::locale();
        let client = Arc::new(Client::new(&lang, &country, None)?);

        let (audio, audio_error) = match AudioEngine::new() {
            Ok(engine) => (Some(engine), None),
            Err(e) => (None, Some(format!("Audio output unavailable: {e}"))),
        };
        let volume = cfg.ui.volume;
        let img_http = reqwest::Client::builder()
            .user_agent(IMAGE_UA)
            .build()
            .context("building image HTTP client")?;

        // Fonts are installed here rather than on the first frame: `set_fonts`
        // only takes effect on the *next* frame, so any heading drawn in the
        // same frame that installs them would ask for a family that does not
        // exist yet — which egui treats as a panic, not a fallback.
        ui::install_fonts(&cc.egui_ctx);

        let ytdlp = resolver::locate_ytdlp();
        if let Some(path) = &ytdlp {
            // Say which copy was chosen. With several installed, "it is using
            // the wrong yt-dlp" is a common and otherwise invisible problem.
            eprintln!("[yt-dlp] using {}", path.display());
        }
        resolver::clean_stale_parts();
        // Enforcing the cache budget walks the whole directory, so keep it off
        // the startup path.
        std::thread::spawn(|| {
            let (removed, freed) = resolver::prune_cache();
            if removed > 0 {
                eprintln!(
                    "[cache] evicted {removed} tracks ({:.0} MB)",
                    freed as f64 / 1e6
                );
            }
        });
        let status = if ytdlp.is_none() {
            Some(
                "yt-dlp not found — playback is disabled. Install it or set YTDLP_PATH."
                    .to_string(),
            )
        } else {
            None
        };

        let (tx, rx) = channel();

        // Desktop media controls: media keys, the panel's media widget, and
        // `playerctl`. Requests arrive on the same channel as everything else,
        // so they are handled on the UI thread like any other input.
        let media_ctx = cc.egui_ctx.clone();
        let media_tx = tx.clone();
        let media = media::Controls::new(window_handle(cc), move |command| {
            let _ = media_tx.send(Msg::Media(command));
            media_ctx.request_repaint();
        });
        if media.is_connected() {
            eprintln!("[media] desktop controls connected");
        }

        // Resolve browser cookies off the UI thread so startup stays instant.
        spawn_auth(&rt, cfg.clone(), tx.clone(), cc.egui_ctx.clone(), false);

        // Playback is handled by yt-dlp reading the browser directly, so a
        // previously-discovered source lets it start immediately without
        // waiting for the (slower) InnerTube cookie extraction.
        let auth_pref = cfg.auth.clone();
        let auth = crate::auth::cached_source().unwrap_or_else(|| cfg.auth.clone());
        let theme_light = cfg.ui.theme == "light";
        let mut app = Self {
            rt,
            client,
            tx,
            rx,
            ytdlp,
            auth,
            auth_settled: false,
            auth_pref,
            authenticated: false,
            deferred_play: None,
            deferred_since: None,
            query: String::new(),
            filter: SearchFilter::All,
            last_search: None,
            nav: Nav::Home,
            theme_light,
            theme_applied: false,
            view: View::Search,
            sections: Vec::new(),
            prev_sections: None,
            status,
            busy: false,
            queue: Vec::new(),
            current: None,
            pending_play: None,
            downloading: HashSet::new(),
            url_cache: HashMap::new(),
            cache_queue: Arc::new(Mutex::new(VecDeque::new())),
            cache_inflight: Arc::new(Mutex::new(HashSet::new())),
            last_prune: Instant::now(),
            show_queue: false,
            show_lyrics: false,
            lyrics: LyricsState::Idle,
            lyrics_for: None,
            autoplay: cfg.ui.autoplay,
            fps_probe: std::env::args().any(|arg| arg == "--fps"),
            fps_frames: 0,
            fps_since: Instant::now(),
            radio_inflight: false,
            radio_anchored: None,
            picker: None,
            renaming: None,
            deleting: None,
            search_items: Vec::new(),
            search_more: None,
            want_more_results: false,
            demo: std::env::args().any(|arg| arg == "--demo").then(Demo::new),
            show_settings: false,
            update_state: UpdateState::Idle,
            update_kind: update::install_kind(),
            last_update_check: cfg.ui.last_update_check,
            cache_usage: (0, 0),
            ytdlp_version: None,
            media,
            media_for: None,
            last_media_push: Instant::now(),
            // Impossible, so the first tick publishes the real volume.
            media_volume: -1.0,
            like_status: LikeStatus::Indifferent,
            likes_allowed: false,
            like_for: None,
            like_inflight: false,
            prefetched: Vec::new(),
            play_counts: load_play_counts(),
            history: Vec::new(),
            prefetch_inflight: HashSet::new(),
            hover_target: None,
            more_rounds: 0,
            track_rounds: 0,
            shuffle: false,
            repeat: Repeat::Off,
            view_history: Vec::new(),
            home_needs_reload: false,
            advance_armed: false,
            audio,
            audio_error,
            volume,
            last_volume: if volume > 0.001 { volume } else { 0.8 },
            pending_save: false,
            pending_save_at: Instant::now(),
            last_audio_recovery: None,
            underruns: 0,
            last_thumb_scan: Instant::now() - Duration::from_secs(1),
            img_http,
            thumbnails: HashMap::new(),
            thumb_order: VecDeque::new(),
            wanted_thumbs: HashSet::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
        };
        // Open on the home feed rather than a blank search page.
        // The demo script navigates for itself; loading Home first would only
        // flash a personalised feed on screen.
        if app.demo.is_none() {
            app.load_page(&cc.egui_ctx, "FEmusic_home".to_string(), false);
        }
        Ok(app)
    }

    // ---------------------------------------------------------------- actions

    /// Fetch the next page of the current search.
    fn spawn_more_results(&mut self, ctx: &egui::Context) {
        let Some(token) = self.search_more.clone() else {
            return;
        };
        self.status = Some("Loading more results…".to_string());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client.search_more(&token).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::MoreResults(result));
            ctx.request_repaint();
        });
    }

    fn run_search(&mut self, ctx: &egui::Context) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.busy = true;
        self.status = Some(format!("Searching “{query}”…"));
        let previous_filter = self.last_search.as_ref().map(|(_, filter)| *filter);
        // Drilling into a category keeps the multi-category view for Back.
        if self.filter != SearchFilter::All && previous_filter == Some(SearchFilter::All) {
            self.prev_sections = Some(std::mem::take(&mut self.sections));
        }
        if self.filter == SearchFilter::All {
            self.prev_sections = None;
        }
        self.last_search = Some((query.clone(), self.filter));
        self.search_items.clear();
        self.search_more = None;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let filter = self.filter;
        self.rt.spawn(async move {
            let result = search_sections(&client, &query, filter).await;
            let _ = tx.send(Msg::Search(result));
            ctx.request_repaint();
        });
    }

    /// Fetch a detail page. `remember` pushes the current view so Back returns
    /// to where the user came from rather than always to search.
    fn load_page(&mut self, ctx: &egui::Context, browse_id: String, remember: bool) {
        if remember {
            let current = std::mem::replace(&mut self.view, View::Search);
            if matches!(&current, View::Detail(page) if page.browse_id == browse_id) {
                self.view = current; // don't stack duplicates
            } else {
                self.view_history.push((self.nav, current));
                if self.view_history.len() > 32 {
                    self.view_history.remove(0);
                }
            }
        }
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

    fn open_page(&mut self, ctx: &egui::Context, browse_id: String) {
        self.load_page(ctx, browse_id, true);
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
                    Ok(source) => {
                        // Ask the stream itself on failure: rodio reports a
                        // failed reader as an unrecognised format, which hides
                        // the actual reason.
                        let failure = source.failure_handle();
                        rodio::Decoder::new(source)
                            .map_err(|e| failure.get().unwrap_or_else(|| e.to_string()))
                    }
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

    fn spawn_more_tracks(
        &mut self,
        ctx: &egui::Context,
        browse_id: String,
        playlist_id: String,
        anchor_video_id: String,
    ) {
        self.busy = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .playlist_more(&anchor_video_id, &playlist_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::MoreTracks { browse_id, result });
            ctx.request_repaint();
        });
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

    /// A stream URL resolved recently enough to still be valid.
    fn cached_url(&self, video_id: &str) -> Option<String> {
        self.url_cache.get(video_id).and_then(|(url, at)| {
            // YouTube URLs live about six hours; stay well clear.
            (at.elapsed() < Duration::from_secs(5 * 3600)).then(|| url.clone())
        })
    }

    /// Queue tracks to be resolved and cached in the background.
    ///
    /// A whole track downloads in under a second once its URL is known, so this
    /// is cheap per track; the expensive part is the URL resolution.
    fn enqueue_cache(&mut self, ctx: &egui::Context, video_ids: Vec<String>) {
        for video_id in video_ids {
            if resolver::cache_path(&video_id).exists()
                || self.downloading.contains(&video_id)
                || self.cache_inflight.lock().unwrap().contains(&video_id)
            {
                continue;
            }
            let mut queue = self.cache_queue.lock().unwrap();
            if !queue.contains(&video_id) {
                queue.push_back(video_id);
            }
        }
        self.pump_cache_worker(ctx);
    }

    /// Resolve-and-cache tracks concurrently.
    ///
    /// A resolve is mostly waiting on the network and a node subprocess, so
    /// running a few at once raises warming throughput far more than batching
    /// them into one yt-dlp invocation does: the marginal cost of each extra
    /// track is the per-video JS challenge (~4.5s), not process startup.
    fn pump_cache_worker(&mut self, ctx: &egui::Context) {
        loop {
            let mut inflight = self.cache_inflight.lock().unwrap();
            if inflight.len() >= MAX_CACHE_WORKERS {
                break;
            }
            let Some(video_id) = self.cache_queue.lock().unwrap().pop_front() else {
                break;
            };
            inflight.insert(video_id.clone());
            drop(inflight);

            let tx = self.tx.clone();
            let ctx = ctx.clone();
            let auth = self.auth.clone();
            let known = self.cached_url(&video_id);
            let inflight = Arc::clone(&self.cache_inflight);
            self.rt.spawn(async move {
                let (url, result) = resolver::resolve_and_fetch(&video_id, &auth, known).await;
                let result = result.map_err(|e| e.to_string());
                inflight.lock().unwrap().remove(&video_id);
                let _ = tx.send(Msg::Downloaded {
                    video_id,
                    url,
                    result,
                });
                let _ = tx.send(Msg::CacheIdle);
                ctx.request_repaint();
            });
        }

        // Once the queue has drained, nothing is being written and it is a safe
        // moment to enforce the cache budget. Throttled, since this walks the
        // whole directory.
        if self.cache_queue.lock().unwrap().is_empty()
            && self.cache_inflight.lock().unwrap().is_empty()
            && self.last_prune.elapsed() > PRUNE_INTERVAL
        {
            self.last_prune = Instant::now();
            std::thread::spawn(|| {
                let (removed, freed) = resolver::prune_cache();
                if removed > 0 {
                    eprintln!(
                        "[cache] evicted {removed} tracks ({:.0} MB)",
                        freed as f64 / 1e6
                    );
                }
            });
        }
    }

    fn spawn_download(&mut self, ctx: &egui::Context, video_id: String) {
        if !self.downloading.insert(video_id.clone()) {
            return; // already in flight
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let auth = self.auth.clone();
        let cached = self.cached_url(&video_id);
        self.rt.spawn(async move {
            let (url, result) = resolver::resolve_and_fetch(&video_id, &auth, cached).await;
            let result = result.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Downloaded {
                video_id,
                url,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn play_index(&mut self, ctx: &egui::Context, idx: usize) {
        self.play_at(ctx, idx, false);
    }

    /// Play `idx`, optionally skipping the wait for sign-in.
    fn play_at(&mut self, ctx: &egui::Context, idx: usize, anonymous_ok: bool) {
        let Some(track) = self.queue.get(idx).cloned() else {
            return;
        };
        let path = resolver::cache_path(&track.video_id);
        // Cached tracks need no network, so they can play immediately. Anything
        // else must wait until we know a concrete cookie source, or the
        // anonymous download would be bot-blocked.
        if !path.exists() && !anonymous_ok && matches!(self.auth.mode, AuthMode::Auto) {
            self.deferred_play = Some(idx);
            self.deferred_since = Some(Instant::now());
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
        } else if self
            .cache_inflight
            .lock()
            .unwrap()
            .contains(&track.video_id)
        {
            // A background warm is already resolving this track; wait for that
            // instead of starting a second yt-dlp for the same video.
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
        // And cache the few after that; a URL plus a ranged fetch is cheap.
        let upcoming: Vec<String> = self
            .queue
            .iter()
            .skip(idx + 1)
            .take(6)
            .map(|track| track.video_id.clone())
            .collect();
        self.enqueue_cache(ctx, upcoming);
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
        let len = self.queue.len();
        let next = if delta > 0 && self.shuffle {
            random_index(len, self.current)
        } else {
            match self.current {
                Some(current) => {
                    let candidate = current as i64 + i64::from(delta);
                    if candidate < 0 {
                        if self.repeat == Repeat::All {
                            len - 1
                        } else {
                            0
                        }
                    } else if candidate as usize >= len {
                        if self.repeat == Repeat::All {
                            0
                        } else {
                            len - 1
                        }
                    } else {
                        candidate as usize
                    }
                }
                None => 0,
            }
        };
        self.play_index(ctx, next);
    }

    /// Space play/pause, arrows seek or skip, `/` search, `Esc` back.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Don't steal keys while the user is typing.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, left, right, escape, slash, shift, queue, settings) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Slash),
                i.modifiers.shift,
                i.key_pressed(egui::Key::Q),
                i.key_pressed(egui::Key::Comma),
            )
        });

        if queue {
            self.show_queue = !self.show_queue;
        }
        if settings {
            if self.show_settings {
                self.show_settings = false;
            } else {
                self.cache_usage = resolver::cache_usage();
                self.spawn_ytdlp_version(ctx);
                self.show_settings = true;
            }
        }

        if slash {
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(ui::SEARCH_ID)));
            return;
        }
        if escape {
            // Whatever is on top closes first: a dialog that ignores Esc feels
            // stuck, and the view behind it should not move.
            if self.show_settings {
                self.show_settings = false;
                return;
            }
            if self.picker.is_some() {
                self.picker = None;
                return;
            }
            if self.renaming.take().is_some() || self.deleting.take().is_some() {
                return;
            }
            if let Some((nav, view)) = self.view_history.pop() {
                self.nav = nav;
                self.view = view;
            } else {
                self.view = View::Search;
            }
            return;
        }
        if space && let Some(audio) = self.audio.as_ref() {
            audio.toggle_pause();
        }
        if right {
            if shift {
                self.step_track(ctx, 1);
            } else if let Some(audio) = self.audio.as_ref() {
                audio.seek(audio.position() + Duration::from_secs(5));
            }
        }
        if left {
            if shift {
                self.step_track(ctx, -1);
            } else if let Some(audio) = self.audio.as_ref() {
                let position = audio.position().saturating_sub(Duration::from_secs(5));
                audio.seek(position);
            }
        }
    }

    fn handle_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::AddToPlaylist { video_id, title } => self.open_picker(ctx, video_id, title),
            Action::RemoveFromPlaylist {
                playlist_id,
                video_id,
                set_video_id,
                title,
            } => {
                // Without the entry id the server rejects the removal, so this
                // only runs where the row actually carried one.
                if let Some(set_video_id) = set_video_id
                    && self.authenticated
                {
                    let client = self.client.clone();
                    let tx = self.tx.clone();
                    let ctx = ctx.clone();
                    self.rt.spawn(async move {
                        let result = client
                            .remove_from_playlist(&playlist_id, &video_id, &set_video_id)
                            .await
                            .map(|()| format!("Removed “{title}” from the playlist"))
                            .map_err(|e| e.to_string());
                        let _ = tx.send(Msg::PlaylistEdited(result));
                        ctx.request_repaint();
                    });
                }
            }
            Action::RenamePlaylist { playlist_id, title } => {
                self.renaming = Some((playlist_id, title));
            }
            Action::DeletePlaylist { playlist_id, title } => {
                self.deleting = Some((playlist_id, title));
            }
            Action::Open(id) => self.open_page(ctx, id),
            Action::PlayQueueIndex(idx) => self.play_index(ctx, idx),
            Action::PlayList { tracks, index } => {
                self.queue = tracks;
                self.play_index(ctx, index);
            }
            Action::LoadMoreTracks => {
                if let View::Detail(page) = &self.view
                    && let Some(playlist_id) = page.playlist_id.clone()
                    && let Some(anchor) = page.tracks.last().map(|track| track.video_id.clone())
                {
                    self.track_rounds = 0;
                    self.spawn_more_tracks(ctx, page.browse_id.clone(), playlist_id, anchor);
                }
            }
        }
    }

    /// The output device can fail mid-playback — `alsa::poll()` returning
    /// `POLLERR`, a USB device unplugged, suspend/resume. Rather than leaving
    /// playback dead (and the log spammed), rebuild the stream and resume.
    fn handle_audio_errors(&mut self, ctx: &egui::Context) {
        let Some(error) = self
            .audio
            .as_ref()
            .and_then(|audio| audio.take_device_error())
        else {
            return;
        };

        // An underrun is a transient glitch. Rebuilding the device would also
        // restart the track, which is far worse than a brief dropout.
        if AudioEngine::is_underrun(&error) {
            self.underruns += 1;
            if self.underruns == 1 || self.underruns.is_multiple_of(20) {
                eprintln!("[audio] buffer underrun (x{}) — {error}", self.underruns);
            }
            return;
        }

        // Nothing is playing, so there is nothing to recover: an idle ALSA
        // device reports POLLERR on its own, and rebuilding here would churn
        // the output device (and say so in the status bar) for no reason.
        if self
            .audio
            .as_ref()
            .is_some_and(|audio| audio.now_playing.is_none())
        {
            return;
        }

        eprintln!("[audio] output device error: {error}");

        if let Some(last) = self.last_audio_recovery
            && last.elapsed() < Duration::from_secs(5)
        {
            return; // don't hammer reconnection
        }
        self.last_audio_recovery = Some(Instant::now());

        let resume = self.audio.as_ref().and_then(|audio| {
            audio
                .now_playing
                .clone()
                .map(|track| (track, audio.position()))
        });
        let volume = self.volume;

        match AudioEngine::new() {
            Ok(mut fresh) => {
                fresh.set_volume(volume);
                self.audio = Some(fresh);
                self.status = Some("Audio device restarted".to_string());
                if let Some((track, position)) = resume {
                    let index = match recovery_index(&self.queue, self.current, &track.video_id) {
                        Some(index) => index,
                        // The queue no longer holds it, so there is nothing to
                        // advance through: make it the whole run rather than
                        // playing something unrelated.
                        None => {
                            self.queue = vec![track.clone()];
                            0
                        }
                    };
                    let path = resolver::cache_path(&track.video_id);
                    if path.exists() {
                        // Resume the cached file where we left off.
                        if let Some(audio) = self.audio.as_mut() {
                            let _ = audio.play_file(&path, track.clone());
                            audio.seek(position);
                        }
                        self.advance_armed = true;
                    } else {
                        // A stream can't be resumed, so start it again.
                        self.play_index(ctx, index);
                    }
                }
            }
            Err(e) => {
                self.audio = None;
                self.audio_error = Some(format!("Audio output unavailable: {e}"));
                self.status = Some(format!("Audio device lost: {e}"));
            }
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::YtdlpVersion(version) => self.ytdlp_version = version,
                Msg::Lyrics(result) => {
                    self.lyrics = match result {
                        Ok(Some(lyrics)) => LyricsState::Ready(lyrics),
                        Ok(None) => LyricsState::None,
                        Err(why) => LyricsState::Failed(why),
                    };
                }
                Msg::Radio(result) => {
                    self.radio_inflight = false;
                    match result {
                        Ok(Some(tracks)) => {
                            let known: std::collections::HashSet<String> =
                                self.queue.iter().map(|t| t.video_id.clone()).collect();
                            let fresh: Vec<Track> = tracks
                                .into_iter()
                                .filter(|track| !known.contains(&track.video_id))
                                .collect();
                            if fresh.is_empty() {
                                // The radio had nothing new; stop where we are
                                // rather than asking again on the next track.
                                self.status = Some("Nothing more like this".to_string());
                                self.current = None;
                                if let Some(audio) = self.audio.as_mut() {
                                    audio.stop();
                                }
                            } else {
                                let added = fresh.len();
                                self.queue.extend(fresh);
                                self.status = Some(format!("Added {added} similar tracks"));
                                if let Some(next) = self.current.map(|index| index + 1) {
                                    self.play_index(ctx, next);
                                }
                            }
                        }
                        Ok(None) => {
                            self.status = Some("Nothing more like this".to_string());
                            self.current = None;
                            if let Some(audio) = self.audio.as_mut() {
                                audio.stop();
                            }
                        }
                        Err(why) => {
                            self.status = Some(format!("Could not find more music: {why}"));
                            self.current = None;
                            if let Some(audio) = self.audio.as_mut() {
                                audio.stop();
                            }
                        }
                    }
                }
                Msg::UpdateChecked(Ok(Some(release))) => {
                    self.update_state = UpdateState::Available(release);
                }
                Msg::UpdateChecked(Ok(None)) => self.update_state = UpdateState::Current,
                Msg::UpdateChecked(Err(why)) => self.update_state = UpdateState::Failed(why),
                Msg::UpdateInstalled(Ok((version, outcome))) => {
                    self.update_state = match outcome {
                        update::Outcome::Replaced => UpdateState::Installed(version),
                        update::Outcome::Saved(path, why) => UpdateState::Saved(path, why),
                    };
                }
                Msg::UpdateInstalled(Err(why)) => self.update_state = UpdateState::Failed(why),
                Msg::MoreResults(result) => match result {
                    Ok((items, more)) => {
                        // An empty page means there was nothing left to add;
                        // say so rather than appearing to ignore the click.
                        self.status = items
                            .is_empty()
                            .then(|| "No more results for this search".to_string());
                        self.search_items.extend(items);
                        self.sections = group_items(&self.search_items);
                        self.search_more = more;
                    }
                    Err(e) => self.status = Some(format!("Could not load more results: {e}")),
                },
                Msg::Search(Ok(results)) => {
                    self.sections = results.sections;
                    self.search_items = results.items;

                    self.search_more = results.more;
                    self.busy = false;
                    self.status = None;
                    self.view = View::Search;
                    self.nav = Nav::Search;
                    // Warm the first couple of song results.
                    let warm: Vec<Track> = self
                        .sections
                        .iter()
                        .find(|section| section.filter.has_row_layout())
                        .map(|section| section.items.as_slice())
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(SearchItem::to_track)
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
                    let is_home = page.browse_id == "FEmusic_home";
                    self.queue = page.tracks.clone();
                    self.current = None;
                    self.advance_armed = false;
                    let warm: Vec<Track> = page.tracks.iter().take(2).cloned().collect();
                    // Also resolve-and-cache the first few: each is under a
                    // second of work once its URL is known, and it persists.
                    let to_cache: Vec<String> = page
                        .tracks
                        .iter()
                        .take(8)
                        .map(|track| track.video_id.clone())
                        .collect();
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
                    self.enqueue_cache(ctx, to_cache);
                    if let Some(token) = next {
                        self.more_rounds = 1;
                        self.spawn_more_shelves(ctx, browse_id, token);
                    }
                    // The startup feed is anonymous; swap in the personalized
                    // one now that sign-in has completed.
                    if is_home && self.authenticated && self.home_needs_reload {
                        self.home_needs_reload = false;
                        self.load_page(ctx, "FEmusic_home".to_string(), false);
                    }
                }
                Msg::Page(Err(e)) => {
                    self.busy = false;
                    self.status = Some(format!("Could not open page: {e}"));
                }
                Msg::Downloaded {
                    video_id,
                    url,
                    result,
                } => {
                    self.downloading.remove(&video_id);
                    if let Some(url) = url {
                        self.url_cache
                            .insert(video_id.clone(), (url, Instant::now()));
                    }
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
                    self.auth_settled = true;
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
                            let (lang, country) = Config::locale();
                            let youtube = resolved.jar.has_youtube_session();
                            // Remember the source that worked so yt-dlp uses it too.
                            self.auth = resolved.source.clone();
                            match Client::new(&lang, &country, Some(resolved.jar)) {
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
                    // The home feed is personalized, so mark it for a reload.
                    self.home_needs_reload = true;
                    if matches!(&self.view, View::Detail(page) if page.browse_id == "FEmusic_home")
                    {
                        self.home_needs_reload = false;
                        self.load_page(ctx, "FEmusic_home".to_string(), false);
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
                        self.deferred_since = None;
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
                Msg::MoreTracks { browse_id, result } => {
                    self.busy = false;
                    let mut next_anchor = None;
                    if let View::Detail(page) = &mut self.view
                        && page.browse_id == browse_id
                        && let Ok(tracks) = result
                    {
                        let known: std::collections::HashSet<String> =
                            page.tracks.iter().map(|t| t.video_id.clone()).collect();
                        let fresh: Vec<Track> = tracks
                            .into_iter()
                            .filter(|track| !known.contains(&track.video_id))
                            .collect();
                        let gained = !fresh.is_empty();
                        page.tracks.extend(fresh.iter().cloned());
                        self.queue.extend(fresh);
                        // One window adds ~50 tracks, so keep going until the
                        // playlist is complete.
                        if gained
                            && self.track_rounds < MAX_TRACK_ROUNDS
                            && page.has_more_tracks()
                            && let (Some(playlist_id), Some(anchor)) = (
                                page.playlist_id.clone(),
                                page.tracks.last().map(|t| t.video_id.clone()),
                            )
                        {
                            self.track_rounds += 1;
                            next_anchor = Some((browse_id.clone(), playlist_id, anchor));
                        }
                    }
                    if let Some((browse_id, playlist_id, anchor)) = next_anchor {
                        self.spawn_more_tracks(ctx, browse_id, playlist_id, anchor);
                    }
                }
                Msg::CacheIdle => {
                    self.pump_cache_worker(ctx);
                }
                Msg::Media(command) => self.handle_media_command(ctx, command),
                Msg::Playlists(result) => {
                    if let Some(picker) = self.picker.as_mut() {
                        match result {
                            Ok(list) => picker.playlists = Some(list),
                            Err(e) => {
                                picker.note = Some(e);
                                picker.busy = false;
                            }
                        }
                    }
                }
                Msg::DeletedPlaylist {
                    playlist_id,
                    result,
                } => match result {
                    Ok(message) => {
                        self.status = Some(message);
                        // Leaving the page open would show a playlist that no
                        // longer exists, and reloading it would just fail.
                        let showing = matches!(
                            &self.view,
                            View::Detail(page)
                                if page.playlist_id.as_deref() == Some(playlist_id.as_str())
                        );
                        if showing {
                            self.nav = Nav::Library;
                            self.load_page(ctx, "FEmusic_library_landing".to_string(), true);
                        }
                    }
                    Err(e) => self.status = Some(format!("Could not delete the playlist: {e}")),
                },
                Msg::PlaylistEdited(result) => match result {
                    Ok(message) => {
                        // Done with the dialog; say what happened in the status
                        // line rather than leaving a window open.
                        self.picker = None;
                        self.status = Some(message);
                    }
                    Err(e) => {
                        if let Some(picker) = self.picker.as_mut() {
                            picker.note = Some(e);
                            picker.busy = false;
                        }
                    }
                },
                Msg::LikeStatus { video_id, result } => {
                    self.like_inflight = false;
                    match result {
                        Ok((status, allowed)) => {
                            // Ignore a reply for a track we have already left.
                            if self.like_for.as_deref() == Some(video_id.as_str()) {
                                self.like_status = status;
                                self.likes_allowed = allowed;
                            }
                        }
                        Err(e) => {
                            self.status = Some(format!("Could not read the rating: {e}"));
                            self.like_for = None;
                        }
                    }
                }
                Msg::LikeSet { video_id, result } => {
                    if let Err(e) = result {
                        self.status = Some(format!("Could not update the rating: {e}"));
                        // Rather than guess at a rollback, re-read the truth.
                        if self.like_for.as_deref() == Some(video_id.as_str()) {
                            self.like_for = None;
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
                            self.thumbnails.insert(uri.clone(), texture);
                            self.thumb_order.push_back(uri.clone());
                            // Evict the oldest textures that the current view
                            // doesn't need, so visible artwork never flickers.
                            if self.thumb_order.len() > MAX_THUMBS {
                                let wanted = self.wanted_thumbs.clone();
                                let mut kept = VecDeque::new();
                                let mut count = self.thumb_order.len();
                                while let Some(oldest) = self.thumb_order.pop_front() {
                                    if count > MAX_THUMBS && !wanted.contains(&oldest) {
                                        self.thumbnails.remove(&oldest);
                                        count -= 1;
                                    } else {
                                        kept.push_back(oldest);
                                    }
                                }
                                self.thumb_order = kept;
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

    /// Play a click that has been waiting too long for sign-in.
    ///
    /// `deferred_play` is otherwise only drained by a successful sign-in, so
    /// when there is no usable cookie source (or the import fails) the click
    /// would be dropped silently and the bar would sit on "Signing in…"
    /// forever. Better to play anonymously, which is how the app behaves when
    /// no browser is signed in at all.
    fn tick_deferred_play(&mut self, ctx: &egui::Context) {
        let (Some(idx), Some(since)) = (self.deferred_play, self.deferred_since) else {
            return;
        };
        if since.elapsed() < DEFERRED_PLAY_TIMEOUT {
            return;
        }
        self.deferred_play = None;
        self.deferred_since = None;
        self.status = Some("Sign-in is taking a while — playing without it".to_string());
        self.play_at(ctx, idx, true);
    }

    /// Keep the desktop's media widget in step with playback.
    fn tick_media(&mut self) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        let playing = audio.is_playing();
        let position = audio.position();
        let total = audio.total;
        let track = audio.now_playing.clone();

        // Metadata only changes when the track does.
        let video_id = track.as_ref().map(|t| t.video_id.clone());
        if video_id != self.media_for {
            self.media_for = video_id;
            match &track {
                Some(track) => {
                    let artist = track.artist_line();
                    self.media.set_track(
                        &track.title,
                        &artist,
                        track.album.as_deref(),
                        total,
                        track.thumbnail.as_deref(),
                    );
                }
                None => self.media.set_stopped(),
            }
        }

        // Nothing loaded reads as Stopped; loaded-but-paused reads as Paused.
        if track.is_none() {
            self.media.set_stopped();
        } else if self.last_media_push.elapsed() >= MEDIA_PUSH_INTERVAL {
            self.last_media_push = Instant::now();
            self.media.set_playback(playing, position);
        }

        // Volume is pushed from here alone, so the desktop's value cannot drift
        // from the app's — including the startup default, which the desktop
        // would otherwise report as 1.0.
        if (self.volume - self.media_volume).abs() > 0.001 {
            self.media_volume = self.volume;
            self.media.set_volume(self.volume);
        }
    }

    /// Act on a request from the desktop's media controls.
    fn handle_media_command(&mut self, ctx: &egui::Context, command: media::Command) {
        match command {
            media::Command::Play => {
                if let Some(audio) = self.audio.as_ref()
                    && audio.is_paused()
                {
                    audio.toggle_pause();
                }
            }
            media::Command::Pause => {
                if let Some(audio) = self.audio.as_ref()
                    && !audio.is_paused()
                {
                    audio.toggle_pause();
                }
            }
            media::Command::Toggle => {
                if let Some(audio) = self.audio.as_ref() {
                    audio.toggle_pause();
                }
            }
            media::Command::Next => self.step_track(ctx, 1),
            media::Command::Previous => self.step_track(ctx, -1),
            media::Command::Stop => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.stop();
                }
                self.current = None;
                self.media_for = None;
                self.media.set_stopped();
            }
            media::Command::SeekBy(delta_ms) => {
                if let Some(audio) = self.audio.as_ref() {
                    let target = audio
                        .position()
                        .as_millis()
                        .saturating_add_signed(delta_ms as i128);
                    audio.seek(Duration::from_millis(target as u64));
                }
            }
            media::Command::SeekTo(position) => {
                if let Some(audio) = self.audio.as_ref() {
                    audio.seek(position);
                }
            }
            media::Command::SetVolume(volume) => {
                // The desktop asked, so the app's own volume follows it.
                self.volume = (volume as f32).clamp(0.0, 1.0);
                self.pending_save = true;
                self.pending_save_at = Instant::now();
                if self.volume > 0.001 {
                    self.last_volume = self.volume;
                }
                if let Some(audio) = self.audio.as_mut() {
                    audio.set_volume(self.volume);
                }
                // The periodic push in `tick_media` publishes this.
            }
        }
    }

    fn tick_playback(&mut self, ctx: &egui::Context) {
        let finished = self.audio.as_ref().map(|a| a.finished()).unwrap_or(false);
        if !(self.advance_armed && finished) {
            return;
        }
        self.advance_armed = false;

        let shuffle_index = if self.shuffle && !self.queue.is_empty() {
            Some(random_index(self.queue.len(), self.current))
        } else {
            None
        };
        match advance_decision(self.repeat, shuffle_index, self.queue.len(), self.current) {
            Advance::Play(index) => self.play_index(ctx, index),
            Advance::Stop => {
                // The queue ran out. Rather than stopping, ask the track's
                // endless radio for more — which is what makes an album keep
                // playing instead of falling silent at the end.
                if self.autoplay && !self.queue.is_empty() {
                    self.spawn_radio(ctx);
                    return;
                }
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
        // Scanning every frame means cloning every visible URI and rebuilding a
        // set of them, which is pure waste while scrolling.
        const SCAN_INTERVAL: Duration = Duration::from_millis(250);
        if let Some(remaining) = SCAN_INTERVAL.checked_sub(self.last_thumb_scan.elapsed()) {
            ctx.request_repaint_after(remaining);
            return;
        }
        self.last_thumb_scan = Instant::now();

        // Only artwork that was actually drawn is worth fetching. Scanning the
        // whole view instead meant a feed of fifteen shelves queued art for
        // hundreds of items nobody had scrolled to — five times the requests,
        // the decode work and the texture memory, for pictures off screen.
        //
        // Nothing needs seeding: an item with no texture still draws a
        // placeholder, so it reports itself as visible and gets fetched.
        let uris: Vec<String> = ui::take_visible_thumbnails(ctx).into_iter().collect();

        // Remember what this view needs so eviction never drops it.
        self.wanted_thumbs = uris.iter().cloned().collect();

        for uri in uris {
            if self.thumb_inflight.len() >= MAX_THUMB_INFLIGHT {
                break;
            }
            if self.thumbnails.contains_key(&uri) || self.thumb_failed.contains(&uri) {
                continue;
            }
            // The key carries the size: "160@https://…". Fetching wants the
            // URL; decoding wants the size, so the texture costs what it is
            // drawn at rather than always the largest.
            let (edge, url) = match uri.split_once('@') {
                Some((edge, url)) => (edge.parse::<u32>().unwrap_or(160), url.to_string()),
                None => (160, uri.clone()),
            };
            if !self.thumb_inflight.insert(uri.clone()) {
                continue; // already in flight
            }
            let http = self.img_http.clone();
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            self.rt.spawn(async move {
                // Decoding is CPU-heavy, so keep it off the UI thread (and off
                // the async workers) to avoid freezing the window.
                let result = match thumbs::fetch(&http, &url).await {
                    Ok(bytes) => tokio::task::spawn_blocking(move || thumbs::decode(&bytes, edge))
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
        let mut activated: Option<Nav> = None;
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(176.0)
            .show(ui, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("♪  Refrain").size(15.0).strong());
                });
                ui.add_space(12.0);
                if nav_button(ui, &mut nav, Nav::Home, "🏠   Home") {
                    activated = Some(Nav::Home);
                }
                if nav_button(ui, &mut nav, Nav::Library, "📚   Library") {
                    activated = Some(Nav::Library);
                }
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

        // Clicking the current section still refreshes it — the home feed's
        // "Quick picks" change on every request.
        if let Some(target) = activated {
            self.nav = target;
            match target {
                Nav::Home => self.load_page(ctx, "FEmusic_home".to_string(), true),
                Nav::Search => self.view = View::Search,
                Nav::Library => {
                    self.load_page(ctx, "FEmusic_library_landing".to_string(), true);
                }
            }
        }
    }

    /// Search box, filter and navigation controls.
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_search = false;
        let mut go_back = false;
        let mut filter_changed = false;
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // Every control shares one height, so they line up.
                ui.spacing_mut().interact_size.y = ui::CONTROL_HEIGHT;
                // Equal side widths, so the middle cluster really is centred.
                let total = ui.available_width();
                let side = 84.0;
                let centre_width = (total - side * 2.0).max(260.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(side, ui::CONTROL_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        let can_go_back =
                            !matches!(self.view, View::Search) || self.prev_sections.is_some();
                        if ui::icon_button(
                            ui,
                            ui::Icon::ChevronLeft,
                            ui::CONTROL_HEIGHT,
                            can_go_back,
                            "Back",
                        )
                        .clicked()
                        {
                            go_back = true;
                        }
                    },
                );

                ui.allocate_ui_with_layout(
                    egui::vec2(centre_width, ui::CONTROL_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        // Field, category chip, button — three controls in a
                        // row. The chip fits its label, but the slot it sits in
                        // is fixed, or the centred cluster would shuffle sideways
                        // whenever the filter changed.
                        const SEARCH_MAX: f32 = 440.0;
                        const FILTER_SLOT: f32 = 118.0;
                        const BUTTON: f32 = 88.0;
                        const GAP: f32 = 10.0;
                        let search_width = (centre_width - GAP * 2.0 - FILTER_SLOT - BUTTON)
                            .clamp(200.0, SEARCH_MAX);
                        let cluster = search_width + GAP * 2.0 + FILTER_SLOT + BUTTON;
                        ui.add_space(((centre_width - cluster) / 2.0).max(0.0));
                        if ui::search_field(ui, &mut self.query, search_width) {
                            do_search = true;
                        }
                        ui.add_space(GAP);
                        let mut chosen = None;
                        ui::fixed_section(
                            ui,
                            egui::vec2(FILTER_SLOT, ui::CONTROL_HEIGHT),
                            // Centred in its slot: left-aligned it hugged the
                            // field and read as part of it, which is the thing
                            // this is meant to stop looking like.
                            Layout::top_down(Align::Center),
                            |ui| {
                                chosen = ui::pill_dropdown(
                                    ui,
                                    "search_filter",
                                    self.filter,
                                    &SearchFilter::ALL_FILTERS,
                                    |filter| filter.label(),
                                );
                            },
                        );
                        if let Some(chosen) = chosen {
                            self.filter = chosen;
                            filter_changed = true;
                        }
                        ui.add_space(GAP);
                        if ui::pill_button(ui, "Search").clicked() {
                            do_search = true;
                        }
                    },
                );

                ui.allocate_ui_with_layout(
                    egui::vec2(side, ui::CONTROL_HEIGHT),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        // The gear is rightmost and stays there. In a
                        // right-to-left layout the first widget added is the
                        // rightmost, so it goes first, and the spinner gets a
                        // slot of its own beside it — appearing there cannot
                        // push anything sideways.
                        let gear = ui::icon_button(
                            ui,
                            ui::Icon::Glyph("⚙"),
                            ui::CONTROL_HEIGHT,
                            true,
                            "Settings and theme",
                        );
                        ui.add_space(6.0);
                        let (busy_rect, _) = ui.allocate_exact_size(
                            egui::vec2(18.0, ui::CONTROL_HEIGHT),
                            egui::Sense::hover(),
                        );
                        if self.busy {
                            ui.put(busy_rect, egui::Spinner::new().size(15.0));
                        }
                        let theme_label = if self.theme_light {
                            "☾  Dark theme"
                        } else {
                            "☀  Light theme"
                        };
                        let mut open_settings = false;
                        let mut toggle_theme = false;
                        egui::Popup::menu(&gear)
                            .id(egui::Id::new("app_menu"))
                            .show(|ui| {
                                if ui.button(theme_label).clicked() {
                                    toggle_theme = true;
                                }
                                if ui.button("⚙  Settings…").clicked() {
                                    open_settings = true;
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
                        if toggle_theme {
                            self.theme_light = !self.theme_light;
                            self.theme_applied = false;
                            self.save_ui_settings();
                        }
                        if open_settings {
                            // Measure the cache as it opens, not every frame.
                            self.cache_usage = resolver::cache_usage();
                            self.spawn_ytdlp_version(ctx);
                            self.show_settings = true;
                        }
                    },
                );
            });
            ui.add_space(6.0);
        });

        if go_back {
            if matches!(self.view, View::Search) && self.prev_sections.is_some() {
                // Leave an expanded category back to the grouped results.
                self.sections = self.prev_sections.take().unwrap_or_default();
                self.filter = SearchFilter::All;
                if let Some((query, _)) = self.last_search.clone() {
                    self.query = query;
                }
            } else {
                match self.view_history.pop() {
                    Some((nav, view)) => {
                        self.nav = nav;
                        self.view = view;
                    }
                    None => {
                        self.nav = Nav::Search;
                        self.view = View::Search;
                    }
                }
            }
        }
        if do_search {
            self.nav = Nav::Search;
            self.run_search(ctx);
        }
        if filter_changed {
            // Changing category re-runs the search for that category.
            self.run_search(ctx);
        }
    }

    /// Report the frame rate the platform is actually delivering, for `--fps`.
    ///
    /// Whether an animation looks smooth is decided by how many frames arrive,
    /// and that depends on the compositor, the driver, and whether the window
    /// is even being looked at. Guessing at it is how you spend an afternoon
    /// measuring a debug build. This turns "it looks choppy" into a number.
    fn tick_fps_probe(&mut self) {
        if !self.fps_probe {
            return;
        }
        self.fps_frames += 1;
        let elapsed = self.fps_since.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            eprintln!(
                "[fps] {:.1} frames/s ({} frames in {elapsed:.2}s)",
                self.fps_frames as f32 / elapsed,
                self.fps_frames
            );
            self.fps_frames = 0;
            self.fps_since = Instant::now();
        }
    }

    /// Write the config once a change has settled.
    ///
    /// The volume is dragged, so saving every frame would rewrite the file
    /// dozens of times for one gesture. A short delay makes it one write — and
    /// the repaint request matters as much as the delay: without it the UI can
    /// go to sleep before the write happens and the setting is lost, which is
    /// what used to happen to the volume on the way out.
    fn flush_pending_save(&mut self, ctx: &egui::Context) {
        if !self.pending_save {
            return;
        }
        const SETTLE: Duration = Duration::from_millis(800);
        let waited = self.pending_save_at.elapsed();
        if waited < SETTLE {
            ctx.request_repaint_after(SETTLE - waited);
            return;
        }
        self.pending_save = false;
        self.save_ui_settings();
    }

    fn save_ui_settings(&self) {
        let mut cfg = Config::load();
        cfg.ui.theme = if self.theme_light { "light" } else { "dark" }.to_string();
        cfg.ui.volume = self.volume;
        cfg.ui.autoplay = self.autoplay;
        let _ = cfg.save();
    }

    fn central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut action: Option<Action> = None;
        let mut hovered: Option<Track> = None;
        let mut see_more: Option<SearchFilter> = None;
        let view = std::mem::replace(&mut self.view, View::Search);
        let current_video = self
            .current
            .and_then(|c| self.queue.get(c))
            .map(|t| t.video_id.clone());

        egui::CentralPanel::default().show(ui, |ui| {
            if self.show_queue {
                egui::Panel::right("queue")
                    .resizable(true)
                    .default_size(300.0)
                    .show(ui, |ui| {
                        self.queue_panel(ui, &current_video, &mut action);
                    });
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &view {
                    View::Search => self.results_view(
                        ui,
                        &current_video,
                        &mut action,
                        &mut hovered,
                        &mut see_more,
                    ),
                    View::Detail(page) => {
                        self.detail_view(ui, page, &current_video, &mut action, &mut hovered)
                    }
                });
        });

        self.view = view;
        // "See more" expands a category — its results are already loaded.
        if let Some(filter) = see_more {
            self.filter = filter;
            if !self.sections.iter().any(|section| section.filter == filter) {
                self.run_search(ctx);
            }
        }
        self.hover_prefetch(ctx, hovered);
        if std::mem::take(&mut self.want_more_results) {
            self.spawn_more_results(ctx);
        }
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
        see_more: &mut Option<SearchFilter>,
    ) {
        if self.sections.is_empty() {
            empty_state(ui, self.busy);
            return;
        }

        // A chosen category shows everything it returned; "All" previews each.
        let expanded = self.filter != SearchFilter::All;
        const PREVIEW: usize = 6;

        // Only the expanded view can page: a grouped "All" search comes back
        // complete, and its only token is a reload one that yields nothing.
        let more_available = self.search_more.is_some() && expanded;

        // Taken out so rendering can borrow `self` mutably.
        let sections = std::mem::take(&mut self.sections);
        for section in &sections {
            let items: Vec<SearchItem> = if expanded {
                section.items.clone()
            } else {
                section.items.iter().take(PREVIEW).cloned().collect()
            };
            if items.is_empty() {
                continue;
            }

            if !expanded {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(section.filter.label()).size(18.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("See more").clicked() {
                            *see_more = Some(section.filter);
                        }
                    });
                });
                ui.add_space(4.0);
            }

            if section.filter.has_row_layout() {
                self.render_rows(
                    ui,
                    section.filter.label(),
                    &items,
                    current_video,
                    action,
                    hovered,
                );
            } else if let Some(idx) = ui::cover_grid(
                ui,
                &self.thumbnails,
                &items,
                168.0,
                current_video.as_deref(),
            ) {
                *action = activate(&items, idx);
            }
        }
        self.sections = sections;

        if more_available {
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                if ui
                    .button("Load more results")
                    .on_hover_text("Fetch the next page from YouTube Music")
                    .clicked()
                {
                    self.want_more_results = true;
                }
            });
            ui.add_space(12.0);
        }
    }

    /// Rows for a list of songs/videos: hover warms a stream, clicking plays
    /// within the list so next/prev walk it.
    fn render_rows(
        &mut self,
        ui: &mut egui::Ui,
        id_prefix: &str,
        items: &[SearchItem],
        current_video: &Option<String>,
        action: &mut Option<Action>,
        hovered: &mut Option<Track>,
    ) {
        let active = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        for (idx, item) in items.iter().enumerate() {
            let selected = item.video_id.as_deref() == current_video.as_deref();
            let trailing = item.duration_secs.map(crate::ytm::domain::fmt_duration);
            let row = ui::list_row(
                ui,
                &self.thumbnails,
                (id_prefix, idx),
                item.thumbnail.as_deref(),
                &item.title,
                &item.segments,
                trailing.as_deref(),
                Some(idx),
                selected,
                active,
                false,
                item.playable(),
                item.navigable(),
            );
            if row.add_to_playlist {
                if let Some(video_id) = item.video_id.clone() {
                    *action = Some(Action::AddToPlaylist {
                        video_id,
                        title: item.title.clone(),
                    });
                }
            } else if let Some(id) = row.link {
                // An artist/album name inside the row opens that page.
                *action = Some(Action::Open(id));
            } else if row.hovered {
                *hovered = item.to_track();
            }
            if row.clicked {
                *action = activate(items, idx);
            }
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

        // Editing is offered for playlists a person owns. Editorial ones are
        // excluded, but the server remains the authority: a playlist owned by
        // someone else will simply refuse, with the reason shown.
        let playlist_id = if page.owner_is_user {
            page.playlist_id.clone()
        } else {
            None
        };
        if let Some(id) = &playlist_id {
            ui.horizontal(|ui| {
                if ui.button("Rename").clicked() {
                    *action = Some(Action::RenamePlaylist {
                        playlist_id: id.clone(),
                        title: page.title.clone(),
                    });
                }
                if ui.button("Delete").clicked() {
                    *action = Some(Action::DeletePlaylist {
                        playlist_id: id.clone(),
                        title: page.title.clone(),
                    });
                }
            });
            ui.add_space(6.0);
        }

        let active = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        for (idx, track) in page.tracks.iter().enumerate() {
            let selected = self.current == Some(idx)
                && current_video.as_deref() == Some(track.video_id.as_str());
            let trailing = track.duration_text();
            let mut segments: Vec<(String, Option<String>)> = Vec::new();
            for (position, artist) in track.artists.iter().enumerate() {
                if position > 0 {
                    segments.push((", ".to_string(), None));
                }
                segments.push((artist.clone(), track.artist_ids.get(position).cloned()));
            }
            let row = ui::list_row(
                ui,
                &self.thumbnails,
                ("track", idx),
                track.thumbnail.as_deref(),
                &track.title,
                &segments,
                trailing.as_deref(),
                Some(idx),
                selected,
                active,
                playlist_id.is_some(),
                true,
                false,
            );
            if row.add_to_playlist {
                *action = Some(Action::AddToPlaylist {
                    video_id: track.video_id.clone(),
                    title: track.title.clone(),
                });
            } else if row.remove_from_playlist {
                if let Some(id) = &playlist_id {
                    *action = Some(Action::RemoveFromPlaylist {
                        playlist_id: id.clone(),
                        video_id: track.video_id.clone(),
                        set_video_id: track.set_video_id.clone(),
                        title: track.title.clone(),
                    });
                }
            } else if let Some(id) = row.link {
                *action = Some(Action::Open(id));
            } else if row.hovered {
                *hovered = Some(track.clone());
            }
            if row.clicked {
                *action = Some(Action::PlayQueueIndex(idx));
            }
        }

        // Long playlists cap at 100 tracks in the page response.
        if page.has_more_tracks() {
            ui.add_space(8.0);
            let remaining = page
                .track_count
                .map(|total| total.saturating_sub(page.tracks.len()))
                .unwrap_or(0);
            ui.vertical_centered(|ui| {
                let label = if remaining > 0 {
                    format!("Load {remaining} more tracks")
                } else {
                    "Load more tracks".to_string()
                };
                if ui
                    .add(egui::Button::new(label).min_size(egui::vec2(200.0, 30.0)))
                    .clicked()
                {
                    *action = Some(Action::LoadMoreTracks);
                }
            });
        }

        // Browsable carousels (artist albums, related, the home feed…).
        for (shelf_index, shelf) in page.shelves.iter().enumerate() {
            if shelf.items.is_empty() {
                continue;
            }
            ui.add_space(18.0);
            if !shelf.title.is_empty() {
                ui.label(ui::title(&shelf.title).size(18.0));
                ui.add_space(6.0);
            }
            if let Some(idx) = ui::cover_row(
                ui,
                &self.thumbnails,
                &shelf.items,
                168.0,
                current_video.as_deref(),
                (page.browse_id.as_str(), shelf_index),
            ) {
                *action = activate(&shelf.items, idx);
            }
        }
    }

    /// Open the "add to playlist" dialog for a track.
    fn open_picker(&mut self, ctx: &egui::Context, video_id: String, title: String) {
        if !self.authenticated {
            self.status = Some("Sign in to save tracks to a playlist".to_string());
            return;
        }
        let needs_list = self.picker.is_none();
        self.picker = Some(Picker {
            video_id,
            title,
            playlists: None,
            new_name: String::new(),
            note: None,
            busy: false,
        });
        if needs_list {
            self.spawn_playlists(ctx);
        }
    }

    fn spawn_playlists(&mut self, ctx: &egui::Context) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .browse("FEmusic_liked_playlists")
                .await
                .map(|page| playlists_from(&page))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Playlists(result));
            ctx.request_repaint();
        });
    }

    fn spawn_add_to_playlist(
        &mut self,
        ctx: &egui::Context,
        playlist_id: String,
        video_id: String,
    ) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .add_to_playlist(&playlist_id, &video_id)
                .await
                .map(|()| "Added to playlist".to_string())
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::PlaylistEdited(result));
            ctx.request_repaint();
        });
    }

    fn spawn_create_and_add(&mut self, ctx: &egui::Context, name: String, video_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = async {
                let playlist_id = client.create_playlist(&name).await?;
                client.add_to_playlist(&playlist_id, &video_id).await?;
                Ok(format!("Created “{name}” and added the track"))
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            let _ = tx.send(Msg::PlaylistEdited(result));
            ctx.request_repaint();
        });
    }

    fn spawn_rename(&mut self, ctx: &egui::Context, playlist_id: String, title: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .rename_playlist(&playlist_id, &title)
                .await
                .map(|()| format!("Renamed to “{title}”"))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::PlaylistEdited(result));
            ctx.request_repaint();
        });
    }

    fn spawn_delete(&mut self, ctx: &egui::Context, playlist_id: String, title: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .delete_playlist(&playlist_id)
                .await
                .map(|()| format!("Deleted “{title}”"))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::DeletedPlaylist {
                playlist_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    /// Ask the chosen yt-dlp for its version. Spawns a process, so it happens
    /// when the settings window opens rather than at startup.
    /// Ask GitHub whether there is a newer release.
    ///
    /// The automatic check runs at most once a day: it is a request to somebody
    /// else's server and nothing about it is urgent. The button in Settings
    /// asks whenever it is pressed.
    fn spawn_update_check(&mut self, ctx: &egui::Context) {
        if matches!(
            self.update_state,
            UpdateState::Checking | UpdateState::Installing
        ) {
            return;
        }
        self.update_state = UpdateState::Checking;
        self.last_update_check = Some(now_unix());
        {
            let mut cfg = Config::load();
            cfg.ui.last_update_check = self.last_update_check;
            let _ = cfg.save();
        }
        let http = self.img_http.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = update::check(&http, update::CURRENT).await;
            let _ = tx.send(Msg::UpdateChecked(result));
            ctx.request_repaint();
        });
    }

    fn update_check_due(&self) -> bool {
        const DAY: u64 = 24 * 60 * 60;
        match self.last_update_check {
            None => true,
            Some(last) => now_unix().saturating_sub(last) >= DAY,
        }
    }

    fn spawn_update_install(&mut self, ctx: &egui::Context, release: update::Release) {
        self.update_state = UpdateState::Installing;
        let kind = self.update_kind.clone();
        let http = self.img_http.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = update::install(&http, &release, &kind)
                .await
                .map(|outcome| (release.version.clone(), outcome));
            let _ = tx.send(Msg::UpdateInstalled(result));
            ctx.request_repaint();
        });
    }

    /// Start the new build and step aside.
    ///
    /// The replacement is already on disk; this process is running from the old
    /// inode, so it has to go for the new one to be what starts next.
    ///
    /// The child is started without the AppImage variables this process
    /// inherited — `APPIMAGE`, `APPDIR`, `LD_LIBRARY_PATH` and friends point
    /// into a mount that disappears when this exits, and a child that inherits
    /// them can fail to come up. When that happens the button looks like it did
    /// nothing at all, which is what this is here to stop: the child is watched
    /// for a moment, and if it falls over the reason is put in front of the
    /// user rather than into a status line in the corner.
    fn restart_now(&mut self) {
        let exe = match &self.update_kind {
            update::InstallKind::AppImage(path) => path.clone(),
            _ => std::env::current_exe().unwrap_or_default(),
        };
        // Exiting this way skips the usual shutdown path, so save first.
        self.save_ui_settings();

        let mut command = std::process::Command::new(&exe);
        for key in [
            "APPIMAGE",
            "APPDIR",
            "ARGV0",
            "OWD",
            "LD_LIBRARY_PATH",
            "LD_PRELOAD",
        ] {
            command.env_remove(key);
        }
        match command.spawn() {
            Ok(mut child) => {
                // Ok from spawn only means fork and exec worked. The new build
                // can still fall over a moment later — a missing library, a
                // mount that went away — so give it a moment and look.
                std::thread::sleep(Duration::from_millis(1500));
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.update_state = UpdateState::Failed(format!(
                            "the new build exited immediately ({status})"
                        ));
                        self.status = Some(format!(
                            "The update is at {} — open it again",
                            exe.display()
                        ));
                    }
                    _ => std::process::exit(0),
                }
            }
            Err(e) => {
                self.update_state =
                    UpdateState::Failed(format!("could not start {}: {e}", exe.display()));
                self.status = Some(format!(
                    "The update is at {} — open it again",
                    exe.display()
                ));
            }
        }
    }

    fn spawn_ytdlp_version(&mut self, ctx: &egui::Context) {
        let Some(path) = self.ytdlp.clone() else {
            return;
        };
        if self.ytdlp_version.is_some() {
            return;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            // Blocking, so keep it off the async worker threads.
            let version = tokio::task::spawn_blocking(move || resolver::ytdlp_version(&path))
                .await
                .ok()
                .flatten();
            let _ = tx.send(Msg::YtdlpVersion(version));
            ctx.request_repaint();
        });
    }

    /// The settings window.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut theme_changed = false;
        let mut auth_changed = false;
        let mut refresh_auth = false;
        let mut clear_cache = false;
        let mut check_updates = false;
        let mut install_update: Option<update::Release> = None;
        let mut restart = false;

        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                // Wide enough for a path to sit on one line, and grouped into
                // cards so the window reads as sections rather than a list.
                ui.set_min_width(460.0);
                ui.spacing_mut().item_spacing.y = 6.0;

                ui::card(ui, |ui| {
                    ui.label(ui::title("Appearance").size(15.0));
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui::segment(ui, "Dark", !self.theme_light).clicked() {
                            self.theme_light = false;
                            theme_changed = true;
                        }
                        if ui::segment(ui, "Light", self.theme_light).clicked() {
                            self.theme_light = true;
                            theme_changed = true;
                        }
                    });
                });

                ui::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(ui::title("Sign-in").size(15.0));
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(if self.authenticated {
                                "signed in"
                            } else {
                                "anonymous"
                            })
                            .color(if self.authenticated { ui::OK } else { ui::WARN })
                            .small(),
                        );
                    });
                    ui.label(
                        RichText::new(if self.authenticated {
                            "Your library, ratings and playlists are available."
                        } else {
                            "Search and browsing work; your library needs a signed-in browser."
                        })
                        .weak()
                        .small(),
                    );
                    ui.add_space(6.0);

                    let in_use = if self.auth.browser.trim().is_empty() {
                        self.auth.mode.label()
                    } else {
                        self.auth.browser.as_str()
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Read cookies").weak().small());
                        ui.add_space(4.0);
                        if let Some(mode) = ui::pill_dropdown(
                            ui,
                            "auth_mode",
                            self.auth_pref.mode,
                            &AuthMode::ALL,
                            |mode| mode.label(),
                        ) {
                            self.auth_pref.mode = mode;
                            auth_changed = true;
                        }
                        ui.add_space(10.0);
                        ui.label(RichText::new(format!("in use: {in_use}")).weak().small());
                    });

                    if self.auth_pref.mode == AuthMode::Browser {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Browser").weak().small());
                            ui.add_space(4.0);
                            let current = KNOWN_BROWSERS
                                .iter()
                                .copied()
                                .find(|browser| *browser == self.auth_pref.browser)
                                .unwrap_or(KNOWN_BROWSERS[0]);
                            if let Some(browser) = ui::pill_dropdown(
                                ui,
                                "auth_browser",
                                current,
                                &KNOWN_BROWSERS,
                                |browser| browser,
                            ) {
                                self.auth_pref.browser = browser.to_string();
                                auth_changed = true;
                            }
                            ui.add_space(10.0);
                            ui.label(RichText::new("Keyring").weak().small());
                            ui.add_space(4.0);
                            let current = KNOWN_KEYRINGS
                                .iter()
                                .copied()
                                .find(|keyring| *keyring == self.auth_pref.keyring)
                                .unwrap_or("");
                            if let Some(keyring) = ui::pill_dropdown(
                                ui,
                                "auth_keyring",
                                current,
                                &KNOWN_KEYRINGS,
                                keyring_label,
                            ) {
                                self.auth_pref.keyring = keyring.to_string();
                                auth_changed = true;
                            }
                        });
                    }

                    if self.auth_pref.mode == AuthMode::File {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Cookies file").weak().small());
                            ui.add_space(4.0);
                            let field = ui.add(
                                egui::TextEdit::singleline(&mut self.auth_pref.cookies_file)
                                    .hint_text("path to a cookies.txt")
                                    .desired_width(300.0),
                            );
                            // Saved when editing stops rather than on each
                            // keystroke: every save re-reads the session.
                            if field.lost_focus() {
                                auth_changed = true;
                            }
                        });
                        // Worth saying before they wonder why nothing works.
                        let path = self.auth_pref.cookies_file.trim();
                        if !path.is_empty() && !std::path::Path::new(path).is_file() {
                            ui.label(RichText::new("No file there.").color(ui::WARN).small());
                        }
                    }

                    ui.add_space(6.0);
                    if ui::quiet_button(ui, "Refresh sign-in")
                        .on_hover_text("Discard the cached cookies and extract them again")
                        .clicked()
                    {
                        refresh_auth = true;
                    }
                });

                ui::card(ui, |ui| {
                    ui.label(ui::title("Storage").size(15.0));
                    let (tracks, bytes) = self.cache_usage;
                    ui.label(
                        RichText::new(format!(
                            "{tracks} cached tracks · {} of {}",
                            ui::fmt_bytes(bytes),
                            ui::fmt_bytes(resolver::CACHE_BUDGET_BYTES)
                        ))
                        .weak()
                        .small(),
                    );
                    ui.add_space(4.0);
                    ui::meter(ui, bytes as f32 / resolver::CACHE_BUDGET_BYTES as f32);
                    ui.add_space(6.0);
                    if ui::quiet_button(ui, "Clear cached audio")
                        .on_hover_text("Playback re-resolves anything you play again")
                        .clicked()
                    {
                        clear_cache = true;
                    }
                });

                ui::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(ui::title("Updates").size(15.0));
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("version {}", update::CURRENT))
                                .weak()
                                .small(),
                        );
                    });
                    match &self.update_state {
                        UpdateState::Idle => {
                            ui.label(RichText::new("Not checked yet.").weak().small());
                        }
                        UpdateState::Checking => {
                            ui.label(RichText::new("Checking…").weak().small());
                        }
                        UpdateState::Current => {
                            ui.label(
                                RichText::new("This is the newest release.").weak().small(),
                            );
                        }
                        UpdateState::Failed(why) => {
                            // The messages already say what went wrong, and they
                            // are not all about checking: a restart that failed
                            // lands here too.
                            ui.label(RichText::new(why.clone()).color(ui::WARN).small());
                        }
                        UpdateState::Available(release) => {
                            ui.label(
                                RichText::new(format!("{} is available.", release.version))
                                    .small(),
                            );
                            let explanation = match &self.update_kind {
                                update::InstallKind::AppImage(_) => {
                                    "The AppImage is replaced in place; restart to use it."
                                }
                                update::InstallKind::Bundle(_) => {
                                    "The app and the tools beside it are replaced; restart to use it."
                                }
                                update::InstallKind::Managed => {
                                    "This copy belongs to your package manager, so update it there."
                                }
                                update::InstallKind::Download => {
                                    "It is downloaded to your Downloads folder to put in place yourself."
                                }
                            };
                            ui.label(RichText::new(explanation).weak().small());
                            ui.label(
                                RichText::new(
                                    "Checked against the checksum the release publishes, and \
                                     installed only if it matches.",
                                )
                                .weak()
                                .small(),
                            );
                        }
                        UpdateState::Installing => {
                            ui.label(
                                RichText::new("Downloading and verifying…").weak().small(),
                            );
                        }
                        UpdateState::Installed(version) => {
                            ui.label(
                                RichText::new(format!("Updated to {version}. Restart to use it."))
                                    .small(),
                            );
                        }
                        UpdateState::Saved(path, why) => {
                            ui.label(
                                RichText::new(format!("Saved to {}", path.display())).small(),
                            );
                            ui.label(RichText::new(why.clone()).weak().small());
                        }
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui::quiet_button(ui, "Check now").clicked() {
                            check_updates = true;
                        }
                        if let UpdateState::Available(release) = &self.update_state
                            && !matches!(self.update_kind, update::InstallKind::Managed)
                            && ui::pill_button(ui, &format!("Update to {}", release.version))
                                .clicked()
                        {
                            install_update = Some(release.clone());
                        }
                        if matches!(self.update_state, UpdateState::Installed(_))
                            && ui::quiet_button(ui, "Restart now").clicked()
                        {
                            restart = true;
                        }
                    });
                });

                ui::card(ui, |ui| {
                    ui.label(ui::title("Playback").size(15.0));
                    ui.add_space(2.0);
                    match &self.ytdlp {
                        Some(path) => {
                            ui::status_row(
                                ui,
                                true,
                                "yt-dlp",
                                &ui::short_path(path, 44),
                                &path.display().to_string(),
                            );
                            let age = resolver::ytdlp_age(path).map(|age| age.as_secs() / 86_400);
                            match (&self.ytdlp_version, age) {
                                (Some(version), Some(days)) => {
                                    ui.label(
                                        RichText::new(format!(
                                            "version {version} · written {days} days ago"
                                        ))
                                        .weak()
                                        .small(),
                                    );
                                    if days > STALE_YTDLP_DAYS {
                                        ui.label(
                                            RichText::new(
                                                "That copy is getting old — yt-dlp has to be updated \
                                                 when YouTube changes. Update it, or download this \
                                                 app again.",
                                            )
                                            .color(ui::WARN)
                                            .small(),
                                        );
                                    }
                                }
                                (Some(version), None) => {
                                    ui.label(
                                        RichText::new(format!("version {version}")).weak().small(),
                                    );
                                }
                                _ => {
                                    ui.label(
                                        RichText::new("Checking the version…").weak().small(),
                                    );
                                }
                            }
                        }
                        None => {
                            ui::status_row(ui, false, "yt-dlp", "not found", "");
                            ui.label(
                                RichText::new("Playback is disabled. Install it, or set YTDLP_PATH.")
                                    .color(ui::WARN)
                                    .small(),
                            );
                        }
                    }

                    let runtime = resolver::js_runtime_report();
                    let runtime_ok = runtime != "none found";
                    ui::status_row(
                        ui,
                        runtime_ok,
                        "JavaScript runtime",
                        &runtime,
                        "yt-dlp needs one to solve YouTube's challenges. Every release carries \
                         QuickJS beside the executable.",
                    );
                    if !runtime_ok {
                        ui.label(
                            RichText::new(
                                "Playback will fail. Install node, deno or quickjs, or download \
                                 this app again.",
                            )
                            .color(ui::WARN)
                            .small(),
                        );
                    }

                    ui.add_space(8.0);
                    let mut autoplay = self.autoplay;
                    if ui
                        .checkbox(&mut autoplay, "Autoplay similar music")
                        .on_hover_text(
                            "When a queue or an album ends, keep playing related tracks from \
                             the track's radio, the way YouTube Music does.",
                        )
                        .changed()
                    {
                        self.autoplay = autoplay;
                        self.pending_save = true;
                        self.pending_save_at = Instant::now();
                    }
                });
            });

        if theme_changed {
            self.theme_applied = false;
            self.save_ui_settings();
        }
        if auth_changed {
            // Write the preference before re-resolving: the resolver reads the
            // config back rather than taking it from here.
            let mut cfg = Config::load();
            cfg.auth = self.auth_pref.clone();
            if let Err(e) = cfg.save() {
                self.status = Some(format!("Could not save the setting: {e}"));
            }
            refresh_auth = true;
        }
        if refresh_auth {
            crate::auth::clear_cache();
            self.status = Some("Re-reading your sign-in…".to_string());
            spawn_auth(&self.rt, Config::load(), self.tx.clone(), ctx.clone(), true);
        }
        if clear_cache {
            let keep = self
                .audio
                .as_ref()
                .and_then(|audio| audio.now_playing.as_ref())
                .map(|track| track.video_id.clone());
            let (removed, freed) = resolver::clear_audio_cache(keep.as_deref());
            self.cache_usage = resolver::cache_usage();
            self.status = Some(format!(
                "Cleared {removed} cached tracks ({:.0} MB)",
                freed as f64 / 1e6
            ));
        }
        if check_updates {
            self.spawn_update_check(ctx);
        }
        if let Some(release) = install_update {
            self.spawn_update_install(ctx, release);
        }
        if restart {
            self.restart_now();
        }
        self.show_settings = open;
    }

    /// The rename dialog and the delete confirmation.
    fn playlist_dialogs(&mut self, ctx: &egui::Context) {
        if let Some((playlist_id, mut name)) = self.renaming.take() {
            let mut window_open = true;
            let mut cancelled = false;
            let mut save = false;
            egui::Window::new("Rename playlist")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .open(&mut window_open)
                .show(ctx, |ui| {
                    ui.set_min_width(300.0);
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .hint_text("Playlist name")
                            .desired_width(280.0),
                    );
                    // Enter saves, which is what anyone typing expects.
                    save = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if save && !name.trim().is_empty() {
                self.spawn_rename(ctx, playlist_id, name.trim().to_string());
            } else if window_open && !cancelled {
                self.renaming = Some((playlist_id, name));
            }
        }

        if let Some((playlist_id, title)) = self.deleting.take() {
            let mut window_open = true;
            let mut cancelled = false;
            let mut confirmed = false;
            egui::Window::new("Delete playlist")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .open(&mut window_open)
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(format!("Delete “{title}”?"));
                    ui.label(
                        RichText::new(
                            "This deletes the playlist, not the music in it. It can't be undone.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                        if ui.button("Delete").clicked() {
                            confirmed = true;
                        }
                    });
                });
            if confirmed {
                self.spawn_delete(ctx, playlist_id, title);
            } else if window_open && !cancelled {
                self.deleting = Some((playlist_id, title));
            }
        }
    }

    /// Draw the picker, if it is open.
    fn playlist_picker(&mut self, ctx: &egui::Context) {
        let Some(mut picker) = self.picker.take() else {
            return;
        };

        let mut keep_open = true;
        let mut chosen: Option<String> = None;
        let mut create_new = false;

        egui::Window::new("Add to playlist")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.set_min_width(300.0);
                ui.label(RichText::new(&picker.title).strong());
                ui.add_space(6.0);
                ui.separator();

                match &picker.playlists {
                    None => {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Loading your playlists…").weak());
                        ui.add_space(6.0);
                    }
                    Some(list) if list.is_empty() => {
                        ui.add_space(6.0);
                        ui.label(RichText::new("No playlists yet — name one below.").weak());
                        ui.add_space(6.0);
                    }
                    Some(list) => {
                        egui::ScrollArea::vertical()
                            .max_height(260.0)
                            .show(ui, |ui| {
                                for (id, title) in list {
                                    if ui
                                        .add_enabled(!picker.busy, egui::Button::new(title))
                                        .clicked()
                                    {
                                        chosen = Some(id.clone());
                                    }
                                }
                            });
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut picker.new_name)
                            .hint_text("New playlist")
                            .desired_width(190.0),
                    );
                    if ui
                        .add_enabled(!picker.busy, egui::Button::new("Create"))
                        .clicked()
                    {
                        create_new = true;
                    }
                });

                if let Some(note) = &picker.note {
                    ui.add_space(4.0);
                    ui.label(RichText::new(note).weak().small());
                }
            });

        if let Some(id) = chosen {
            picker.busy = true;
            picker.note = Some("Adding…".to_string());
            self.spawn_add_to_playlist(ctx, id, picker.video_id.clone());
        } else if create_new {
            let name = picker.new_name.trim().to_string();
            if name.is_empty() {
                picker.note = Some("Give the playlist a name first.".to_string());
            } else {
                picker.busy = true;
                picker.note = Some("Creating…".to_string());
                self.spawn_create_and_add(ctx, name, picker.video_id.clone());
            }
        }

        if keep_open {
            self.picker = Some(picker);
        }
    }

    /// Advance the demo script: each step fires once its condition holds.
    ///
    /// Conditions rather than a schedule, because authentication, a search and a
    /// page load all take a variable amount of time. A timer races them — the
    /// first attempt fired its "open an album" step before the search had
    /// returned, and the demo never left the search view.
    fn tick_demo(&mut self, ctx: &egui::Context) {
        let Some(demo) = self.demo.as_ref() else {
            return;
        };
        let Some(step) = Demo::STEPS.get(demo.step).copied() else {
            return;
        };
        // Keep frames coming while a demo runs. The UI sleeps when nothing asks
        // for a repaint, and a sleeping UI never ticks the demo: the first
        // recording sat on the search results for sixteen seconds because the
        // step that opens an album was only reached when something else
        // happened to wake the app. Slow enough not to spin — a demo wants a
        // few frames a second, not a busy loop.
        ctx.request_repaint_after(Duration::from_millis(100));
        let waited = demo.step_started.elapsed();
        if waited < Demo::STEP_GAP {
            return;
        }
        let ready = self.demo_step_ready(step);
        if !ready && waited < Demo::STEP_TIMEOUT {
            return;
        }
        if let Some(demo) = self.demo.as_mut() {
            demo.step += 1;
            demo.step_started = Instant::now();
        }
        if ready {
            self.run_demo_step(ctx, step);
        }
    }

    /// What has to be true before a step is worth running.
    fn demo_step_ready(&self, step: DemoStep) -> bool {
        match step {
            // Signing in re-runs the last search, so search after it settles or
            // the view is yanked back mid-script.
            DemoStep::Search => self.auth_settled,
            DemoStep::OpenAlbum => self
                .sections
                .iter()
                .any(|section| section.filter == SearchFilter::Albums && !section.items.is_empty()),
            DemoStep::Play => matches!(&self.view, View::Detail(page) if !page.tracks.is_empty()),
            DemoStep::ShowQueue => self
                .audio
                .as_ref()
                .is_some_and(|audio| audio.now_playing.is_some()),
        }
    }

    fn run_demo_step(&mut self, ctx: &egui::Context, step: DemoStep) {
        match step {
            DemoStep::Search => {
                self.query = DEMO_QUERY.to_string();
                self.filter = SearchFilter::All;
                self.run_search(ctx);
            }
            DemoStep::OpenAlbum => {
                // Found by searching rather than hard-coded, so the demo does
                // not rot when an id changes.
                let album = self
                    .sections
                    .iter()
                    .find(|section| section.filter == SearchFilter::Albums)
                    .and_then(|section| section.items.first())
                    .and_then(|item| item.browse_id.clone());
                if let Some(album) = album {
                    self.load_page(ctx, album, true);
                }
            }
            DemoStep::Play => {
                if matches!(&self.view, View::Detail(page) if !page.tracks.is_empty()) {
                    self.play_index(ctx, 0);
                }
            }
            DemoStep::ShowQueue => self.show_queue = true,
        }
    }

    /// Ask the track that just finished for more of the same.
    fn spawn_radio(&mut self, ctx: &egui::Context) {
        if self.radio_inflight {
            return;
        }
        let Some(track) = self
            .current
            .and_then(|index| self.queue.get(index))
            .cloned()
        else {
            return;
        };
        if self.radio_anchored.as_deref() == Some(track.video_id.as_str()) {
            return;
        }
        self.radio_anchored = Some(track.video_id.clone());
        self.radio_inflight = true;
        self.status = Some("Looking for more like this…".to_string());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .radio_more(&track.video_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Radio(result));
            ctx.request_repaint();
        });
    }

    /// Fetch lyrics for the track that is playing, once per track, and only
    /// while the lyrics tab is the one being looked at.
    fn tick_lyrics(&mut self, ctx: &egui::Context) {
        if !self.show_lyrics {
            return;
        }
        let Some(track) = self.audio.as_ref().and_then(|a| a.now_playing.clone()) else {
            return;
        };
        if self.lyrics_for.as_deref() == Some(track.video_id.as_str()) {
            return;
        }
        self.lyrics_for = Some(track.video_id.clone());
        self.lyrics = LyricsState::Loading;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .lyrics(&track.video_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Lyrics(result));
            ctx.request_repaint();
        });
    }

    /// The words, or why there are none.
    ///
    /// One label for the whole song, rather than one per line. egui wraps a
    /// label inside a vertical layout but not inside a horizontal one, and
    /// building this out of a line-per-label inside a horizontal is what made
    /// the longer lines clip at the panel edge. The blank lines in the text
    /// are the stanza breaks, and a label keeps them.
    fn lyrics_view(&self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(width);
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        top: 0,
                        bottom: 0,
                    })
                    .show(ui, |ui| match &self.lyrics {
                        LyricsState::Idle | LyricsState::Loading => {
                            ui.label(RichText::new("Looking for lyrics…").weak().small());
                        }
                        LyricsState::None => {
                            ui.label(RichText::new("This track has no lyrics.").weak().small());
                        }
                        LyricsState::Failed(why) => {
                            ui.label(
                                RichText::new(format!("Could not fetch lyrics: {why}"))
                                    .color(ui::WARN)
                                    .small(),
                            );
                        }
                        LyricsState::Ready(lyrics) => {
                            ui.label(RichText::new(&lyrics.text).size(14.0));
                            if let Some(source) = &lyrics.source {
                                ui.add_space(12.0);
                                ui.label(RichText::new(source.clone()).weak().small());
                            }
                        }
                    });
            });
    }

    /// Keep the displayed rating in step with the track that is playing.
    fn tick_like_status(&mut self, ctx: &egui::Context) {
        if !self.authenticated || self.like_inflight {
            return;
        }
        let Some(track) = self.audio.as_ref().and_then(|a| a.now_playing.clone()) else {
            return;
        };
        if self.like_for.as_deref() == Some(track.video_id.as_str()) {
            return;
        }
        self.like_inflight = true;
        self.like_for = Some(track.video_id.clone());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .like_status(&track.video_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::LikeStatus {
                video_id: track.video_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn spawn_set_like(&mut self, ctx: &egui::Context, video_id: String, status: LikeStatus) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = client
                .set_like(&video_id, status)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::LikeSet { video_id, result });
            ctx.request_repaint();
        });
    }

    /// YouTube Music's "Up next" list. Reuses the ordinary row so the playing
    /// track carries the same equaliser marker it has everywhere else.
    fn queue_panel(
        &mut self,
        ui: &mut egui::Ui,
        current_video: &Option<String>,
        action: &mut Option<Action>,
    ) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            if ui::tab(ui, "Up next", !self.show_lyrics).clicked() {
                self.show_lyrics = false;
            }
            ui.add_space(12.0);
            if ui::tab(ui, "Lyrics", self.show_lyrics).clicked() {
                self.show_lyrics = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(14.0);
                if !self.show_lyrics {
                    ui.label(
                        RichText::new(format!("{} in queue", self.queue.len()))
                            .weak()
                            .small(),
                    );
                }
            });
        });
        if self.show_lyrics {
            self.lyrics_view(ui);
            return;
        }
        ui.add_space(8.0);

        if self.queue.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(RichText::new("Nothing queued yet").weak());
            });
            return;
        }

        let active = self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false);
        let current = self.current;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, track) in self.queue.iter().enumerate() {
                    let selected = current == Some(idx)
                        && current_video.as_deref() == Some(track.video_id.as_str());
                    let mut segments: Vec<(String, Option<String>)> = Vec::new();
                    for (position, artist) in track.artists.iter().enumerate() {
                        if position > 0 {
                            segments.push((", ".to_string(), None));
                        }
                        segments.push((artist.clone(), track.artist_ids.get(position).cloned()));
                    }
                    let trailing = track.duration_text();
                    let row = ui::list_row(
                        ui,
                        &self.thumbnails,
                        ("queue", idx),
                        track.thumbnail.as_deref(),
                        &track.title,
                        &segments,
                        trailing.as_deref(),
                        Some(idx),
                        selected,
                        active,
                        false,
                        true,
                        false,
                    );
                    if row.clicked {
                        *action = Some(Action::PlayQueueIndex(idx));
                    }
                }
                ui.add_space(10.0);
            });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut toggle_pause = false;
        let mut step = 0i32;
        let mut seek_to: Option<f32> = None;
        let mut volume = self.volume;
        let mut shuffle = self.shuffle;
        let mut repeat = self.repeat;
        let mut show_queue = self.show_queue;
        let mut like_click: Option<LikeStatus> = None;
        let like_status = self.like_status;
        let likes_allowed = self.likes_allowed;
        let signed_in = self.authenticated;

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

                // Equal side widths put the transport cluster in the middle of
                // the *bar*, and reserving them exactly keeps it there when a
                // title gets shorter or longer.
                let side = (ui.available_width() * 0.28).clamp(220.0, 340.0);

                ui.horizontal(|ui| {
                    // Left: artwork and titles.
                    ui::fixed_section(
                        ui,
                        egui::vec2(side, row_height),
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
                                // Room is reserved for the thumbs whether or
                                // not they are drawn, so the title can't shift.
                                const THUMBS_W: f32 = 56.0;
                                ui::fixed_section(
                                    ui,
                                    egui::vec2((side - 76.0 - THUMBS_W).max(60.0), row_height),
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
                                // YouTube Music keeps the rating beside the
                                // title; rating needs a signed-in listener.
                                ui.add_space(4.0);
                                ui.add_enabled_ui(signed_in && likes_allowed, |ui| {
                                    if ui::transport_toggle(
                                        ui,
                                        "👍",
                                        None,
                                        like_status == LikeStatus::Like,
                                        26.0,
                                    )
                                    .clicked()
                                    {
                                        like_click = Some(LikeStatus::Like);
                                    }
                                    if ui::transport_toggle(
                                        ui,
                                        "👎",
                                        None,
                                        like_status == LikeStatus::Dislike,
                                        26.0,
                                    )
                                    .clicked()
                                    {
                                        like_click = Some(LikeStatus::Dislike);
                                    }
                                });
                            } else {
                                ui.label(RichText::new("Nothing playing").weak());
                            }
                        },
                    );

                    // Centre: transport, centred because the sides are equal.
                    let centre_width = (ui.available_width() - side).max(180.0);
                    ui::fixed_section(
                        ui,
                        egui::vec2(centre_width, row_height),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            let spacing = ui.spacing().item_spacing.x;
                            let buttons = 28.0 + 34.0 + 48.0 + 34.0 + 28.0 + spacing * 4.0;
                            ui.add_space(((centre_width - buttons) / 2.0).max(0.0));
                            // Transport is inert until something is loaded.
                            ui.add_enabled_ui(has_track, |ui| {
                                if ui::transport_toggle(ui, "🔀", None, shuffle, 28.0).clicked() {
                                    shuffle = !shuffle;
                                }
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
                                // Repeat-one gets a text badge: the "🔂" glyph
                                // is missing from many fonts and renders blank,
                                // so the state used to look like it did nothing.
                                let badge = (repeat == Repeat::One).then_some("1");
                                if ui::transport_toggle(
                                    ui,
                                    "🔁",
                                    badge,
                                    repeat != Repeat::Off,
                                    28.0,
                                )
                                .clicked()
                                {
                                    repeat = match repeat {
                                        Repeat::Off => Repeat::All,
                                        Repeat::All => Repeat::One,
                                        Repeat::One => Repeat::Off,
                                    };
                                }
                            });
                        },
                    );

                    // Right: volume.
                    ui::fixed_section(
                        ui,
                        egui::vec2(side, row_height),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            // Right padding so the slider doesn't touch the edge.
                            ui.add_space(14.0);
                            ui::fixed_section(
                                ui,
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
                            ui.add_space(2.0);
                            if ui::queue_icon(ui, show_queue, 26.0).clicked() {
                                show_queue = !show_queue;
                            }
                        },
                    );
                });

                // Progress row. The time labels get a fixed width too, or the
                // bar slides around as "0:05" becomes "12:34".
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    const TIME_W: f32 = 48.0;
                    ui.add_space(14.0);
                    ui::fixed_section(
                        ui,
                        egui::vec2(TIME_W, 16.0),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.label(RichText::new(ui::fmt_time(pos)).weak().small());
                        },
                    );
                    ui.add_space(10.0);
                    let bar_width = (ui.available_width() - TIME_W - 24.0).max(80.0);
                    ui::fixed_section(
                        ui,
                        egui::vec2(bar_width, 16.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if let Some(p) = ui::progress_bar(ui, pos, total) {
                                seek_to = Some(p);
                            }
                        },
                    );
                    ui.add_space(10.0);
                    ui::fixed_section(
                        ui,
                        egui::vec2(TIME_W, 16.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.label(RichText::new(ui::fmt_time(total)).weak().small());
                        },
                    );
                });
                ui.add_space(10.0);
            });

        if (self.volume - volume).abs() > 0.001 {
            self.volume = volume;
            self.pending_save = true;
            self.pending_save_at = Instant::now();
        }
        self.shuffle = shuffle;
        self.repeat = repeat;
        self.show_queue = show_queue;
        if let Some(clicked) = like_click {
            let next = self.like_status.toggled(clicked);
            if let Some(track) = self.audio.as_ref().and_then(|a| a.now_playing.clone()) {
                // Show it straight away; a failure re-reads the real state.
                self.like_status = next;
                self.spawn_set_like(ctx, track.video_id, next);
            }
        }
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
    /// Nothing should be lost on the way out, even if the app is closed in the
    /// same second the volume changed.
    fn on_exit(&mut self) {
        if self.pending_save {
            self.save_ui_settings();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if !self.theme_applied {
            ui::apply_theme(&ctx, self.theme_light);
            self.theme_applied = true;
        }

        self.tick_fps_probe();
        self.flush_pending_save(&ctx);
        if self.update_state == UpdateState::Idle && self.update_check_due() {
            self.spawn_update_check(&ctx);
        }
        self.drain_messages(&ctx);
        self.handle_audio_errors(&ctx);
        self.handle_shortcuts(&ctx);
        self.ensure_thumbnails(&ctx);
        self.tick_playback(&ctx);
        self.tick_deferred_play(&ctx);
        self.tick_like_status(&ctx);
        self.tick_lyrics(&ctx);
        self.tick_media();
        self.tick_demo(&ctx);
        self.playlist_picker(&ctx);
        self.playlist_dialogs(&ctx);
        self.settings_window(&ctx);
        self.sidebar(ui, &ctx);
        self.top_bar(ui, &ctx);
        self.bottom_bar(ui, &ctx);
        self.central(ui, &ctx);

        // The equaliser on the playing row is the one thing here that has to
        // look like motion, and it is drawn from a continuous clock — so the
        // frame rate is the only thing deciding whether it looks smooth. Four
        // frames a second, which is what the position label needs, reads as a
        // stutter; thirty is enough to look like an animation. It costs a
        // little while something is playing and nothing at all otherwise,
        // since a paused player holds its bars still.
        // "A track is loaded and not paused" rather than `is_playing()`: that
        // one is built on rodio's `Player::empty()`, which asks how many
        // sources are *queued* — and a single appended source leaves the queue
        // the moment the audio thread picks it up, so it reads as empty while
        // it is still playing. Depending on it here means the animation's frame
        // rate falls to whatever else happens to wake the app.
        let ticking = self
            .audio
            .as_ref()
            .is_some_and(|audio| audio.now_playing.is_some() && !audio.is_paused());
        if ticking {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.busy || !self.downloading.is_empty() {
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
            ui.label(ui::title(&page.title).size(26.0));
            if !page.subtitle.is_empty() {
                ui.label(RichText::new(&page.subtitle).weak());
            }
            if !page.description.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&page.description).weak().small());
            }
            ui.add_space(4.0);
            // Feeds put their tracks in shelves, so "0 tracks" would be both
            // wrong and prominent. Only count when there is something to count.
            if !page.tracks.is_empty() {
                ui.label(
                    RichText::new(format!("{} tracks", page.tracks.len()))
                        .weak()
                        .small(),
                );
            }
        });
    });
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(4.0);
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            video_id: id.to_string(),
            title: id.to_string(),
            artists: Vec::new(),
            artist_ids: Vec::new(),
            album: None,
            duration_secs: None,
            set_video_id: None,
            thumbnail: None,
        }
    }

    /// A device rebuild used to replace the queue with the recovered track
    /// alone, so when that track ended there was nothing to play next and
    /// playback stopped for good. The queue is still valid: the index survives.
    #[test]
    fn a_recovered_track_keeps_its_place_in_the_queue() {
        let queue = vec![track("a"), track("b"), track("c")];
        assert_eq!(recovery_index(&queue, Some(1), "b"), Some(1));
        assert_eq!(recovery_index(&queue, Some(0), "a"), Some(0));
    }

    #[test]
    fn a_queue_without_the_track_starts_a_new_run() {
        let queue = vec![track("a"), track("b")];
        assert_eq!(recovery_index(&queue, Some(1), "gone"), None);
        assert_eq!(recovery_index(&queue, None, "b"), None);
        assert_eq!(recovery_index(&[], Some(0), "b"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repeat off walks the queue and then stops.
    #[test]
    fn repeat_off_advances_then_stops() {
        assert_eq!(
            advance_decision(Repeat::Off, None, 3, Some(0)),
            Advance::Play(1)
        );
        assert_eq!(
            advance_decision(Repeat::Off, None, 3, Some(2)),
            Advance::Stop
        );
    }

    /// Repeat all wraps from the last track back to the first.
    #[test]
    fn repeat_all_wraps() {
        assert_eq!(
            advance_decision(Repeat::All, None, 3, Some(2)),
            Advance::Play(0)
        );
    }

    /// Repeat one replays the same track rather than moving on — the whole
    /// point of the mode, and the case that looked broken in the UI.
    #[test]
    fn repeat_one_replays_current() {
        assert_eq!(
            advance_decision(Repeat::One, None, 3, Some(1)),
            Advance::Play(1)
        );
        // Including on the last track, where the other modes would stop/wrap.
        assert_eq!(
            advance_decision(Repeat::One, None, 3, Some(2)),
            Advance::Play(2)
        );
    }

    /// Shuffle supplies its own index instead of walking the queue.
    #[test]
    fn shuffle_uses_its_index() {
        assert_eq!(
            advance_decision(Repeat::Off, Some(2), 3, Some(0)),
            Advance::Play(2)
        );
        // Repeat one still wins: it is a stronger statement than shuffle.
        assert_eq!(
            advance_decision(Repeat::One, Some(2), 3, Some(0)),
            Advance::Play(0)
        );
    }

    /// Nothing playing, or an empty queue, must not index out of bounds.
    #[test]
    fn stops_without_a_queue() {
        assert_eq!(advance_decision(Repeat::All, None, 0, None), Advance::Stop);
        assert_eq!(advance_decision(Repeat::One, None, 0, None), Advance::Stop);
        assert_eq!(advance_decision(Repeat::One, None, 3, None), Advance::Stop);
    }
}
