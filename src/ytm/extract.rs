//! Tolerant, name-based extraction of data from YouTube Music's renderer JSON.
//!
//! YouTube's response trees are deeply nested and get reshuffled regularly.
//! Instead of modelling every layer with serde structs, we walk the tree looking
//! for renderer names we care about. This keeps the client resilient to layout
//! changes: unknown wrappers are simply skipped.

use serde_json::Value;

use super::domain::{ItemKind, LikeStatus, Page, PageKind, SearchItem, Shelf, Track};

/// Tile artwork renders around 160px, so ask for a source near that.
const THUMBNAIL_TARGET: u32 = 220;

/// Collect every value stored under `name` anywhere in the tree.
pub fn find_renderers<'a>(v: &'a Value, name: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    collect(v, name, &mut out);
    out
}

/// First value stored under `name`, if any.
pub fn find_first<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    let mut out = Vec::new();
    collect_first(v, name, &mut out);
    out.into_iter().next()
}

fn collect<'a>(v: &'a Value, name: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == name {
                    out.push(val);
                }
                collect(val, name, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, name, out);
            }
        }
        _ => {}
    }
}

fn collect_first<'a>(v: &'a Value, name: &str, out: &mut Vec<&'a Value>) {
    if !out.is_empty() {
        return;
    }
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == name {
                    out.push(val);
                }
                collect_first(val, name, out);
                if !out.is_empty() {
                    return;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_first(item, name, out);
                if !out.is_empty() {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// Join a YouTube text node: `{ "simpleText" }` or `{ "runs": [{ "text" }] }`.
pub fn text(v: &Value) -> String {
    if let Some(s) = v.get("simpleText").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(runs) = v.get("runs").and_then(Value::as_array) {
        let mut s = String::new();
        for run in runs {
            if let Some(t) = run.get("text").and_then(Value::as_str) {
                s.push_str(t);
            }
        }
        return s;
    }
    String::new()
}

/// Pick a thumbnail close to `target` pixels wide: the smallest one that is at
/// least `target`, otherwise the largest available. Asking for the biggest
/// (544px) wastes bandwidth when tiles render at ~160px.
pub fn thumbnail_for(v: &Value, target: u32) -> Option<String> {
    let mut smallest_big_enough: Option<(u32, String)> = None;
    let mut largest: Option<(u32, String)> = None;

    for renderer in find_renderers(v, "musicThumbnailRenderer") {
        let Some(thumbs) = renderer
            .get("thumbnail")
            .and_then(|t| t.get("thumbnails"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for thumb in thumbs {
            let url = thumb.get("url").and_then(Value::as_str).unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let width = thumb.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            if largest.as_ref().is_none_or(|(best, _)| width > *best) {
                largest = Some((width, url.to_string()));
            }
            if width >= target
                && smallest_big_enough
                    .as_ref()
                    .is_none_or(|(best, _)| width < *best)
            {
                smallest_big_enough = Some((width, url.to_string()));
            }
        }
    }

    smallest_big_enough.or(largest).map(|(_, url)| url)
}

/// Parse `m:ss` or `h:mm:ss` into seconds.
pub fn parse_duration(s: &str) -> Option<u32> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let mut secs: u32 = 0;
    for p in &parts {
        secs = secs.checked_mul(60)?.checked_add(p.parse::<u32>().ok()?)?;
    }
    Some(secs)
}

/// One text run, with any endpoint it points at.
#[derive(Clone, Debug, Default)]
struct Run {
    text: String,
    browse_id: Option<String>,
    page_type: Option<String>,
    watch_id: Option<String>,
}

fn endpoint_ids(endpoint: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let browse = endpoint.get("browseEndpoint");
    let browse_id = browse
        .and_then(|b| b.get("browseId"))
        .and_then(Value::as_str)
        .map(String::from);
    let page_type = browse
        .and_then(|b| b.get("browseEndpointContextSupportedConfigs"))
        .and_then(|c| c.get("browseEndpointContextMusicConfig"))
        .and_then(|m| m.get("pageType"))
        .and_then(Value::as_str)
        .map(String::from);
    let watch_id = endpoint
        .get("watchEndpoint")
        .and_then(|w| w.get("videoId"))
        .and_then(Value::as_str)
        .map(String::from);
    (browse_id, page_type, watch_id)
}

fn runs_of(text_node: &Value) -> Vec<Run> {
    let mut out = Vec::new();
    if let Some(runs) = text_node.get("runs").and_then(Value::as_array) {
        for r in runs {
            let (browse_id, page_type, watch_id) = r
                .get("navigationEndpoint")
                .map(endpoint_ids)
                .unwrap_or((None, None, None));
            out.push(Run {
                text: r
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                browse_id,
                page_type,
                watch_id,
            });
        }
    } else if let Some(s) = text_node.get("simpleText").and_then(Value::as_str) {
        out.push(Run {
            text: s.to_string(),
            ..Default::default()
        });
    }
    out
}

/// The per-row text columns of a `musicResponsiveListItemRenderer`.
fn flex_columns(item: &Value) -> Vec<Vec<Run>> {
    let mut out = Vec::new();
    if let Some(cols) = item.get("flexColumns").and_then(Value::as_array) {
        for col in cols {
            let text_node = col
                .get("musicResponsiveListItemFlexColumnRenderer")
                .and_then(|r| r.get("text"));
            out.push(text_node.map(runs_of).unwrap_or_default());
        }
    }
    out
}

/// The right-hand fixed columns (usually duration) of a list row.
fn fixed_texts(item: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(cols) = item.get("fixedColumns").and_then(Value::as_array) {
        for col in cols {
            if let Some(t) = col
                .get("musicResponsiveListItemFixedColumnRenderer")
                .and_then(|r| r.get("text"))
            {
                out.push(text(t));
            }
        }
    }
    out
}

fn is_separator(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || matches!(t, "•" | "&" | "," | "·" | "/" | "|" | "-")
}

/// Best-effort artist names from a secondary column.
fn artists_from_runs(runs: &[Run]) -> Vec<String> {
    let named: Vec<String> = runs
        .iter()
        .filter(|r| r.page_type.as_deref() == Some("MUSIC_PAGE_TYPE_ARTIST"))
        .map(|r| r.text.trim().to_string())
        .filter(|t| !is_separator(t))
        .collect();
    if !named.is_empty() {
        return named;
    }
    // Fallback: everything before the first "•" separator.
    let mut out = Vec::new();
    for r in runs {
        if r.text.contains('•') {
            break;
        }
        if !is_separator(&r.text) {
            out.push(r.text.trim().to_string());
        }
    }
    out
}

fn album_from_runs(runs: &[Run]) -> Option<String> {
    runs.iter()
        .find(|r| r.page_type.as_deref() == Some("MUSIC_PAGE_TYPE_ALBUM"))
        .map(|r| r.text.trim().to_string())
}

fn artist_ids_from_runs(runs: &[Run]) -> Vec<String> {
    runs.iter()
        .filter(|run| run.page_type.as_deref() == Some("MUSIC_PAGE_TYPE_ARTIST"))
        .filter_map(|run| run.browse_id.clone())
        .collect()
}

/// Split a row's secondary column into labelled parts; linked parts get a
/// browse id so the UI can make just those clickable.
fn segments_from_runs(runs: &[Run]) -> Vec<(String, Option<String>)> {
    runs.iter()
        .map(|run| {
            let link = matches!(
                run.page_type.as_deref(),
                Some("MUSIC_PAGE_TYPE_ARTIST") | Some("MUSIC_PAGE_TYPE_ALBUM")
            )
            .then(|| run.browse_id.clone())
            .flatten();
            (run.text.clone(), link)
        })
        .collect()
}

fn duration_from(item: &Value, cols: &[Vec<Run>]) -> Option<u32> {
    for t in fixed_texts(item) {
        if let Some(d) = parse_duration(&t) {
            return Some(d);
        }
    }
    for col in cols {
        for r in col {
            if let Some(d) = parse_duration(&r.text) {
                return Some(d);
            }
        }
    }
    None
}

fn video_id_of(item: &Value, cols: &[Vec<Run>]) -> Option<String> {
    item.get("playlistItemData")
        .and_then(|d| d.get("videoId"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            cols.iter()
                .flat_map(|c| c.iter())
                .find_map(|r| r.watch_id.clone())
        })
}

/// Extract a playable [`Track`] from a list row. Returns `None` for rows that
/// are not tracks (albums, artists, ...).
pub fn parse_track(item: &Value) -> Option<Track> {
    let cols = flex_columns(item);
    let title = cols
        .first()
        .and_then(|c| c.first())
        .map(|r| r.text.trim().to_string())
        .filter(|t| !t.is_empty())?;
    let video_id = video_id_of(item, &cols)?;
    let subtitle_runs = cols.get(1).cloned().unwrap_or_default();
    Some(Track {
        video_id,
        title,
        artists: artists_from_runs(&subtitle_runs),
        artist_ids: artist_ids_from_runs(&subtitle_runs),
        album: album_from_runs(&subtitle_runs),
        duration_secs: duration_from(item, &cols),
        set_video_id: set_video_id_of(item),
        thumbnail: thumbnail_for(item, THUMBNAIL_TARGET),
    })
}

/// The id of a track's entry within the playlist it was listed in.
fn set_video_id_of(item: &Value) -> Option<String> {
    find_first(item, "playlistItemData")
        .and_then(|data| data.get("playlistSetVideoId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn kind_from_page_type(page_type: Option<&str>) -> Option<ItemKind> {
    Some(match page_type? {
        "MUSIC_PAGE_TYPE_ALBUM" => ItemKind::Album,
        "MUSIC_PAGE_TYPE_ARTIST" => ItemKind::Artist,
        "MUSIC_PAGE_TYPE_PLAYLIST" => ItemKind::Playlist,
        "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE" | "MUSIC_PAGE_TYPE_PODCAST" => ItemKind::Podcast,
        "MUSIC_PAGE_TYPE_USER_CHANNEL" => ItemKind::Profile,
        _ => return None,
    })
}

fn kind_from_label(label: &str) -> Option<ItemKind> {
    Some(match label {
        "Song" => ItemKind::Song,
        "Video" => ItemKind::Video,
        "Album" | "Single" | "EP" => ItemKind::Album,
        "Artist" => ItemKind::Artist,
        "Playlist" | "Community playlist" => ItemKind::Playlist,
        "Podcast" | "Episode" | "Episodes" => ItemKind::Podcast,
        "Profile" => ItemKind::Profile,
        _ => return None,
    })
}

fn parse_search_item(item: &Value) -> Option<SearchItem> {
    let cols = flex_columns(item);
    let title = cols
        .first()
        .and_then(|c| c.first())
        .map(|r| r.text.trim().to_string())
        .filter(|t| !t.is_empty())?;

    let nav = item.get("navigationEndpoint");
    let (browse_id, page_type) = nav
        .map(endpoint_ids)
        .map(|(b, p, _)| (b, p))
        .unwrap_or((None, None));
    let video_id = video_id_of(item, &cols);

    let subtitle_runs = cols.get(1).cloned().unwrap_or_default();
    let kind = kind_from_page_type(page_type.as_deref())
        .or_else(|| subtitle_runs.first().and_then(|r| kind_from_label(&r.text)))
        .or_else(|| {
            if video_id.is_some() {
                Some(ItemKind::Song)
            } else {
                None
            }
        })
        .unwrap_or(ItemKind::Other);

    Some(SearchItem {
        title,
        artists: artists_from_runs(&subtitle_runs),
        subtitle: subtitle_runs.iter().map(|r| r.text.clone()).collect(),
        segments: segments_from_runs(&subtitle_runs),
        kind,
        browse_id,
        video_id,
        thumbnail: thumbnail_for(item, THUMBNAIL_TARGET),
        duration_secs: duration_from(item, &cols),
    })
}

/// All rows of a (filtered) search response, de-duplicated and order-preserving.
pub fn parse_search_items(v: &Value) -> Vec<SearchItem> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in find_renderers(v, "musicResponsiveListItemRenderer") {
        let Some(si) = parse_search_item(item) else {
            continue;
        };
        let key = si
            .video_id
            .clone()
            .or_else(|| si.browse_id.clone())
            .unwrap_or_else(|| si.title.clone());
        if seen.insert(key) {
            out.push(si);
        }
    }
    out
}

/// Extract a tile from a `musicTwoRowItemRenderer` (album / playlist / artist).
pub fn parse_two_row_item(item: &Value) -> Option<SearchItem> {
    let title = item.get("title").map(text).unwrap_or_default();
    let title = title.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let (browse_id, page_type, watch_id) = item
        .get("navigationEndpoint")
        .map(endpoint_ids)
        .unwrap_or((None, None, None));
    let subtitle_runs = item.get("subtitle").map(runs_of).unwrap_or_default();
    let kind = kind_from_page_type(page_type.as_deref())
        .or_else(|| subtitle_runs.first().and_then(|r| kind_from_label(&r.text)))
        .unwrap_or(if watch_id.is_some() {
            ItemKind::Song
        } else {
            ItemKind::Other
        });
    Some(SearchItem {
        title,
        artists: artists_from_runs(&subtitle_runs),
        subtitle: subtitle_runs.iter().map(|r| r.text.clone()).collect(),
        segments: segments_from_runs(&subtitle_runs),
        kind,
        browse_id,
        video_id: watch_id,
        thumbnail: thumbnail_for(item, THUMBNAIL_TARGET),
        duration_secs: None,
    })
}

/// Collect the carousels/shelves of browsable tiles on a page.
pub fn parse_shelves(v: &Value) -> Vec<Shelf> {
    let mut out = Vec::new();
    for carousel in find_renderers(v, "musicCarouselShelfRenderer") {
        let title = carousel
            .get("header")
            .and_then(|h| h.get("musicCarouselShelfBasicHeaderRenderer"))
            .and_then(|h| h.get("title"))
            .map(text)
            .unwrap_or_default();
        let mut items: Vec<SearchItem> = find_renderers(carousel, "musicTwoRowItemRenderer")
            .into_iter()
            .filter_map(parse_two_row_item)
            .collect();
        // Some carousels — e.g. the home page's "Quick picks" — hold list rows
        // rather than tiles.
        if items.is_empty() {
            items = find_renderers(carousel, "musicResponsiveListItemRenderer")
                .into_iter()
                .filter_map(parse_track)
                .map(|track| SearchItem::from_track(&track))
                .collect();
        }
        if !items.is_empty() {
            out.push(Shelf { title, items });
        }
    }

    // The library landing page uses an untitled grid instead of carousels.
    for grid in find_renderers(v, "gridRenderer") {
        let items: Vec<SearchItem> = find_renderers(grid, "musicTwoRowItemRenderer")
            .into_iter()
            .filter_map(parse_two_row_item)
            .collect();
        if !items.is_empty() {
            out.push(Shelf {
                title: String::new(),
                items,
            });
        }
    }
    out
}

/// Friendly names for the account-relative browse ids.
fn friendly_title(browse_id: &str) -> String {
    match browse_id {
        "FEmusic_library_landing" => "Your Library".to_string(),
        "FEmusic_liked_videos" => "Liked Songs".to_string(),
        "FEmusic_liked_playlists" => "Liked Playlists".to_string(),
        "FEmusic_library_albums" => "Library Albums".to_string(),
        "FEmusic_library_artists" => "Library Artists".to_string(),
        "FEmusic_home" => "Home".to_string(),
        "FEmusic_explore" => "Explore".to_string(),
        "VLLM" => "Liked Music".to_string(),
        "VLSE" => "Episodes for Later".to_string(),
        _ if browse_id.starts_with("FEmusic_") => "Library".to_string(),
        _ => browse_id.to_string(),
    }
}

/// Collect the page's main track list, skipping anything inside a carousel
/// (those become shelves instead).
pub fn collect_tracks(v: &Value, out: &mut Vec<Track>) {
    match v {
        Value::Object(map) => {
            for (key, value) in map {
                // Carousel contents are rendered as shelves.
                if key == "musicCarouselShelfRenderer" {
                    continue;
                }
                if key == "musicResponsiveListItemRenderer"
                    && let Some(track) = parse_track(value)
                {
                    out.push(track);
                }
                collect_tracks(value, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_tracks(item, out);
            }
        }
        _ => {}
    }
}

fn first_continuation(v: &Value) -> Option<String> {
    let entries = v.get("continuations")?.as_array()?;
    for entry in entries {
        for key in ["nextContinuationData", "reloadContinuationData"] {
            if let Some(token) = entry
                .get(key)
                .and_then(|d| d.get("continuation"))
                .and_then(Value::as_str)
            {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// The token for loading further shelves (feeds page in as you scroll).
///
/// Continuation *responses* use `sectionListContinuation` where the first
/// response uses `sectionListRenderer`, so accept either.
/// A token for fetching the *next* page, wherever it sits in a response.
///
/// Deliberately ignores `reloadContinuationData`, which looks like a paging
/// token but is not: sending one back returns an empty page. A grouped "All"
/// search offers only that kind — its results are already complete — while a
/// single-category search offers a real `nextContinuationData` token worth 20
/// more rows, which is why "load more" appears in the expanded view only.
pub fn parse_any_continuation(v: &Value) -> Option<String> {
    if let Some(token) = find_first(v, "continuationCommand")
        .and_then(|command| command.get("token"))
        .and_then(Value::as_str)
    {
        return Some(token.to_string());
    }
    find_first(v, "nextContinuationData")
        .and_then(|data| data.get("continuation"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn parse_continuation(v: &Value) -> Option<String> {
    for name in ["sectionListContinuation", "sectionListRenderer"] {
        for node in find_renderers(v, name) {
            if let Some(token) = first_continuation(node) {
                return Some(token);
            }
        }
    }
    None
}

/// Number of tracks a page claims to have, read from its header
/// (e.g. "243 tracks • 15 hours").
fn parse_track_count(header: &Value) -> Option<usize> {
    for key in ["secondSubtitle", "subtitle"] {
        let Some(node) = header.get(key) else {
            continue;
        };
        // e.g. "9.6K views • 243 tracks • 16+ hours"
        for part in text(node).to_lowercase().split(['•', ',', '|']) {
            let part = part.trim();
            if !(part.contains("track") || part.contains("song")) {
                continue;
            }
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(count) = digits.parse() {
                return Some(count);
            }
        }
    }
    None
}

/// The raw playlist id for the player-panel endpoint, when this page is a real
/// playlist (`VLRDCLAK5uy_…` / `VLPL…`) rather than a collection like `VLLM`.
pub fn parse_playlist_id(browse_id: &str) -> Option<String> {
    let rest = browse_id.strip_prefix("VL")?;
    (rest.starts_with("PL") || rest.starts_with("RD") || rest.starts_with("LA"))
        .then(|| rest.to_string())
}

/// Tracks from a `next` (player panel) response, used to page in the rest of a
/// long playlist.
pub fn parse_panel_tracks(v: &Value) -> Vec<Track> {
    find_renderers(v, "playlistPanelVideoRenderer")
        .into_iter()
        .filter_map(|item| {
            let video_id = item.get("videoId").and_then(Value::as_str)?.to_string();
            let title = item.get("title").map(text).unwrap_or_default();
            if title.is_empty() {
                return None;
            }
            let byline = item.get("longBylineText").map(runs_of).unwrap_or_default();
            Some(Track {
                video_id,
                title,
                artists: artists_from_runs(&byline),
                artist_ids: artist_ids_from_runs(&byline),
                album: album_from_runs(&byline),
                duration_secs: item
                    .get("lengthText")
                    .map(text)
                    .and_then(|length| parse_duration(&length)),
                // Panel entries belong to the player queue, not a playlist.
                set_video_id: None,
                thumbnail: thumbnail_for(item, THUMBNAIL_TARGET),
            })
        })
        .collect()
}

fn page_kind(browse_id: &str) -> PageKind {
    if browse_id.starts_with("MPREb") || browse_id.starts_with("MPRE") {
        PageKind::Album
    } else if browse_id.starts_with("VL") || browse_id.starts_with("RD") {
        PageKind::Playlist
    } else if browse_id.starts_with("UC") {
        PageKind::Artist
    } else if browse_id.starts_with("MPSP") {
        PageKind::Podcast
    } else {
        PageKind::Unknown
    }
}

/// Parse a `browse` response into a [`Page`].
pub fn parse_page(browse_id: &str, v: &Value) -> Page {
    const HEADER_NAMES: [&str; 3] = [
        "musicResponsiveHeaderRenderer",
        "musicImmersiveHeaderRenderer",
        "musicDetailHeaderRenderer",
    ];
    let header = HEADER_NAMES.iter().find_map(|name| find_first(v, name));

    let mut title = String::new();
    let mut subtitle = String::new();
    let mut description = String::new();
    let mut thumbnail = None;
    let mut header_artist: Option<String> = None;
    if let Some(h) = header {
        title = h.get("title").map(text).unwrap_or_default();
        subtitle = h
            .get("subtitle")
            .map(text)
            .or_else(|| h.get("secondSubtitle").map(text))
            .unwrap_or_default();
        if let Some(ds) = find_first(h, "musicDescriptionShelfRenderer") {
            description = ds.get("description").map(text).unwrap_or_default();
        }
        thumbnail = thumbnail_for(h, THUMBNAIL_TARGET);
        // Album/single pages carry the artist here rather than on each row.
        header_artist = ["straplineTextOne", "straplineTextTwo"]
            .iter()
            .find_map(|key| h.get(*key))
            .map(text)
            .filter(|name| !name.trim().is_empty());
    }

    let kind = page_kind(browse_id);

    let mut tracks: Vec<Track> = Vec::new();
    collect_tracks(v, &mut tracks);
    // Album rows often omit the artist (implied) and the artwork, so fill the
    // gaps from the header instead of showing "Unknown artist" and blanks.
    for track in &mut tracks {
        if track.artists.is_empty()
            && kind == PageKind::Album
            && let Some(artist) = &header_artist
        {
            track.artists = vec![artist.clone()];
        }
        if track.thumbnail.is_none() {
            track.thumbnail = thumbnail.clone();
        }
    }

    let shelves = parse_shelves(v);
    let continuation = parse_continuation(v);
    let track_count = header.and_then(parse_track_count);
    let playlist_id = parse_playlist_id(browse_id);

    Page {
        browse_id: browse_id.to_string(),
        kind,
        title: if title.is_empty() {
            friendly_title(browse_id)
        } else {
            title
        },
        subtitle,
        description,
        thumbnail: thumbnail.or_else(|| tracks.first().and_then(|t| t.thumbnail.clone())),
        tracks,
        shelves,
        continuation,
        track_count,
        playlist_id,
        owner_is_user: owner_is_user(v),
    }
}

/// Read the rating state from a watch-next response.
///
/// The player overlays carry it: `likeButtonRenderer.likeStatus` is one of
/// `INDIFFERENT` / `LIKE` / `DISLIKE`, next to whether rating is allowed at all.
/// Missing entirely (older or reshuffled responses) reads as "not rated, and
/// not rateable", which is the safe direction to fail in.
/// The browse id of a track's lyrics page, if the player response offers one.
///
/// The watch-next response lists its tabs, and the lyrics one carries a browse
/// endpoint rather than any words itself, so the text takes a second request.
/// A track without lyrics simply has no such tab.
pub fn lyrics_browse_id(v: &Value) -> Option<String> {
    let mut found = None;
    collect_lyrics_id(v, &mut found);
    found
}

/// Whether this description shelf is a lyrics page rather than a blurb.
///
/// Both use `musicDescriptionShelfRenderer`: an album's Wikipedia summary is
/// one, and so is a lyrics page. The lyrics shelf is the collapsible one with
/// a source credit; a blurb offers a "more" button instead. Reading the wrong
/// one would show an album's description as the words of a song, which is
/// exactly what the test for this caught.
fn is_lyrics_shelf(shelf: &Value) -> bool {
    let has = |key: &str| shelf.get(key).is_some();
    (has("maxExpandedLines") || has("footer")) && !has("moreButton")
}

fn collect_lyrics_id(node: &Value, found: &mut Option<String>) {
    if found.is_some() {
        return;
    }
    match node {
        Value::Object(map) => {
            if let Some(endpoint) = map.get("browseEndpoint").and_then(Value::as_object) {
                let page_type = endpoint
                    .get("browseEndpointContextSupportedConfigs")
                    .and_then(|configs| configs.get("browseEndpointContextMusicConfig"))
                    .and_then(|music| music.get("pageType"))
                    .and_then(Value::as_str);
                if page_type == Some("MUSIC_PAGE_TYPE_TRACK_LYRICS")
                    && let Some(id) = endpoint.get("browseId").and_then(Value::as_str)
                {
                    *found = Some(id.to_string());
                    return;
                }
            }
            for value in map.values() {
                collect_lyrics_id(value, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_lyrics_id(item, found);
            }
        }
        _ => {}
    }
}

/// The words themselves, from the page [`lyrics_browse_id`] points at.
///
/// The page is a description shelf holding the lyrics as text runs, newlines
/// and all. YouTube's web response carries no timing with them, so there is
/// nothing to follow along with — the text is all there is.
pub fn parse_lyrics(v: &Value) -> Option<String> {
    let shelf = find_renderers(v, "musicDescriptionShelfRenderer")
        .into_iter()
        .find(|shelf| is_lyrics_shelf(shelf))?;
    let runs = shelf.get("description")?.get("runs")?.as_array()?;
    let mut text = String::new();
    for run in runs {
        if let Some(part) = run.get("text").and_then(Value::as_str) {
            text.push_str(part);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

/// Who the lyrics are licensed from, as the page credits it.
pub fn lyrics_source(v: &Value) -> Option<String> {
    let shelf = find_renderers(v, "musicDescriptionShelfRenderer")
        .into_iter()
        .find(|shelf| is_lyrics_shelf(shelf))?;
    let runs = shelf.get("footer")?.get("runs")?.as_array()?;
    let mut text = String::new();
    for run in runs {
        if let Some(part) = run.get("text").and_then(Value::as_str) {
            text.push_str(part);
        }
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub fn parse_like_status(v: &Value) -> (LikeStatus, bool) {
    let Some(button) = find_first(v, "likeButtonRenderer") else {
        return (LikeStatus::Indifferent, false);
    };
    let status = match button.get("likeStatus").and_then(Value::as_str) {
        Some("LIKE") => LikeStatus::Like,
        Some("DISLIKE") => LikeStatus::Dislike,
        _ => LikeStatus::Indifferent,
    };
    let allowed = button
        .get("likesAllowed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (status, allowed)
}

/// Whether a playlist's header credits a *user* as its owner.
///
/// Editorial playlists (YouTube Music's own) have a facepile with no linked
/// channel; a playlist owned by a person links to a `MUSIC_PAGE_TYPE_USER_CHANNEL`.
/// Note this says "owned by a user", not "owned by *this* user": a Google
/// account can control several channels, and the playlist's owner need not be
/// the account's active one, so comparing against the signed-in channel does not
/// work. It is used only to keep the destructive action off editorial playlists,
/// with the server remaining the real authority.
pub fn owner_is_user(v: &Value) -> bool {
    find_first(v, "avatarStackViewModel")
        .and_then(|facepile| find_first(facepile, "browseEndpoint"))
        .and_then(|endpoint| endpoint.get("browseEndpointContextSupportedConfigs"))
        .and_then(|configs| find_first(configs, "pageType"))
        .and_then(Value::as_str)
        .is_some_and(|page_type| page_type == "MUSIC_PAGE_TYPE_USER_CHANNEL")
}

#[cfg(test)]
mod like_tests {
    use super::*;
    use serde_json::json;

    /// The shape captured from a real watch-next response, trimmed to the parts
    /// that matter.
    fn response(status: &str, allowed: bool) -> Value {
        json!({
            "playerOverlays": { "playerOverlayRenderer": { "actions": [ {
                "likeButtonRenderer": {
                    "target": { "videoId": "khnokW3Mw24" },
                    "likeStatus": status,
                    "likesAllowed": allowed
                }
            } ] } }
        })
    }

    #[test]
    fn reads_the_rating_from_the_player_overlays() {
        assert_eq!(
            parse_like_status(&response("LIKE", true)),
            (LikeStatus::Like, true)
        );
        assert_eq!(
            parse_like_status(&response("DISLIKE", true)),
            (LikeStatus::Dislike, true)
        );
        assert_eq!(
            parse_like_status(&response("INDIFFERENT", true)),
            (LikeStatus::Indifferent, true)
        );
    }

    /// A reshuffled or truncated response must not claim the track is rateable.
    #[test]
    fn missing_overlays_read_as_unrated_and_unrateable() {
        assert_eq!(
            parse_like_status(&json!({})),
            (LikeStatus::Indifferent, false)
        );
        assert_eq!(
            parse_like_status(&json!({ "playerOverlays": {} })),
            (LikeStatus::Indifferent, false)
        );
    }
}

/// Tests against captured InnerTube responses.
///
/// The parser walks the renderer tree by name, which is what makes it tolerant
/// of shape changes — and how it fails: a renamed renderer or a moved field
/// means extraction returns nothing and the app goes quietly empty. These are
/// real responses, captured by `tools/capture-fixtures.py`.
///
/// The assertions are the contract the app depends on rather than exact
/// content: counts are floors, titles are non-empty, ids look like ids. So
/// re-capturing should not break them unless the shape really changed, which
/// is the signal worth having.
#[cfg(test)]
mod captured {
    use super::*;
    use crate::ytm::domain::{ItemKind, PageKind};
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/{name}.json.gz",
            env!("CARGO_MANIFEST_DIR")
        );
        let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut text = String::new();
        GzDecoder::new(std::io::BufReader::new(file))
            .read_to_string(&mut text)
            .expect("fixture should be gzip");
        serde_json::from_str(&text).expect("fixture should be JSON")
    }

    #[test]
    fn an_unfiltered_search_yields_rows_of_several_kinds() {
        let items = parse_search_items(&fixture("search_all"));
        assert!(
            items.len() >= 20,
            "expected a page of rows, got {}",
            items.len()
        );
        for item in &items {
            assert!(!item.title.is_empty(), "row with no title: {item:?}");
        }

        // The All view is grouped client-side, so the response itself has to
        // carry more than one kind or the sections come out empty.
        let kinds: Vec<ItemKind> = items.iter().map(|item| item.kind).collect();
        assert!(kinds.contains(&ItemKind::Song), "no songs in {kinds:?}");
        assert!(kinds.len() >= 3, "expected several kinds, got {kinds:?}");

        assert!(
            items.iter().any(|item| item.video_id.is_some()),
            "no playable row anywhere"
        );
        assert!(
            items.iter().any(|item| item.duration_secs.is_some()),
            "no durations anywhere"
        );
        assert!(
            items.iter().any(|item| item.browse_id.is_some()),
            "nothing to open anywhere"
        );
        assert!(
            items.iter().any(|item| item.thumbnail.is_some()),
            "no artwork anywhere"
        );
        assert!(
            items.iter().any(|item| !item.artists.is_empty()),
            "no artist names anywhere"
        );
    }

    #[test]
    fn a_filtered_search_is_a_page_of_song_rows() {
        // A filtered search still arrives in a shelf, but the app labels that
        // section itself and shows the rows — so what has to hold is that the
        // rows are there and read as the kind that was asked for.
        let value = fixture("search_songs");
        assert!(
            find_first(&value, "musicShelfRenderer").is_some(),
            "a filtered search should still arrive in a shelf"
        );
        let items = parse_search_items(&value);
        assert!(items.len() >= 10, "got {} rows", items.len());
        assert!(items.iter().all(|item| !item.title.is_empty()));
        assert!(items.iter().any(|item| item.video_id.is_some()));
        let songs = items
            .iter()
            .filter(|item| item.kind == ItemKind::Song)
            .count();
        assert!(
            songs >= 8,
            "only {songs} of {} rows read as songs",
            items.len()
        );
    }

    #[test]
    fn an_album_page_carries_its_tracks_and_its_description() {
        // The id is synthetic: parse_page only reads its prefix to decide what
        // kind of page this is, and a made-up one survives re-capturing.
        let page = parse_page("MPREb_fixture", &fixture("browse_album"));
        assert!(
            matches!(page.kind, PageKind::Album),
            "read as {:?}",
            page.kind
        );
        assert!(!page.title.is_empty());
        assert!(page.thumbnail.is_some(), "the header art");
        assert!(!page.description.is_empty(), "the blurb under the header");
        assert!(page.tracks.len() >= 5, "got {} tracks", page.tracks.len());
        assert!(
            page.track_count.is_some_and(|n| n >= page.tracks.len()),
            "track count {:?} against {} tracks",
            page.track_count,
            page.tracks.len()
        );
        for track in &page.tracks {
            assert!(!track.video_id.is_empty(), "track with no id: {track:?}");
            assert!(!track.title.is_empty(), "track with no title: {track:?}");
        }
        assert!(page.tracks.iter().any(|t| t.duration_secs.is_some()));
        assert!(page.tracks.iter().any(|t| !t.artists.is_empty()));
    }

    #[test]
    fn an_artist_page_is_shelves_of_openable_items() {
        let page = parse_page("UCfixture", &fixture("browse_artist"));
        assert!(
            matches!(page.kind, PageKind::Artist),
            "read as {:?}",
            page.kind
        );
        assert!(!page.title.is_empty());
        assert!(
            page.shelves.len() >= 2,
            "got {} shelves",
            page.shelves.len()
        );
        for shelf in &page.shelves {
            assert!(!shelf.title.is_empty(), "a shelf with no title");
            assert!(!shelf.items.is_empty(), "shelf {:?} is empty", shelf.title);
            assert!(shelf.items.iter().all(|item| !item.title.is_empty()));
        }
        // Some of those have to be openable, or the page is a dead end.
        assert!(
            page.shelves
                .iter()
                .flat_map(|shelf| &shelf.items)
                .any(|item| item.browse_id.is_some()),
            "nothing on the page can be opened"
        );
    }

    #[test]
    fn a_playlist_page_carries_its_entries() {
        let page = parse_page("VLPLfixture", &fixture("browse_playlist"));
        assert!(
            matches!(page.kind, PageKind::Playlist),
            "read as {:?}",
            page.kind
        );
        assert!(!page.title.is_empty());
        assert!(page.tracks.len() >= 5, "got {} tracks", page.tracks.len());
        for track in &page.tracks {
            assert!(!track.video_id.is_empty(), "entry with no id: {track:?}");
            assert!(!track.title.is_empty(), "entry with no title: {track:?}");
        }
        assert!(page.tracks.iter().any(|t| !t.artists.is_empty()));
    }

    #[test]
    fn the_home_feed_is_shelves() {
        let value = fixture("browse_home");
        // Home is a feed rather than a detail page, so it has no kind of its
        // own; what matters is that its shelves come through.
        let page = parse_page("FEmusic_home", &value);
        assert!(
            matches!(page.kind, PageKind::Unknown),
            "read as {:?}",
            page.kind
        );

        let shelves = parse_shelves(&value);
        assert!(shelves.len() >= 3, "got {} shelves", shelves.len());
        for shelf in &shelves {
            assert!(!shelf.title.is_empty(), "a shelf with no title");
            assert!(!shelf.items.is_empty(), "shelf {:?} is empty", shelf.title);
        }
    }

    #[test]
    fn a_player_response_points_at_the_lyrics_page() {
        let id = lyrics_browse_id(&fixture("next")).expect("this track has lyrics");
        assert!(id.starts_with("MPLYt"), "unexpected lyrics id: {id}");
    }

    #[test]
    fn the_lyrics_page_holds_the_words() {
        let value = fixture("lyrics");
        let text = parse_lyrics(&value).expect("the page should carry lyrics");
        assert!(
            text.lines().count() >= 8,
            "only {} lines",
            text.lines().count()
        );
        assert!(
            text.contains('\n'),
            "the words should keep their line breaks"
        );
        assert_eq!(
            lyrics_source(&value).as_deref(),
            Some("Source: Musixmatch"),
            "the credit should come through"
        );
    }

    /// A track without lyrics has no tab to follow, and the parser has to say
    /// so rather than inventing an empty page.
    #[test]
    fn a_track_without_lyrics_offers_no_page() {
        assert_eq!(lyrics_browse_id(&fixture("browse_album")), None);
        assert_eq!(parse_lyrics(&fixture("browse_album")), None);
    }

    /// Every test above would pass by finding nothing if a fixture stopped
    /// carrying the renderers the parser walks, so check that directly.
    #[test]
    fn fixtures_still_carry_the_renderers_the_parser_walks() {
        for (name, renderer) in [
            ("search_all", "musicResponsiveListItemRenderer"),
            ("browse_album", "musicShelfRenderer"),
            ("browse_album", "musicTwoRowItemRenderer"),
            ("browse_home", "musicCarouselShelfRenderer"),
        ] {
            assert!(
                find_first(&fixture(name), renderer).is_some(),
                "{name} no longer contains {renderer}"
            );
        }
    }
}
