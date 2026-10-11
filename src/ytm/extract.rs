//! Tolerant, name-based extraction of data from YouTube Music's renderer JSON.
//!
//! YouTube's response trees are deeply nested and get reshuffled regularly.
//! Instead of modelling every layer with serde structs, we walk the tree looking
//! for renderer names we care about. This keeps the client resilient to layout
//! changes: unknown wrappers are simply skipped.

use serde_json::Value;

use super::domain::{ItemKind, Page, PageKind, SearchItem, Shelf, Track};

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

/// Largest thumbnail URL nested anywhere inside `v`.
pub fn largest_thumbnail(v: &Value) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for tr in find_renderers(v, "musicThumbnailRenderer") {
        if let Some(thumbs) = tr
            .get("thumbnail")
            .and_then(|t| t.get("thumbnails"))
            .and_then(Value::as_array)
        {
            for t in thumbs {
                let url = t.get("url").and_then(Value::as_str).unwrap_or_default();
                let w = t.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                if url.is_empty() {
                    continue;
                }
                if best.as_ref().map(|(bw, _)| w > *bw).unwrap_or(true) {
                    best = Some((w, url.to_string()));
                }
            }
        }
    }
    best.map(|(_, u)| u)
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
            let (_, page_type, watch_id) = r
                .get("navigationEndpoint")
                .map(endpoint_ids)
                .unwrap_or((None, None, None));
            out.push(Run {
                text: r.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
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
        album: album_from_runs(&subtitle_runs),
        duration_secs: duration_from(item, &cols),
        thumbnail: largest_thumbnail(item),
    })
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
        "Album" | "Single" | "EP" => ItemKind::Album,
        "Artist" => ItemKind::Artist,
        "Playlist" | "Community playlist" => ItemKind::Playlist,
        "Podcast" => ItemKind::Podcast,
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
        kind,
        browse_id,
        video_id,
        thumbnail: largest_thumbnail(item),
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
        kind,
        browse_id,
        video_id: watch_id,
        thumbnail: largest_thumbnail(item),
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
        let items: Vec<SearchItem> = find_renderers(carousel, "musicTwoRowItemRenderer")
            .into_iter()
            .filter_map(parse_two_row_item)
            .collect();
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
    let header = HEADER_NAMES
        .iter()
        .find_map(|name| find_first(v, name));

    let mut title = String::new();
    let mut subtitle = String::new();
    let mut description = String::new();
    let mut thumbnail = None;
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
        thumbnail = largest_thumbnail(h);
    }

    let tracks: Vec<Track> = find_renderers(v, "musicResponsiveListItemRenderer")
        .into_iter()
        .filter_map(parse_track)
        .collect();

    let shelves = parse_shelves(v);

    Page {
        kind: page_kind(browse_id),
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
    }
}
