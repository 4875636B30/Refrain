//! Authentication: getting cookies out of a browser (or a file) and turning
//! them into the credentials YouTube's API expects.
//!
//! Rather than re-implementing Chrome/Firefox cookie decryption, we reuse
//! `yt-dlp --cookies-from-browser` to materialise a Netscape `cookies.txt`,
//! which we then parse. The same cookies serve two purposes:
//!
//! * playback — passed straight back to `yt-dlp` to get past the bot check, and
//! * the catalogue — sent to InnerTube with a `SAPISIDHASH` `Authorization`
//!   header so the user's library becomes visible.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use tokio::process::Command;

use crate::config::{
    self, AuthConfig, AuthMode, Config,
};

/// A single HTTP cookie.
#[derive(Clone, Debug)]
pub struct Cookie {
    pub domain: String,
    pub name: String,
    pub value: String,
}

/// A set of cookies plus the derived `SAPISID` used for request signing.
#[derive(Clone, Debug)]
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
    sapisid: Option<String>,
}

impl CookieJar {
    pub fn new(cookies: Vec<Cookie>) -> Self {
        let sapisid = pick_sapisid(&cookies);
        Self { cookies, sapisid }
    }

    /// Build the `Cookie` header.
    ///
    /// Google sets the same cookie name on `.youtube.com`, `.google.com` and
    /// regional domains like `.google.at`. Sending the name more than once makes
    /// the API authenticate against the wrong scope, so we keep the most
    /// specific one per name: youtube.com > google.com > google.* .
    pub fn cookie_header(&self) -> String {
        fn score(domain: &str) -> u8 {
            let d = domain.trim_start_matches('.');
            if d.ends_with("youtube.com") {
                0
            } else if d == "google.com" {
                1
            } else if d.starts_with("google.") {
                2
            } else {
                3
            }
        }

        let mut candidates: Vec<(&Cookie, u8)> = self
            .cookies
            .iter()
            // Only send Google/YouTube cookies: keeps the header small and
            // avoids leaking unrelated site cookies to the API.
            .filter(|c| c.domain.contains("google") || c.domain.contains("youtube"))
            .map(|c| (c, score(&c.domain)))
            .collect();
        candidates.sort_by_key(|(_, rank)| *rank);

        let mut seen = std::collections::HashSet::new();
        let mut parts = Vec::new();
        for (cookie, _) in candidates {
            if seen.insert(cookie.name.as_str()) {
                parts.push(format!("{}={}", cookie.name, cookie.value));
            }
        }
        parts.join("; ")
    }

    /// `Authorization: SAPISIDHASH <ts>_<sha1(ts + " " + sapisid + " " + origin)>`.
    pub fn auth_header(&self, origin: &str) -> Option<String> {
        let sapisid = self.sapisid.as_ref()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let mut hasher = Sha1::new();
        hasher.update(format!("{ts} {sapisid} {origin}").as_bytes());
        let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Some(format!("SAPISIDHASH {ts}_{hex}"))
    }

    pub fn has_sapisid(&self) -> bool {
        self.sapisid.is_some()
    }

    /// `LOGIN_INFO` is only set once a real YouTube session exists.
    pub fn has_youtube_session(&self) -> bool {
        self.cookies.iter().any(|c| c.name == "LOGIN_INFO")
    }
}

/// The `SAPISID` used for request signing, preferring the youtube.com scope.
fn pick_sapisid(cookies: &[Cookie]) -> Option<String> {
    for name in ["__Secure-3PAPISID", "SAPISID"] {
        for domain in ["youtube.com", "google.com"] {
            if let Some(cookie) = cookies
                .iter()
                .find(|c| c.name == name && c.domain.contains(domain))
            {
                return Some(cookie.value.clone());
            }
        }
    }
    cookies
        .iter()
        .find(|c| c.name == "SAPISID")
        .map(|c| c.value.clone())
}

/// Where the materialised cookie jar is cached.
pub fn cookies_file_path() -> PathBuf {
    config::cache_dir().join("cookies.txt")
}

/// Where we remember which browser actually worked, so later launches don't
/// have to probe every browser again.
fn source_cache_path() -> PathBuf {
    config::cache_dir().join("auth-source.txt")
}

pub fn cached_source() -> Option<AuthConfig> {
    let text = std::fs::read_to_string(source_cache_path()).ok()?;
    let mut lines = text.lines();
    let browser = lines.next()?.trim().to_string();
    let profile = lines.next().unwrap_or("").trim().to_string();
    let keyring = lines.next().unwrap_or("").trim().to_string();
    if browser.is_empty() {
        return None;
    }
    // Re-verify the profile still exists, so switching browsers self-heals.
    if !profile.is_empty() && !Path::new(&profile).exists() {
        return None;
    }
    Some(AuthConfig {
        mode: AuthMode::Browser,
        browser,
        keyring,
        profile,
        cookies_file: String::new(),
    })
}

fn store_source(auth: &AuthConfig) {
    let path = source_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!("{}\n{}\n{}\n", auth.browser, auth.profile, auth.keyring),
    );
}

/// Parse a Netscape-format `cookies.txt`, honouring the `#HttpOnly_` prefix.
pub fn parse_netscape(text: &str) -> Vec<Cookie> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = raw;
        if let Some(rest) = line.strip_prefix("#HttpOnly_") {
            line = rest;
        } else if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 {
            out.push(Cookie {
                domain: fields[0].to_string(),
                name: fields[5].to_string(),
                value: fields[6].to_string(),
            });
        }
    }
    out
}

/// Extract cookies from the configured source by driving `yt-dlp`.
pub async fn materialize(auth: &AuthConfig) -> Result<Vec<Cookie>> {
    let source_args = auth.ytdlp_args();
    if source_args.is_empty() {
        bail!("no cookie source configured");
    }
    let ytdlp = crate::audio::resolver::locate_ytdlp()
        .context("yt-dlp not found, so browser cookies cannot be extracted")?;

    let out = cookies_file_path();
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&out);

    let mut args: Vec<String> = Vec::new();
    args.extend(crate::audio::resolver::js_runtime_args());
    args.extend(source_args);
    args.extend([
        "--cookies".to_string(),
        out.to_string_lossy().into_owned(),
        // A lightweight, reliably-available video: we only need the run to
        // finish so yt-dlp dumps the jar.
        "--simulate".to_string(),
        "--skip-download".to_string(),
        "--no-warnings".to_string(),
        "--no-playlist".to_string(),
        "--print".to_string(),
        "%(id)s".to_string(),
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
    ]);

    let _ = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new(&ytdlp).args(&args).output(),
    )
    .await
    .context("yt-dlp timed out while extracting cookies")?;

    // yt-dlp can exit non-zero (e.g. an unrelated warning) yet still have
    // written the jar, so we judge success by the file, not the exit code.
    let text = std::fs::read_to_string(&out).with_context(|| {
        format!(
            "yt-dlp did not write {}. Check the browser name/profile path.",
            out.display()
        )
    })?;
    let cookies = parse_netscape(&text);
    if cookies.is_empty() {
        bail!("cookie file was produced but contained no cookies");
    }
    Ok(cookies)
}

/// A resolved cookie source together with its parsed jar.
#[derive(Clone, Debug)]
pub struct ResolvedAuth {
    /// The concrete source (browser + profile, or file) that actually worked —
    /// this is what must be handed to `yt-dlp` for playback.
    pub source: AuthConfig,
    pub jar: CookieJar,
}

fn is_chromium(browser: &str) -> bool {
    matches!(
        browser,
        "chrome" | "chromium" | "brave" | "edge" | "opera" | "vivaldi" | "whale"
    )
}

/// Chromium browsers encrypt cookies with a key held in the OS keyring. yt-dlp
/// guesses which keyring from the desktop environment, which is unreliable
/// (e.g. when launched without `XDG_CURRENT_DESKTOP`) — and guessing
/// `BASICTEXT` silently fails to decrypt most cookies. So we try them
/// explicitly and remember whichever works.
fn keyring_order() -> Vec<&'static str> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_uppercase();
    if desktop.contains("KDE") {
        vec!["KWALLET6", "KWALLET", "GNOMEKEYRING", "BASICTEXT"]
    } else {
        vec!["GNOMEKEYRING", "KWALLET6", "KWALLET", "BASICTEXT"]
    }
}

/// The concrete sources to try for one browser: itself if a keyring is already
/// set (or it isn't a Chromium browser), otherwise each keyring option.
fn keyring_variants(source: &AuthConfig) -> Vec<AuthConfig> {
    if !source.keyring.is_empty() || !is_chromium(&source.browser) {
        return vec![source.clone()];
    }
    keyring_order()
        .into_iter()
        .map(|keyring| AuthConfig {
            keyring: keyring.to_string(),
            ..source.clone()
        })
        .collect()
}

/// Try a browser across its keyring options, preferring one that yields a
/// usable `SAPISID`.
async fn try_source(source: &AuthConfig) -> Option<ResolvedAuth> {
    let mut fallback: Option<ResolvedAuth> = None;
    for variant in keyring_variants(source) {
        let Ok(cookies) = materialize(&variant).await else {
            continue;
        };
        let jar = CookieJar::new(cookies);
        if jar.has_sapisid() {
            return Some(ResolvedAuth {
                source: variant,
                jar,
            });
        }
        if fallback.is_none() {
            fallback = Some(ResolvedAuth {
                source: variant,
                jar,
            });
        }
    }
    fallback
}

/// How long a dumped cookie jar is reused before re-extracting from the browser.
/// Extraction costs ~10s of `yt-dlp`, so this is the difference between an
/// instant start and a slow one.
const COOKIE_TTL: std::time::Duration = std::time::Duration::from_secs(45 * 60);

/// Recently dumped cookies, if any, avoiding a slow re-extraction.
fn cached_cookies() -> Option<Vec<Cookie>> {
    let path = cookies_file_path();
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    if modified.elapsed().ok()? > COOKIE_TTL {
        return None;
    }
    let cookies = parse_netscape(&std::fs::read_to_string(&path).ok()?);
    (!cookies.is_empty()).then_some(cookies)
}

/// Drop the cached cookie jar (and remembered source), forcing re-extraction.
pub fn clear_cache() {
    let _ = std::fs::remove_file(cookies_file_path());
    let _ = std::fs::remove_file(source_cache_path());
}

/// Resolve cookies according to the config, reusing a recent extraction.
pub async fn resolve(config: &Config) -> Result<Option<ResolvedAuth>> {
    // Fast path: a fresh jar plus a remembered source means no yt-dlp at all.
    if !matches!(config.auth.mode, AuthMode::None | AuthMode::File)
        && let (Some(source), Some(cookies)) = (cached_source(), cached_cookies())
    {
        return Ok(Some(ResolvedAuth {
            source,
            jar: CookieJar::new(cookies),
        }));
    }
    resolve_uncached(config).await
}

/// Resolve ignoring any cached jar (used when the cached one stops working).
pub async fn resolve_fresh(config: &Config) -> Result<Option<ResolvedAuth>> {
    let _ = std::fs::remove_file(cookies_file_path());
    resolve_uncached(config).await
}

async fn resolve_uncached(config: &Config) -> Result<Option<ResolvedAuth>> {
    match config.auth.mode {
        AuthMode::None => Ok(None),
        AuthMode::File => {
            let cookies = materialize(&config.auth).await?;
            Ok(Some(ResolvedAuth {
                source: config.auth.clone(),
                jar: CookieJar::new(cookies),
            }))
        }
        AuthMode::Browser => {
            let configured = try_source(&config.auth).await;
            if let Some(found) = &configured {
                if found.jar.has_youtube_session() {
                    store_source(&found.source);
                    return Ok(configured);
                }
            }
            match auto_resolve().await? {
                Some(better) => Ok(Some(better)),
                None => Ok(configured),
            }
        }
        AuthMode::Auto => auto_resolve().await,
    }
}

/// Try each detected browser, preferring one with a real YouTube session.
///
/// A previously-successful browser is tried first so subsequent launches are
/// fast.
pub async fn auto_resolve() -> Result<Option<ResolvedAuth>> {
    let cached = cached_source();
    let mut ordered: Vec<(String, PathBuf)> = Vec::new();
    if let Some(source) = &cached {
        ordered.push((source.browser.clone(), PathBuf::from(&source.profile)));
    }
    for candidate in candidates() {
        if !ordered
            .iter()
            .any(|(b, p)| b == &candidate.0 && p == &candidate.1)
        {
            ordered.push(candidate);
        }
    }

    let mut fallback: Option<ResolvedAuth> = None;
    for (browser, profile) in ordered.into_iter().take(6) {
        // Carry over a known-good keyring for the cached browser.
        let keyring = cached
            .as_ref()
            .filter(|c| c.browser == browser)
            .map(|c| c.keyring.clone())
            .unwrap_or_default();
        let source = AuthConfig {
            mode: AuthMode::Browser,
            browser,
            keyring,
            profile: profile.to_string_lossy().into_owned(),
            cookies_file: String::new(),
        };
        let Some(found) = try_source(&source).await else {
            continue;
        };
        let has_session = found.jar.has_youtube_session();
        if fallback.is_none() {
            fallback = Some(found.clone());
        }
        if has_session {
            store_source(&found.source);
            return Ok(Some(found));
        }
    }
    Ok(fallback)
}

// --------------------------------------------------------------- discovery

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Candidate `(browser, user_data_or_profile_dir)` pairs to try, in order.
fn candidates() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();

    // Firefox: profiles live under an XDG path on modern builds.
    for base in [
        home().join(".config/mozilla/firefox"),
        home().join(".mozilla/firefox"),
        home().join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ] {
        if let Some(profile) = firefox_default_profile(&base) {
            out.push(("firefox".to_string(), profile));
        }
    }

    // Chromium family: value is the user-data directory.
    let chromium = [
        ("brave", ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
        ("brave", ".config/BraveSoftware/Brave-Browser"),
        ("chrome", ".var/app/com.google.Chrome/config/google-chrome"),
        ("chrome", ".config/google-chrome"),
        ("chromium", ".var/app/org.chromium.Chromium/config/chromium"),
        ("chromium", ".config/chromium"),
        ("edge", ".config/microsoft-edge"),
        ("vivaldi", ".config/vivaldi"),
        ("opera", ".config/opera"),
    ];
    for (browser, rel) in chromium {
        let dir = home().join(rel);
        if dir.join("Default/Cookies").is_file() || dir.join("Default/Network/Cookies").is_file() {
            out.push((browser.to_string(), dir));
        }
    }
    out
}

/// Read `profiles.ini` and return the default profile's absolute path.
fn firefox_default_profile(base: &std::path::Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(base.join("profiles.ini")).ok()?;
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut current = String::new();
    let mut kvs: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if !kvs.is_empty() {
                sections.push((std::mem::take(&mut current), std::mem::take(&mut kvs)));
            }
            current = line.trim_matches(['[', ']']).to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            kvs.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    if !kvs.is_empty() {
        sections.push((current, kvs));
    }

    let find = |sec: &(String, Vec<(String, String)>), key: &str| -> Option<String> {
        sec.1.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };

    // Honour [Install...] Default first, then any profile marked Default=1,
    // then just the first profile.
    let install_default = sections
        .iter()
        .filter(|(name, _)| name.starts_with("Install"))
        .find_map(|sec| find(sec, "Default"));
    let profiles: Vec<&(String, Vec<(String, String)>)> = sections
        .iter()
        .filter(|(name, _)| name.starts_with("Profile"))
        .collect();

    let chosen = install_default
        .or_else(|| {
            profiles
                .iter()
                .find(|sec| find(sec, "Default").as_deref() == Some("1"))
                .and_then(|sec| find(sec, "Path"))
        })
        .or_else(|| profiles.first().and_then(|sec| find(sec, "Path")))?;

    let path = base.join(chosen);
    path.is_dir().then_some(path)
}

/// Human-readable list of detected cookie sources, for diagnostics.
pub fn detected_sources() -> Vec<String> {
    candidates()
        .into_iter()
        .map(|(browser, path)| format!("{browser}  ({})", path.display()))
        .collect()
}
