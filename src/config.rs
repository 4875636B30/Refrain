//! Persistent configuration (`~/.config/refrain/config.toml`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "refrain")
}

/// One-time move of settings and cache from the project's previous name, so the
/// rename doesn't lose anyone's config, cookies or play counts.
fn migrate_legacy_dirs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(legacy) = directories::ProjectDirs::from("", "", "youtube-music-rs") else {
            return;
        };
        for (old, new) in [
            (legacy.config_dir().to_path_buf(), config_dir_raw()),
            (legacy.cache_dir().to_path_buf(), cache_dir_raw()),
        ] {
            if !new.exists() && old.exists() {
                let _ = std::fs::rename(&old, &new);
            }
        }
    });
}

/// Split a POSIX locale string into InnerTube's `hl` and `gl`.
///
/// Both are validated, because passing a non-language straight through is not a
/// harmless degradation: `LANG=C` yields `hl=C`, and YouTube answers that with
/// HTTP 400 "Request contains an invalid argument" — which breaks search and
/// browsing completely for anyone running in a minimal environment. `C` and
/// `POSIX` are exactly the locales that have no language code to send.
fn parse_locale(raw: &str) -> (String, String) {
    let mut parts = raw.split(['_', '.', '@']);
    let language = parts.next().unwrap_or("").trim();
    let region = parts.next().unwrap_or("").trim();

    // ISO 639-1 is two letters, and a few languages use three (`fil`).
    let language =
        if (2..=3).contains(&language.len()) && language.chars().all(|c| c.is_ascii_alphabetic()) {
            language.to_lowercase()
        } else {
            "en".to_string()
        };
    // ISO 3166-1 alpha-2.
    let region = if region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()) {
        region.to_uppercase()
    } else {
        "US".to_string()
    };
    (language, region)
}

fn config_dir_raw() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn cache_dir_raw() -> PathBuf {
    project_dirs()
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

pub fn config_dir() -> PathBuf {
    migrate_legacy_dirs();
    config_dir_raw()
}

pub fn cache_dir() -> PathBuf {
    migrate_legacy_dirs();
    cache_dir_raw()
}

impl Config {
    /// The system language and region, used for InnerTube's `hl`/`gl`. Falls
    /// back to `en`/`US` when the locale doesn't specify them.
    pub fn locale() -> (String, String) {
        let raw = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_MESSAGES"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        parse_locale(&raw)
    }
}

/// The browsers `yt-dlp --cookies-from-browser` understands, offered in the
/// settings window so nobody has to spell one correctly by hand.
pub const KNOWN_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "brave", "edge", "opera", "vivaldi", "whale",
];

/// Chromium-family keyrings to try, on Linux. Empty means "try each in turn",
/// which is what the client does by default — and what it has to do, since
/// yt-dlp's own guess lands on `BASICTEXT` and silently fails to decrypt.
pub const KNOWN_KEYRINGS: [&str; 5] = ["", "GNOMEKEYRING", "KWALLET6", "KWALLET", "BASICTEXT"];

impl AuthMode {
    pub const ALL: [AuthMode; 4] = [
        AuthMode::Auto,
        AuthMode::Browser,
        AuthMode::File,
        AuthMode::None,
    ];

    /// How it reads in the settings window.
    pub fn label(self) -> &'static str {
        match self {
            AuthMode::Auto => "Detect automatically",
            AuthMode::Browser => "From a browser",
            AuthMode::File => "From a cookies file",
            AuthMode::None => "Never send cookies",
        }
    }
}

/// How a keyring choice reads in the settings window.
pub fn keyring_label(keyring: &str) -> &str {
    match keyring {
        "" => "Try each in turn",
        "GNOMEKEYRING" => "GNOME Keyring",
        "KWALLET6" | "KWALLET" => "KWallet",
        "BASICTEXT" => "Plain text",
        other => other,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auth: AuthConfig,
    pub ui: UiConfig,
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Load the config, falling back to defaults (and env vars) on any problem.
    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        cfg.apply_env();
        cfg
    }

    #[allow(dead_code)] // used by the in-app settings panel (in progress)
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// `YTDLP_COOKIES` / `YTDLP_COOKIES_FROM_BROWSER` override, applied only when
    /// the config file didn't specify a source.
    fn apply_env(&mut self) {
        if self.auth.configured() {
            return;
        }
        if let Ok(file) = std::env::var("YTDLP_COOKIES")
            && !file.trim().is_empty()
        {
            self.auth.mode = AuthMode::File;
            self.auth.cookies_file = file;
            return;
        }
        if let Ok(browser) = std::env::var("YTDLP_COOKIES_FROM_BROWSER")
            && !browser.trim().is_empty()
        {
            self.auth.mode = AuthMode::Browser;
            // Allow `browser+keyring:profile` syntax straight from the env var.
            let spec = browser.trim().to_string();
            let (browser, rest) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
            let (browser, keyring) = browser.split_once('+').unwrap_or((browser, ""));
            self.auth.browser = browser.to_string();
            self.auth.keyring = keyring.to_string();
            self.auth.profile = rest.to_string();
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Try to auto-detect a signed-in browser; fall back to anonymous.
    #[default]
    Auto,
    /// Read cookies from a browser via `yt-dlp --cookies-from-browser`.
    Browser,
    /// Read cookies from a Netscape-format `cookies.txt`.
    File,
    /// Never send cookies.
    None,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub mode: AuthMode,
    /// Browser name: firefox, chrome, chromium, brave, edge, opera, vivaldi, whale.
    pub browser: String,
    /// Chromium keyring: BASICTEXT, GNOMEKEYRING, KWALLET, KWALLET5, KWALLET6.
    pub keyring: String,
    /// Explicit profile name or path.
    pub profile: String,
    /// Netscape cookies file, used when `mode = "file"`.
    pub cookies_file: String,
}

impl AuthConfig {
    pub fn configured(&self) -> bool {
        matches!(self.mode, AuthMode::Browser | AuthMode::File)
    }

    /// Arguments to hand `yt-dlp` for cookie authentication.
    pub fn ytdlp_args(&self) -> Vec<String> {
        match self.mode {
            AuthMode::Browser => {
                let browser = self.browser.trim();
                if browser.is_empty() {
                    return Vec::new();
                }
                let mut spec = browser.to_string();
                // `BROWSER+KEYRING` is Linux-only; yt-dlp knows its own way
                // around the macOS and Windows stores.
                #[cfg(target_os = "linux")]
                if !self.keyring.trim().is_empty() {
                    spec.push('+');
                    spec.push_str(self.keyring.trim());
                }
                if !self.profile.trim().is_empty() {
                    spec.push(':');
                    spec.push_str(self.profile.trim());
                }
                vec!["--cookies-from-browser".to_string(), spec]
            }
            AuthMode::File => {
                let file = self.cookies_file.trim();
                if file.is_empty() {
                    Vec::new()
                } else {
                    vec!["--cookies".to_string(), file.to_string()]
                }
            }
            AuthMode::Auto | AuthMode::None => Vec::new(),
        }
    }
}

/// For fields whose absence should mean "on", since `serde`'s default for a
/// bool is false.
fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub volume: f32,
    /// "dark" or "light".
    pub theme: String,
    /// When the last update check happened, as a Unix timestamp. The check is a
    /// request to somebody else's server, so it happens once a day rather than
    /// every launch.
    #[serde(default)]
    pub last_update_check: Option<u64>,
    /// Whether to keep playing similar music when a queue runs out.
    #[serde(default = "yes")]
    pub autoplay: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            volume: 0.8,
            theme: "dark".to_string(),
            last_update_check: None,
            autoplay: true,
        }
    }
}

#[cfg(test)]
mod locale_tests {
    use super::parse_locale;

    fn locale(raw: &str) -> (String, String) {
        let (language, region) = parse_locale(raw);
        (language, region)
    }

    #[test]
    fn reads_a_normal_locale() {
        assert_eq!(locale("en_US.UTF-8"), ("en".into(), "US".into()));
        assert_eq!(locale("de_AT.UTF-8"), ("de".into(), "AT".into()));
        assert_eq!(locale("pt_BR"), ("pt".into(), "BR".into()));
        // Three-letter languages exist and should survive.
        assert_eq!(locale("fil_PH"), ("fil".into(), "PH".into()));
        // A language on its own has no region.
        assert_eq!(locale("en"), ("en".into(), "US".into()));
        assert_eq!(locale("en_US@currency=EUR"), ("en".into(), "US".into()));
    }

    /// `C` and `POSIX` have no language code, and sending them as `hl` makes
    /// YouTube reject every request with HTTP 400 — so they must not be passed
    /// through. This is what a canary run in CI caught.
    #[test]
    fn locale_without_a_language_falls_back_to_english() {
        for raw in ["C", "C.UTF-8", "POSIX", "", "  ", "123_45", "_US"] {
            assert_eq!(
                locale(raw),
                ("en".into(), "US".into()),
                "{raw:?} should fall back"
            );
        }
    }

    /// A region that is not two letters is not a region.
    #[test]
    fn odd_regions_fall_back_to_us() {
        assert_eq!(locale("en_USA"), ("en".into(), "US".into()));
        assert_eq!(locale("en_1"), ("en".into(), "US".into()));
        assert_eq!(locale("en_us"), ("en".into(), "US".into()));
    }
}
