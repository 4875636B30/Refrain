//! Stream resolution via `yt-dlp`, cached on disk.
//!
//! `yt-dlp` owns the fragile work of YouTube signature/`n`-param deciphering.
//! We ask it for the best audio stream and let ffmpeg normalise it to `m4a`
//! (AAC), which `rodio`/symphonia can decode. Results are cached under
//! `~/.cache/refrain/audio/<video_id>.m4a`, so replaying a track is
//! instant and seeking works.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

const DOWNLOAD_TIMEOUT_SECS: u64 = 180;

/// A binary on `PATH`, with the platform's executable suffix.
///
/// Windows needs this: the file is `node.exe`, and looking for `node` finds
/// nothing — which silently leaves yt-dlp without a JavaScript runtime, and
/// playback fails with a confusing challenge error.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
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
    let with_path = |name: &str, path: &Path| {
        vec![
            "--js-runtimes".to_string(),
            format!("{}:{}", name, path.display()),
        ]
    };

    // A runtime shipped beside us comes first: a bundle that carries one needs
    // nothing installed, and this is how a Flatpak or an AppImage can be
    // self-sufficient without bundling a whole Node.
    for (name, file) in [
        ("deno", "deno"),
        ("node", "node"),
        ("quickjs", "qjs"),
        ("quickjs", "quickjs"),
    ] {
        if let Some(path) = beside_executable_named(file) {
            return with_path(name, &path);
        }
    }

    // Then the usual ones on PATH, in yt-dlp's own order of preference.
    for name in ["deno", "node"] {
        if which(name).is_some() {
            return name_only(name);
        }
    }
    if let Some(path) = which("qjs").or_else(|| which("quickjs")) {
        return with_path("quickjs", &path);
    }

    // Deno's installer puts it in ~/.deno/bin, which may not be on PATH.
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".deno/bin");
        if dir.join("deno").is_file() {
            return with_path("deno", &dir.join("deno"));
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

/// A file of this name next to our own executable.
fn beside_executable_named(name: &str) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// A `yt-dlp` shipped next to our own executable, so the app can be distributed
/// as a self-contained bundle.
fn beside_executable() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    ["yt-dlp", "yt-dlp_linux", "yt-dlp_macos", "yt-dlp.exe"]
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Every `yt-dlp` we can find, in the order they are preferred when they are
/// equally fresh: an explicit override, the copy shipped beside us, a
/// user-local install, then whatever is on `PATH`.
fn candidates() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for candidate in [
        env_override(),
        beside_executable(),
        home_bin(),
        search_path(),
    ]
    .into_iter()
    .flatten()
    {
        if !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Choose between candidates by how recently each was written, falling back to
/// preference order.
///
/// Split out from the filesystem work so the rule itself can be tested.
fn freshest(candidates: &[(PathBuf, Option<SystemTime>)]) -> Option<PathBuf> {
    let mut best: Option<(&SystemTime, usize, &PathBuf)> = None;
    for (index, (path, time)) in candidates.iter().enumerate() {
        let Some(time) = time else {
            continue;
        };
        let better = match best {
            None => true,
            // Ties go to the earlier candidate, which is the more deliberate
            // location (beside the binary beats whatever is on PATH).
            Some((best_time, best_index, _)) => {
                time > best_time || (time == best_time && index < best_index)
            }
        };
        if better {
            best = Some((time, index, path));
        }
    }
    best.map(|(_, _, path)| path.clone())
        .or_else(|| candidates.first().map(|(path, _)| path.clone()))
}

/// Find a usable `yt-dlp` binary.
///
/// `YTDLP_PATH` is an instruction rather than a guess, so it wins outright.
/// Otherwise the **freshest** candidate wins, not the first one found: the
/// bundle ships a copy frozen at release time, and yt-dlp is exactly the part
/// that breaks when YouTube changes — so preferring the bundled copy
/// unconditionally would leave a released build unable to play anything until
/// the app itself was updated. Which binary was written most recently is a good
/// proxy for which is current, and it costs a `stat` rather than running
/// `--version` (over a second on a PyInstaller build).
pub fn locate_ytdlp() -> Option<PathBuf> {
    if let Some(explicit) = env_override() {
        return Some(explicit);
    }
    let found = candidates();
    let with_times: Vec<(PathBuf, Option<SystemTime>)> = found
        .iter()
        .map(|path| (path.clone(), modified(path)))
        .collect();
    freshest(&with_times)
}

/// The version `yt-dlp` reports, for diagnostics.
///
/// Spawns a process (over a second), so ask on demand — the settings window —
/// rather than at startup.
pub fn ytdlp_version(ytdlp: &Path) -> Option<String> {
    let output = std::process::Command::new(ytdlp)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// How long ago the chosen `yt-dlp` was written.
///
/// A proxy for its age: both a download and a package update refresh it.
pub fn ytdlp_age(ytdlp: &Path) -> Option<Duration> {
    modified(ytdlp)?.elapsed().ok()
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
/// Budget for cached audio. Tracks run a few MB each, so this is a few hundred
/// of them: everything played recently stays, but the directory cannot grow
/// without bound.
const CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Files touched more recently than this are never evicted — they may be the
/// track that is playing, one being warmed, or a fetch still in flight.
const CACHE_PROTECT: Duration = Duration::from_secs(10 * 60);

/// Turn yt-dlp's stderr into something a person can act on.
///
/// The raw text is written for developers — `ERROR: [youtube] <id>: Sign in to
/// confirm you're not a bot` — and several of these failures have a specific fix
/// worth naming rather than leaving the reader to infer it. Anything
/// unrecognised falls back to yt-dlp's own last error line, which still beats a
/// generic message.
pub fn explain_failure(stderr: &str) -> String {
    let lower = stderr.to_lowercase();

    // yt-dlp delegates YouTube's JS challenges to a real runtime.
    if lower.contains("page needs to be reloaded")
        || lower.contains("js challenge")
        || lower.contains("signature solving failed")
    {
        return "YouTube's JavaScript challenge could not be solved — install `node` \
                (or `deno`) so yt-dlp can run it."
            .to_string();
    }
    if lower.contains("available in your country") || lower.contains("blocked it in your country") {
        return "This track is not available in your region.".to_string();
    }
    if lower.contains("age-restricted")
        || lower.contains("age restricted")
        || lower.contains("confirm your age")
    {
        return "This track is age-restricted, which needs a signed-in session.".to_string();
    }
    if lower.contains("not a bot") {
        return "YouTube asked for a bot check. Sign in to music.youtube.com in your \
                browser, then use Refresh sign-in in Settings."
            .to_string();
    }
    if lower.contains("private video") {
        return "This track is private.".to_string();
    }
    if lower.contains("video unavailable")
        || lower.contains("video is unavailable")
        || lower.contains("this video is not available")
    {
        return "This track is unavailable on YouTube.".to_string();
    }
    if lower.contains("requested format is not available") {
        return "No playable audio stream was offered for this track.".to_string();
    }
    if lower.contains("name resolution")
        || lower.contains("unable to download webpage")
        || lower.contains("getaddrinfo")
    {
        return "Could not reach YouTube — check your network connection.".to_string();
    }
    if lower.contains("ffmpeg") && lower.contains("not") {
        return "ffmpeg is needed for this track but was not found.".to_string();
    }

    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("ERROR"))
        .map(|line| {
            line.trim_start_matches("ERROR")
                .trim_start_matches(':')
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| "yt-dlp failed for an unrecognised reason".to_string())
}

/// How much cached audio is on disk: (tracks, bytes).
pub fn cache_usage() -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return (0, 0);
    };
    let mut tracks = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|ext| ext != "m4a") {
            continue;
        }
        bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        tracks += 1;
    }
    (tracks, bytes)
}

/// Delete cached audio, keeping one track's file.
///
/// The exception matters: deleting the file that is playing would be fine on
/// Linux (the open handle survives), but it would silently un-cache the track
/// you are listening to.
pub fn clear_audio_cache(keep: Option<&str>) -> (usize, u64) {
    let keep = keep.map(cache_path);
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return (0, 0);
    };
    let (mut removed, mut freed) = (0usize, 0u64);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "m4a") {
            continue;
        }
        if keep.as_deref() == Some(path.as_path()) {
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
            freed += size;
        }
    }
    (removed, freed)
}

/// Drop the least recently used cached tracks until the cache fits its budget.
///
/// Returns how many files were removed and how many bytes that freed.
pub fn prune_cache() -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return (0, 0);
    };

    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "m4a") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        files.push((meta.modified().unwrap_or(UNIX_EPOCH), meta.len(), path));
    }

    let plan = eviction_plan(files, CACHE_BUDGET_BYTES, SystemTime::now(), CACHE_PROTECT);
    let (mut removed, mut freed) = (0usize, 0u64);
    for (size, path) in plan {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
            freed += size;
        }
    }
    (removed, freed)
}

/// Choose which cached files to drop: oldest first, until what remains fits the
/// budget. Anything touched within `protect` is left alone — it may be the
/// track that is playing, one being warmed, or a fetch still in flight, and
/// evicting those would be far worse than being briefly over budget.
fn eviction_plan(
    files: Vec<(SystemTime, u64, PathBuf)>,
    budget: u64,
    now: SystemTime,
    protect: Duration,
) -> Vec<(u64, PathBuf)> {
    let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    if total <= budget {
        return Vec::new();
    }

    let mut files = files;
    files.sort_by_key(|(touched, _, _)| *touched);

    let mut plan = Vec::new();
    for (touched, size, path) in files {
        if total <= budget {
            break;
        }
        if now.duration_since(touched).unwrap_or_default() < protect {
            continue;
        }
        total -= size;
        plan.push((size, path));
    }
    plan
}

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

/// User agent for direct media fetches. Any agent works — unlike the API, the
/// media request isn't bound to the client that resolved it.
const FETCH_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Tried if the first attempt is refused. YouTube's media edges are picky about
/// agent/range combinations in ways that vary by route, so a second agent is
/// cheap insurance against a spurious 403.
const FALLBACK_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

/// Size of each ranged request. Big enough that a normal track arrives in one
/// request, small enough that a long one doesn't need a huge buffer.
const FETCH_CHUNK: u64 = 16 * 1024 * 1024;

/// Anything smaller than this isn't an audio file worth caching.
const MIN_AUDIO_BYTES: u64 = 16 * 1024;

/// Resolve a direct, range-capable audio URL with `yt-dlp -g`.
///
/// This still costs a few seconds (YouTube's URLs are ciphered and need solving),
/// but everything after it — fetching, caching, replaying — is ours.
pub async fn resolve_url(video_id: &str, auth: &crate::config::AuthConfig) -> Result<String> {
    let ytdlp = locate_ytdlp().context("yt-dlp not found")?;

    let mut args: Vec<String> = Vec::new();
    args.extend(js_runtime_args());
    args.extend(auth.ytdlp_args());
    args.extend([
        "-f".to_string(),
        "bestaudio[ext=m4a]/bestaudio".to_string(),
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "-g".to_string(),
        format!("https://www.youtube.com/watch?v={video_id}"),
    ]);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Command::new(&ytdlp).args(&args).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("yt-dlp timed out resolving the URL"))?
    .context("spawning yt-dlp")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("could not resolve {video_id}: {}", explain_failure(&stderr));
    }

    let url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !url.starts_with("http") {
        bail!("yt-dlp returned no URL for {video_id}");
    }
    Ok(url)
}

/// Download a resolved URL straight into the audio cache.
///
/// The explicit `Range` header matters: YouTube throttles un-ranged media
/// requests (we measured a full track at 33 KB/s without it, and 5–9 MB/s
/// with), which is what made fetching direct URLs look unreliable.
/// Fetch and write one bounded range of the media into `scratch`.
///
/// Ranges are always *bounded*. An un-ranged GET is throttled to ~33 KB/s, and
/// an open-ended `bytes=0-` is refused outright (403) for some agent/route
/// combinations — which is a failure, not a slowdown, so it must be avoided.
async fn fetch_ranges(
    http: &reqwest::Client,
    url: &str,
    user_agent: &str,
    scratch: &Path,
) -> Result<u64> {
    let mut file = std::fs::File::create(scratch).context("creating cache scratch file")?;
    let mut written = 0u64;

    loop {
        let end = written + FETCH_CHUNK - 1;
        let mut response = http
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={written}-{end}"))
            .header(reqwest::header::USER_AGENT, user_agent)
            .send()
            .await
            .context("requesting audio")?;

        let status = response.status();
        if !status.is_success() {
            bail!("audio request returned HTTP {status}");
        }
        // A 200 means the range was ignored and the whole file is coming, so
        // there is nothing left to ask for afterwards.
        let whole_file = status == reqwest::StatusCode::OK;

        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await.context("reading audio body")? {
            file.write_all(&chunk).context("writing cached audio")?;
            received += chunk.len() as u64;
        }
        written += received;

        // A short response means the file ended inside this range.
        if whole_file || received == 0 || received < FETCH_CHUNK {
            break;
        }
    }

    file.flush().context("flushing cached audio")?;
    Ok(written)
}

pub async fn fetch_audio(url: &str, video_id: &str) -> Result<PathBuf> {
    let dest = cache_path(video_id);
    if is_cached(&dest) {
        return Ok(dest);
    }

    let dir = cache_dir();
    std::fs::create_dir_all(&dir).context("creating audio cache directory")?;
    let scratch = dir.join(format!(".{video_id}.fetch"));

    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;

    // Media URLs live about six hours, but a refusal is not always an expired
    // URL, so give a second agent a chance before giving up on it.
    let mut last_error = None;
    for user_agent in [FETCH_USER_AGENT, FALLBACK_USER_AGENT] {
        match fetch_ranges(&http, url, user_agent, &scratch).await {
            Ok(bytes) if bytes >= MIN_AUDIO_BYTES => {
                std::fs::rename(&scratch, &dest).context("moving audio into cache")?;
                return Ok(dest);
            }
            Ok(bytes) => {
                last_error = Some(anyhow::anyhow!("audio response was only {bytes} bytes"))
            }
            Err(e) => last_error = Some(e),
        }
        let _ = std::fs::remove_file(&scratch);
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not fetch audio")))
}

/// Resolve and cache a track, reusing a known URL when it still works.
///
/// Prefers our own ranged fetch (a whole track lands in about a second) and
/// falls back to yt-dlp's downloader if the URL is rejected. Returns the URL
/// that was used, so callers can keep it for later.
pub async fn resolve_and_fetch(
    video_id: &str,
    auth: &crate::config::AuthConfig,
    known_url: Option<String>,
) -> (Option<String>, Result<PathBuf>) {
    let mut url = match known_url {
        Some(url) => Some(url),
        None => resolve_url(video_id, auth).await.ok(),
    };

    let mut result = match &url {
        Some(url) => fetch_audio(url, video_id).await,
        None => Err(anyhow::anyhow!("could not resolve a stream URL")),
    };

    // A rejected URL is usually simply expired, so try one fresh resolve
    // before falling back to the slower path.
    if result.is_err()
        && let Ok(fresh) = resolve_url(video_id, auth).await
    {
        url = Some(fresh.clone());
        result = fetch_audio(&fresh, video_id).await;
    }

    if let Err(e) = &result {
        // Falling back here costs ~8s per track instead of ~1s, so say so:
        // a silent degradation looks like "resolution is just slow".
        eprintln!("[cache] ranged fetch failed for {video_id} ({e}); using yt-dlp");
        result = download_track(video_id, auth).await;
    }

    (url, result)
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
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("{}", explain_failure(&stderr));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(secs: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(secs)
    }

    fn file(age_secs: u64, size: u64, name: &str) -> (SystemTime, u64, PathBuf) {
        (ago(age_secs), size, PathBuf::from(name))
    }

    /// Under budget, nothing is touched.
    #[test]
    fn keeps_everything_under_budget() {
        let files = vec![file(60, 100, "a"), file(120, 100, "b")];
        let plan = eviction_plan(files, 1000, SystemTime::now(), Duration::from_secs(10));
        assert!(plan.is_empty());
    }

    /// Over budget, the oldest go first and only enough to fit.
    #[test]
    fn evicts_oldest_first_until_it_fits() {
        let files = vec![
            file(3600, 400, "oldest"),
            file(1800, 400, "middle"),
            file(900, 400, "newest"),
        ];
        // 1200 bytes held, budget 500: dropping the two oldest leaves 400.
        // The protection window is short here so all three are eligible.
        let plan = eviction_plan(files, 500, SystemTime::now(), Duration::from_secs(60));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].1, PathBuf::from("oldest"));
        assert_eq!(plan[1].1, PathBuf::from("middle"));
    }

    /// A track being written or played right now must survive, even when the
    /// cache is over budget — hence the age floor rather than plain LRU.
    #[test]
    fn never_evicts_recently_touched_files() {
        let files = vec![file(3600, 800, "stale"), file(1, 800, "in-use")];
        // Budget forces eviction, but only the stale file may go.
        let plan = eviction_plan(files, 800, SystemTime::now(), Duration::from_secs(600));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].1, PathBuf::from("stale"));
    }

    /// If everything is protected, the cache stays over budget rather than
    /// evicting something that is in use.
    #[test]
    fn stays_over_budget_when_all_protected() {
        let files = vec![file(1, 800, "a"), file(2, 800, "b")];
        let plan = eviction_plan(files, 100, SystemTime::now(), Duration::from_secs(600));
        assert!(plan.is_empty());
    }
}

#[cfg(test)]
mod failure_tests {
    use super::explain_failure;

    /// Each of these has a specific fix, so the message should name it rather
    /// than echoing yt-dlp's phrasing back at the reader.
    #[test]
    fn names_the_fix_for_known_failures() {
        let cases = [
            (
                "ERROR: [youtube] abc: The page needs to be reloaded",
                "node",
            ),
            (
                "ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies",
                "bot check",
            ),
            (
                "ERROR: [youtube] abc: The uploader has not made this video available in your country",
                "region",
            ),
            (
                "ERROR: [youtube] abc: Sign in to confirm your age. This video may be inappropriate",
                "age-restricted",
            ),
            (
                "ERROR: [youtube] abc: Private video. Sign in if you've been granted access",
                "private",
            ),
            ("ERROR: [youtube] abc: Video unavailable", "unavailable"),
            (
                "ERROR: [youtube] abc: Requested format is not available. Use --list-formats",
                "No playable audio stream",
            ),
        ];
        for (stderr, expected) in cases {
            let message = explain_failure(stderr);
            assert!(
                message.to_lowercase().contains(&expected.to_lowercase()),
                "{stderr:?} produced {message:?}, which does not mention {expected:?}"
            );
        }
    }

    /// An unrecognised failure still reports yt-dlp's own error line.
    #[test]
    fn falls_back_to_the_last_error_line() {
        let stderr = "[youtube] abc: Downloading webpage\nERROR: something peculiar happened\n";
        assert_eq!(explain_failure(stderr), "something peculiar happened");
    }

    /// Empty stderr must not panic or produce an empty message.
    #[test]
    fn copes_with_no_stderr() {
        assert!(!explain_failure("").is_empty());
    }
}

#[cfg(test)]
mod selection_tests {
    use super::freshest;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn at(secs: u64, name: &str) -> (PathBuf, Option<SystemTime>) {
        (
            PathBuf::from(name),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        )
    }

    /// The whole point: a bundled copy frozen at release time must lose to a
    /// system copy that has been updated since.
    #[test]
    fn prefers_the_most_recently_written() {
        let candidates = vec![at(1_000, "beside-exe"), at(2_000, "on-path")];
        assert_eq!(freshest(&candidates), Some(PathBuf::from("on-path")));
        let reversed = vec![at(2_000, "beside-exe"), at(1_000, "on-path")];
        assert_eq!(freshest(&reversed), Some(PathBuf::from("beside-exe")));
    }

    /// Equal timestamps fall back to preference order, so a deliberate location
    /// beats a stray one on PATH.
    #[test]
    fn ties_keep_preference_order() {
        let candidates = vec![at(5_000, "beside-exe"), at(5_000, "on-path")];
        assert_eq!(freshest(&candidates), Some(PathBuf::from("beside-exe")));
    }

    /// An unreadable candidate is skipped rather than aborting the search.
    #[test]
    fn skips_candidates_without_a_timestamp() {
        let candidates = vec![(PathBuf::from("unreadable"), None), at(10, "readable")];
        assert_eq!(freshest(&candidates), Some(PathBuf::from("readable")));
    }

    /// With no timestamps at all, the first candidate still wins.
    #[test]
    fn falls_back_to_the_first_candidate() {
        let candidates = vec![
            (PathBuf::from("first"), None),
            (PathBuf::from("second"), None),
        ];
        assert_eq!(freshest(&candidates), Some(PathBuf::from("first")));
        assert_eq!(freshest(&[]), None);
    }
}
