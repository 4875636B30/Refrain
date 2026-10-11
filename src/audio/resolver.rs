//! Stream resolution via `yt-dlp`, cached on disk.
//!
//! `yt-dlp` owns the fragile work of YouTube signature/`n`-param deciphering.
//! We ask it for the best audio stream and let ffmpeg normalise it to `m4a`
//! (AAC), which `rodio`/symphonia can decode. Results are cached under
//! `~/.cache/youtube-music-rs/audio/<video_id>.m4a`, so replaying a track is
//! instant and seeking works.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

const DOWNLOAD_TIMEOUT_SECS: u64 = 180;

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// yt-dlp needs a JavaScript runtime to solve YouTube's JS challenges.
/// Deno is its default; Node.js works as well if Deno isn't present.
///
/// The argument is `RUNTIME[:PATH]`. Passing a bare path (e.g. `/usr/bin/node`)
/// is *not* valid and silently breaks challenge solving — the symptom is
/// "The page needs to be reloaded" — so a runtime on `PATH` is named directly,
/// and only an out-of-PATH install uses the `name:path` form.
pub(crate) fn js_runtime_args() -> Vec<String> {
    let name_only = |name: &str| vec!["--js-runtimes".to_string(), name.to_string()];

    if which("deno").is_some() {
        return name_only("deno");
    }
    if which("node").is_some() {
        return name_only("node");
    }
    // Deno's installer puts it in ~/.deno/bin, which may not be on PATH.
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".deno/bin");
        if dir.join("deno").is_file() {
            return vec![
                "--js-runtimes".to_string(),
                format!("deno:{}", dir.display()),
            ];
        }
    }
    Vec::new()
}

/// Authentication is needed because YouTube bot-blocks anonymous access to much
/// of the music catalogue. The cookie source comes from the config.
fn cookie_args(auth: &crate::config::AuthConfig) -> Vec<String> {
    auth.ytdlp_args()
}

fn env_override() -> Option<PathBuf> {
    std::env::var_os("YTDLP_PATH")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

fn home_bin() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home).join(".local/bin/yt-dlp");
    candidate.is_file().then_some(candidate)
}

fn search_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("yt-dlp"))
        .find(|p| p.is_file())
}

/// A `yt-dlp` shipped next to our own executable, so the app can be distributed
/// as a self-contained bundle.
fn beside_executable() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    ["yt-dlp", "yt-dlp_linux", "yt-dlp.exe"]
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Find a usable `yt-dlp` binary, preferring an explicit override.
pub fn locate_ytdlp() -> Option<PathBuf> {
    env_override()
        .or_else(beside_executable)
        .or_else(home_bin)
        .or_else(search_path)
}

/// Directory holding the decoded-audio cache.
pub fn cache_dir() -> PathBuf {
    crate::config::cache_dir().join("audio")
}

/// Cache location for a resolved track.
pub fn cache_path(video_id: &str) -> PathBuf {
    cache_dir().join(format!("{video_id}.m4a"))
}

/// Remove scratch files left behind by streams that were stopped early.
pub fn clean_stale_parts() {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part") {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn is_cached(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

/// Arguments for `yt-dlp` that stream the audio to stdout (`-o -`).
///
/// Restricted to `m4a` on purpose: YouTube serves it progressively with the
/// `moov` atom first, so `rodio` can decode from the first bytes. If no `m4a`
/// is offered the caller should fall back to downloading (where ffmpeg can
/// transcode).
pub fn stream_args(video_id: &str, auth: &crate::config::AuthConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.extend(js_runtime_args());
    args.extend(auth.ytdlp_args());
    args.extend([
        "-f".to_string(),
        "bestaudio[ext=m4a]".to_string(),
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "-o".to_string(),
        "-".to_string(),
        format!("https://www.youtube.com/watch?v={video_id}"),
    ]);
    args
}

/// Download and cache a track's audio. Returns the path to the playable file.
pub async fn download_track(video_id: &str, auth: &crate::config::AuthConfig) -> Result<PathBuf> {
    let dest = cache_path(video_id);
    if is_cached(&dest) {
        return Ok(dest);
    }

    let ytdlp = locate_ytdlp().context(
        "yt-dlp was not found. Install it (e.g. `pipx install yt-dlp`, or download the \
         standalone binary to ~/.local/bin) or set YTDLP_PATH.",
    )?;

    let dir = cache_dir();
    std::fs::create_dir_all(&dir).context("creating audio cache directory")?;

    // Work in a scratch dir so partial downloads never land in the cache.
    let tmp = dir.join(format!(".{video_id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).context("creating scratch directory")?;

    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let out_template = tmp.join("audio.%(ext)s");

    let mut args: Vec<String> = Vec::new();
    args.extend(js_runtime_args());
    args.extend(cookie_args(auth));
    args.extend([
        "-f".to_string(),
        "bestaudio[ext=m4a]/bestaudio/best".to_string(),
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "--no-part".to_string(),
        "-x".to_string(),
        "--audio-format".to_string(),
        "m4a".to_string(),
        "-o".to_string(),
        out_template.to_string_lossy().into_owned(),
        url,
    ]);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
        Command::new(&ytdlp).args(&args).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("yt-dlp timed out after {DOWNLOAD_TIMEOUT_SECS}s"))?
    .context("spawning yt-dlp")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .trim()
            .lines()
            .last()
            .unwrap_or("unknown error")
            .to_string();
        let _ = std::fs::remove_dir_all(&tmp);
        if stderr.contains("Sign in to confirm") || stderr.contains("not a bot") {
            bail!(
                "YouTube refused anonymous access to this track (bot check). Sign into \
                 music.youtube.com in your browser, or set a cookie source in \
                 ~/.config/youtube-music-rs/config.toml."
            );
        }
        bail!("yt-dlp failed for {video_id}: {last}");
    }

    // yt-dlp picks the extension, so locate whatever it produced.
    let produced = std::fs::read_dir(&tmp)
        .context("reading scratch directory")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file());

    let Some(produced) = produced else {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("yt-dlp produced no output file for {video_id}");
    };

    if let Err(e) = std::fs::rename(&produced, &dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e).context("moving downloaded audio into cache");
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(dest)
}
