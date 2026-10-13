//! Minimal client for YouTube Music's internal ("InnerTube") API.
//!
//! YouTube Music has no public API. Like other third-party clients we POST to
//! the same `music.youtube.com/youtubei/v1/*` endpoints the web app uses, with
//! a spoofed `WEB_REMIX` client context.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER,
};
use serde_json::{Value, json};

use crate::auth::CookieJar;

use super::domain::{LikeStatus, Lyrics, Page, SearchItem, Shelf};
use super::extract;

const ENDPOINT_BASE: &str = "https://music.youtube.com/youtubei/v1";
const API_ORIGIN: &str = "https://music.youtube.com";

/// Spoofed web client identity. Bump if responses start coming back empty.
const CLIENT_NAME: &str = "WEB_REMIX";
const CLIENT_VERSION: &str = "1.20250630.00.00";
const CLIENT_NAME_HEADER: &str = "67";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Search filter, mapped to YouTube's opaque protobuf `params` tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchFilter {
    All,
    Songs,
    Videos,
    Albums,
    Artists,
    Playlists,
}

impl SearchFilter {
    pub const ALL_FILTERS: [SearchFilter; 6] = [
        SearchFilter::All,
        SearchFilter::Songs,
        SearchFilter::Videos,
        SearchFilter::Albums,
        SearchFilter::Artists,
        SearchFilter::Playlists,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SearchFilter::All => "All",
            SearchFilter::Songs => "Songs",
            SearchFilter::Videos => "Videos",
            SearchFilter::Albums => "Albums",
            SearchFilter::Artists => "Artists",
            SearchFilter::Playlists => "Playlists",
        }
    }

    /// Whether this category reads better as rows than as cover tiles.
    pub fn has_row_layout(self) -> bool {
        matches!(self, SearchFilter::Songs | SearchFilter::Videos)
    }

    fn param(self) -> Option<&'static str> {
        match self {
            SearchFilter::All => None,
            SearchFilter::Songs => Some("EgWKAQIIAWoSEAUQCRADEAoQBBAQEBUQDhAR"),
            SearchFilter::Videos => Some("EgWKAQIQAWoSEAUQCRADEAoQBBAQEBUQDhAR"),
            SearchFilter::Albums => Some("EgWKAQIYAWoSEAUQCRADEAoQBBAQEBUQDhAR"),
            SearchFilter::Artists => Some("EgWKAQIgAWoSEAUQCRADEAoQBBAQEBUQDhAR"),
            SearchFilter::Playlists => Some("EgeKAQQoADgBahIQBRAJEAMQChAEEBAQFRAOEBE="),
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    headers: HeaderMap,
    lang: String,
    country: String,
    jar: Option<CookieJar>,
    /// Set once the server has rejected our locale and English worked instead.
    ///
    /// The locale comes from the environment, so it can name a language YouTube
    /// does not know — `eo` and `xx` are two letters and still invalid. That is
    /// rejected outright rather than ignored, which would take search and
    /// browsing down with it.
    locale_fallback: AtomicBool,
}

impl Client {
    pub fn new(lang: &str, country: &str, jar: Option<CookieJar>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .build()
            .context("building HTTP client")?;

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://music.youtube.com"),
        );
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://music.youtube.com/"),
        );
        headers.insert("X-Goog-Api-Format-Version", HeaderValue::from_static("1"));
        headers.insert(
            "X-YouTube-Client-Name",
            HeaderValue::from_static(CLIENT_NAME_HEADER),
        );
        headers.insert(
            "X-YouTube-Client-Version",
            HeaderValue::from_static(CLIENT_VERSION),
        );

        Ok(Self {
            http,
            headers,
            lang: lang.to_string(),
            country: country.to_string(),
            jar,
            locale_fallback: AtomicBool::new(false),
        })
    }

    /// Whether requests carry an authenticated session.
    pub fn is_authenticated(&self) -> bool {
        self.jar.as_ref().map(|j| j.has_sapisid()).unwrap_or(false)
    }

    /// Ask InnerTube whether our cookies are recognised as a signed-in user.
    pub async fn logged_in(&self) -> Result<bool> {
        let value = self
            .post("browse", json!({ "browseId": "FEmusic_library_landing" }))
            .await?;
        let logged = value
            .get("responseContext")
            .and_then(|c| c.get("serviceTrackingParams"))
            .and_then(Value::as_array)
            .map(|tracking| {
                tracking.iter().any(|entry| {
                    entry
                        .get("params")
                        .and_then(Value::as_array)
                        .map(|params| {
                            params.iter().any(|p| {
                                p.get("key").and_then(Value::as_str) == Some("logged_in")
                                    && p.get("value").and_then(Value::as_str) == Some("1")
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        Ok(logged)
    }

    fn context(&self) -> Value {
        let (lang, country) = if self.locale_fallback.load(Ordering::Relaxed) {
            ("en".to_string(), "US".to_string())
        } else {
            (self.lang.clone(), self.country.clone())
        };
        json!({
            "client": {
                "clientName": CLIENT_NAME,
                "clientVersion": CLIENT_VERSION,
                "hl": lang,
                "gl": country,
            },
            "user": { "lockedSafetyMode": false },
            "request": { "useSsl": true },
        })
    }

    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        if let Some(jar) = &self.jar {
            let cookie = jar.cookie_header();
            if !cookie.is_empty()
                && let Ok(v) = HeaderValue::from_str(&cookie)
            {
                headers.insert(COOKIE, v);
            }
            // SAPISIDHASH is time-stamped, so it must be recomputed per request.
            if let Some(auth) = jar.auth_header(API_ORIGIN)
                && let Ok(v) = HeaderValue::from_str(&auth)
            {
                headers.insert(AUTHORIZATION, v);
            }
            headers.insert("X-Goog-AuthUser", HeaderValue::from_static("0"));
            headers.insert("X-Origin", HeaderValue::from_static(API_ORIGIN));
        }
        headers
    }

    async fn post(&self, endpoint: &str, mut body: Value) -> Result<Value> {
        body["context"] = self.context();
        let url = format!("{ENDPOINT_BASE}/{endpoint}?prettyPrint=false");
        let resp = self
            .http
            .post(&url)
            .headers(self.request_headers())
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {endpoint}"))?;

        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // A locale YouTube doesn't recognise is refused rather than
            // ignored, and it came from the environment rather than from us.
            // English is always accepted, so try that once — and keep it only
            // if it actually works, so a genuine bad request doesn't quietly
            // change the user's language for the rest of the session.
            if status == reqwest::StatusCode::BAD_REQUEST
                && self.lang != "en"
                && !self.locale_fallback.load(Ordering::Relaxed)
            {
                self.locale_fallback.store(true, Ordering::Relaxed);
                let mut retry = body.clone();
                retry["context"] = self.context();
                return match Box::pin(self.post(endpoint, retry)).await {
                    Ok(value) => Ok(value),
                    Err(e) => {
                        self.locale_fallback.store(false, Ordering::Relaxed);
                        Err(e)
                    }
                };
            }
            bail!(
                "InnerTube {endpoint} failed ({status}): {}",
                inner_tube_reason(&body_text)
            );
        }
        // The like endpoints reply with an empty body, so an absent payload is
        // not an error.
        if body_text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body_text)
            .with_context(|| format!("decoding {endpoint} JSON response"))
    }

    /// A track's lyrics, if it has any.
    ///
    /// Two requests: the watch-next response lists the tabs, and the lyrics one
    /// is a browse endpoint rather than the words themselves. A track without
    /// lyrics has no such tab, which is the common case for instrumentals.
    pub async fn lyrics(&self, video_id: &str) -> Result<Option<Lyrics>> {
        let player = self.post("next", json!({ "videoId": video_id })).await?;
        let Some(browse_id) = extract::lyrics_browse_id(&player) else {
            return Ok(None);
        };
        let page = self
            .post("browse", json!({ "browseId": browse_id }))
            .await?;
        Ok(extract::parse_lyrics(&page).map(|text| Lyrics {
            text,
            source: extract::lyrics_source(&page),
        }))
    }

    /// How the signed-in listener rated a track, and whether rating is allowed.
    ///
    /// The watch-next response is where the player overlays carry this, so it
    /// costs one request when the playing track changes.
    pub async fn like_status(&self, video_id: &str) -> Result<(LikeStatus, bool)> {
        let value = self.post("next", json!({ "videoId": video_id })).await?;
        Ok(extract::parse_like_status(&value))
    }

    /// Create a playlist, returning its id.
    ///
    /// Private, which is what YouTube Music's own "New playlist" creates.
    pub async fn create_playlist(&self, title: &str) -> Result<String> {
        let value = self
            .post(
                "playlist/create",
                json!({ "title": title, "privacyStatus": "PRIVATE" }),
            )
            .await?;
        value
            .get("playlistId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("playlist/create returned no playlistId")
    }

    /// Add a track to a playlist.
    pub async fn add_to_playlist(&self, playlist_id: &str, video_id: &str) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id }]),
        )
        .await
    }

    /// Remove a track from a playlist.
    ///
    /// Needs the entry's own id as well as the video id: the same track can sit
    /// in a playlist twice, and the server rejects the action without knowing
    /// which entry is meant.
    pub async fn remove_from_playlist(
        &self,
        playlist_id: &str,
        video_id: &str,
        set_video_id: &str,
    ) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{
                "action": "ACTION_REMOVE_VIDEO",
                "removedVideoId": video_id,
                "setVideoId": set_video_id,
            }]),
        )
        .await
    }

    /// Rename a playlist.
    pub async fn rename_playlist(&self, playlist_id: &str, title: &str) -> Result<()> {
        self.edit_playlist(
            playlist_id,
            json!([{ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": title }]),
        )
        .await
    }

    /// Delete a playlist outright. The server refuses unless you own it.
    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.post(
            "playlist/delete",
            json!({ "playlistId": bare_playlist_id(playlist_id) }),
        )
        .await?;
        Ok(())
    }

    /// Apply edits to a playlist. Every mutation goes through here.
    async fn edit_playlist(&self, playlist_id: &str, actions: Value) -> Result<()> {
        self.post(
            "browse/edit_playlist",
            json!({ "playlistId": bare_playlist_id(playlist_id), "actions": actions }),
        )
        .await?;
        Ok(())
    }

    /// Set or clear the rating for a track.
    pub async fn set_like(&self, video_id: &str, status: LikeStatus) -> Result<()> {
        let endpoint = match status {
            LikeStatus::Like => "like/like",
            LikeStatus::Dislike => "like/dislike",
            LikeStatus::Indifferent => "like/removelike",
        };
        self.post(endpoint, json!({ "target": { "videoId": video_id } }))
            .await?;
        Ok(())
    }

    /// Search the catalogue. Results are de-duplicated and classified.
    ///
    /// Returns the token for fetching more, when the response offers one —
    /// search is paged the same way feeds are.
    pub async fn search(
        &self,
        query: &str,
        filter: SearchFilter,
    ) -> Result<(Vec<SearchItem>, Option<String>)> {
        let mut body = json!({ "query": query });
        if let Some(params) = filter.param() {
            body["params"] = Value::String(params.to_string());
        }
        let value = self.post("search", body).await?;
        Ok((
            extract::parse_search_items(&value),
            extract::parse_any_continuation(&value),
        ))
    }

    /// Fetch the next page of a search.
    pub async fn search_more(&self, token: &str) -> Result<(Vec<SearchItem>, Option<String>)> {
        let value = self
            .post("search", json!({ "continuation": token }))
            .await?;
        Ok((
            extract::parse_search_items(&value),
            extract::parse_any_continuation(&value),
        ))
    }

    /// Load a detail page by browse id (`MPREb…` album, `VL…` playlist, `UC…` artist).
    pub async fn browse(&self, browse_id: &str) -> Result<Page> {
        let body = json!({ "browseId": browse_id });
        let value = self.post("browse", body).await?;
        Ok(extract::parse_page(browse_id, &value))
    }

    /// Page in further shelves (the home/explore feeds load this way).
    pub async fn browse_continuation(&self, token: &str) -> Result<MoreShelves> {
        let body = json!({ "continuation": token });
        let value = self.post("browse", body).await?;
        Ok(MoreShelves {
            shelves: extract::parse_shelves(&value),
            continuation: extract::parse_continuation(&value),
        })
    }

    /// The account's listening history, most recent first.
    pub async fn history(&self) -> Result<Vec<super::domain::Track>> {
        Ok(self.browse("FEmusic_history").await?.tracks)
    }

    /// The rest of a long playlist, via the player panel.
    ///
    /// `browse` caps a playlist at 100 tracks. The `next` endpoint returns a
    /// ~100-track window *around* an anchor video, so we anchor on the last
    /// track we know and keep whatever is new.
    pub async fn playlist_more(
        &self,
        anchor_video_id: &str,
        playlist_id: &str,
    ) -> Result<Vec<super::domain::Track>> {
        let body = json!({
            "videoId": anchor_video_id,
            "playlistId": playlist_id,
            "playlistPanelOffset": 0,
            "isAudioOnly": true,
        });
        let value = self.post("next", body).await?;
        Ok(extract::parse_panel_tracks(&value))
    }
}

/// Extra shelves returned by a continuation request.
pub struct MoreShelves {
    pub shelves: Vec<Shelf>,
    pub continuation: Option<String>,
}

/// Pull the human-readable reason out of an InnerTube error body.
///
/// The body is JSON like `{"error": {"code": 400, "message": "Request contains
/// an invalid argument.", ...}}`, and the whole envelope is noise in a status
/// bar — as anyone who has seen one will agree. Anything unparseable falls back
/// to a truncated body, so the message is never empty.
fn inner_tube_reason(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.trim().chars().take(120).collect());

    // A rejection for permission reasons is almost always a stale session, and
    // saying so is more useful than repeating the server's wording.
    let lower = message.to_lowercase();
    if lower.contains("permission") || lower.contains("unauthorized") {
        return format!("{message} — sign in again from Settings");
    }
    if message.is_empty() {
        return "no reason given".to_string();
    }
    message
}

/// Playlists arrive as browse ids (`VLPL…`) but the editing endpoints take the
/// bare id (`PL…`). Sending the wrong one is a silent no-op, so it happens in
/// one place and is tested.
fn bare_playlist_id(id: &str) -> &str {
    id.strip_prefix("VL").unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::bare_playlist_id;

    #[test]
    fn playlist_ids_lose_only_their_browse_prefix() {
        assert_eq!(bare_playlist_id("VLPLabc123"), "PLabc123");
        // Already bare: unchanged.
        assert_eq!(bare_playlist_id("PLabc123"), "PLabc123");
        // A "VL" inside the id is part of the id.
        assert_eq!(bare_playlist_id("VLPLaVLb"), "PLaVLb");
        // Album browse ids are not playlists and must not be mangled.
        assert_eq!(bare_playlist_id("MPREb_x"), "MPREb_x");
    }
}

#[cfg(test)]
mod error_tests {
    use super::inner_tube_reason;

    /// The real body that broke the canary, verbatim enough to matter.
    #[test]
    fn pulls_the_reason_out_of_the_envelope() {
        let body = r#"{
            "error": {
                "code": 400,
                "message": "Request contains an invalid argument.",
                "errors": [{ "message": "Request contains an invalid argument.", "domain": "global" }]
            }
        }"#;
        assert_eq!(
            inner_tube_reason(body),
            "Request contains an invalid argument."
        );
    }

    /// A permission failure usually means a stale session, so say what to do.
    #[test]
    fn suggests_signing_in_for_permission_failures() {
        let body = r#"{"error": {"code": 403, "message": "The caller does not have permission"}}"#;
        let reason = inner_tube_reason(body);
        assert!(reason.contains("permission"), "{reason}");
        assert!(reason.contains("sign in"), "{reason}");
    }

    /// Never return an empty message, whatever the server sends.
    #[test]
    fn copes_with_bodies_that_are_not_json() {
        assert_eq!(inner_tube_reason("<html>nope</html>"), "<html>nope</html>");
        assert_eq!(inner_tube_reason(""), "no reason given");
        assert!(!inner_tube_reason("{}").is_empty());
    }
}
