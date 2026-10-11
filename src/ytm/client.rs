//! Minimal client for YouTube Music's internal ("InnerTube") API.
//!
//! YouTube Music has no public API. Like other third-party clients we POST to
//! the same `music.youtube.com/youtubei/v1/*` endpoints the web app uses, with
//! a spoofed `WEB_REMIX` client context.

use anyhow::{Context, Result, bail};
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER,
};
use serde_json::{Value, json};

use crate::auth::CookieJar;

use super::domain::{Page, SearchItem, Shelf};
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
        json!({
            "client": {
                "clientName": CLIENT_NAME,
                "clientVersion": CLIENT_VERSION,
                "hl": self.lang,
                "gl": self.country,
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
            bail!(
                "InnerTube {endpoint} returned HTTP {status}: {}",
                body_text.chars().take(200).collect::<String>()
            );
        }
        serde_json::from_str(&body_text)
            .with_context(|| format!("decoding {endpoint} JSON response"))
    }

    /// Search the catalogue. Results are de-duplicated and classified.
    pub async fn search(&self, query: &str, filter: SearchFilter) -> Result<Vec<SearchItem>> {
        let mut body = json!({ "query": query });
        if let Some(params) = filter.param() {
            body["params"] = Value::String(params.to_string());
        }
        let value = self.post("search", body).await?;
        Ok(extract::parse_search_items(&value))
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
