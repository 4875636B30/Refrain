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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub volume: f32,
    /// "dark" or "light".
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            volume: 0.8,
            theme: "dark".to_string(),
        }
    }
}
