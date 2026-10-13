//! Checking for a newer release, and installing it.
//!
//! Two halves. Finding out whether there is one is a GitHub API call. Putting
//! it in place is a file swap beside the running executable — and that only
//! happens where it is safe: an AppImage, or a bundle unpacked by hand. Where a
//! package manager owns the files (a distribution package, a Flatpak), the app
//! says so and offers the download instead, because two owners of one file is
//! how installations rot.
//!
//! Nothing is installed without a checksum to check it against. The releases
//! publish one next to each asset, and if it is missing the download is kept
//! but not put in place — an updater that installs an unverified binary is a
//! worse idea than no updater.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Where releases are published.
const REPO: &str = "4875636B30/Refrain";

/// The version this build reports, from the manifest.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// What an update would do to this installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// The AppImage file, which is the executable itself.
    AppImage(PathBuf),
    /// A directory holding the executable with the tools beside it.
    Bundle(PathBuf),
    /// Owned by a package manager. The app must not touch it.
    Managed,
    /// Somewhere else entirely: download it and let the user decide.
    Download,
}

/// A newer release, with the asset that suits this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes_url: String,
    asset_name: String,
    asset_url: String,
    checksum_url: Option<String>,
}

/// What happened to an update.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Put in place. It takes effect on the next start.
    Replaced,
    /// Saved, but not installed — either because it could not be verified, or
    /// because this installation is not one the app may overwrite.
    Saved(PathBuf, String),
}

/// Lowercase hex, for a hash digest.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Compare two version strings, ignoring a leading `v`.
///
/// Numeric per component, so 0.1.10 is newer than 0.1.9 — which a string
/// comparison gets wrong, and which is exactly the kind of thing that makes an
/// updater stop offering updates after the tenth patch.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A trailing component like "1-rc1" is not a number; take what is.
    let patch = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

/// The asset name for a platform, as the release workflow writes them.
fn asset_name(version: &str, os: &str, arch: &str) -> Option<String> {
    let target = match (os, arch) {
        ("linux", "x86_64") => format!("linux-{arch}"),
        ("macos", "aarch64") => "macos-aarch64-apple-darwin".to_string(),
        ("macos", "x86_64") => "macos-x86_64-apple-darwin".to_string(),
        ("windows", "x86_64") => format!("windows-{arch}"),
        _ => return None,
    };
    if os == "linux" && arch == "x86_64" {
        // Linux ships both; prefer the tarball, which is the bundle layout the
        // updater can replace file by file. The AppImage case is handled
        // separately, because there the AppImage *is* the installation.
        return Some(format!("refrain-{version}-{target}.tar.gz"));
    }
    let extension = if os == "windows" { "zip" } else { "tar.gz" };
    Some(format!("refrain-{version}-{target}.{extension}"))
}

/// How this installation should be updated, from where it runs.
///
/// `appimage` is the `APPIMAGE` environment variable, which the AppImage
/// runtime sets to the file it is running from.
pub fn install_kind_for(exe: &Path, appimage: Option<&str>) -> InstallKind {
    if let Some(appimage) = appimage.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(appimage);
        if path.is_file() {
            return InstallKind::AppImage(path);
        }
    }
    let Some(dir) = exe.parent() else {
        return InstallKind::Download;
    };
    // A package manager put it there, and only a package manager should move it.
    if dir.starts_with("/usr") || dir.starts_with("/app") || dir.starts_with("/nix") {
        return InstallKind::Managed;
    }
    // A development build: replacing it would be replacing the build output.
    if dir.ends_with("target/debug") || dir.ends_with("target/release") {
        return InstallKind::Download;
    }
    InstallKind::Bundle(dir.to_path_buf())
}

pub fn install_kind() -> InstallKind {
    let exe = std::env::current_exe().unwrap_or_default();
    let appimage = std::env::var("APPIMAGE").ok();
    install_kind_for(&exe, appimage.as_deref())
}

/// Ask GitHub for the latest release, and whether it is newer than `current`.
pub async fn check(http: &reqwest::Client, current: &str) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        // Shared addresses hit GitHub's unauthenticated limit, which is 60
        // requests an hour for everyone behind them. Saying so beats "403", and
        // the daily check simply tries again tomorrow.
        if status.as_u16() == 403 || status.as_u16() == 429 {
            return Err("GitHub is rate-limiting this network, so try again later".to_string());
        }
        return Err(format!("GitHub answered {status}"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("could not read the reply: {e}"))?;

    let tag = body
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("the release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    if !is_newer(&version, current) {
        return Ok(None);
    }

    let wanted =
        asset_name(&version, std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            format!(
                "no build for {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    let assets = body
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let find = |name: &str| {
        assets
            .iter()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|asset| asset.get("browser_download_url"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let asset_url = find(&wanted).ok_or_else(|| format!("the release has no {wanted}"))?;
    let checksum_url = find(&format!("{wanted}.sha256"));

    Ok(Some(Release {
        version,
        notes_url: body
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or("https://github.com/4875636B30/Refrain/releases")
            .to_string(),
        asset_name: wanted,
        asset_url,
        checksum_url,
    }))
}

/// Download `release` into `dir`, hashing it as it arrives.
///
/// Streamed to a file rather than read into memory: a release is tens of
/// megabytes, and holding it once in the response and again in the staging file
/// is the largest thing this app ever allocates. Hashing on the way in means
/// the file is written once and read once, and the peak stays at one chunk.
async fn download_to(
    http: &reqwest::Client,
    release: &Release,
    dir: &Path,
) -> Result<(PathBuf, String), String> {
    let mut response = http
        .get(&release.asset_url)
        .send()
        .await
        .map_err(|e| format!("could not download {}: {e}", release.asset_name))?;
    if !response.status().is_success() {
        return Err(format!(
            "downloading {} failed ({})",
            release.asset_name,
            response.status()
        ));
    }
    use sha2::Digest as _;
    let path = dir.join(format!(".{}.part", release.asset_name));
    let mut file = std::fs::File::create(&path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(chunk) => chunk,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(format!("could not download {}: {e}", release.asset_name));
            }
        };
        let Some(chunk) = chunk else { break };
        hasher.update(&chunk);
        if let Err(e) = std::io::Write::write_all(&mut file, &chunk) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("could not write {}: {e}", path.display()));
        }
    }
    drop(file);
    Ok((path, hex_digest(hasher.finalize())))
}

/// Download `release` and, where it is safe, put it in place.
pub async fn install(
    http: &reqwest::Client,
    release: &Release,
    kind: &InstallKind,
) -> Result<Outcome, String> {
    // Stage beside the destination, so putting it in place is a rename rather
    // than a copy across filesystems.
    let dir = match kind {
        InstallKind::AppImage(target) => target.parent().map(Path::to_path_buf),
        InstallKind::Bundle(dir) => Some(dir.clone()),
        InstallKind::Managed | InstallKind::Download => download_dir(),
    };
    let dir = dir.ok_or("nowhere to put the download")?;
    let (staged, actual) = download_to(http, release, &dir).await?;

    // The checksum is what makes this safe to do unattended, so it is required.
    let Some(checksum_url) = &release.checksum_url else {
        let path = keep_download(&staged, &release.asset_name)?;
        return Ok(Outcome::Saved(
            path,
            "this release publishes no checksum, so it was downloaded rather than installed"
                .to_string(),
        ));
    };
    let expected = http
        .get(checksum_url)
        .send()
        .await
        .map_err(|e| format!("could not fetch the checksum: {e}"))?
        .text()
        .await
        .map_err(|e| format!("could not fetch the checksum: {e}"))?;
    let expected = expected
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if expected.is_empty() || expected != actual {
        let _ = std::fs::remove_file(&staged);
        return Err("the download does not match its published checksum".to_string());
    }

    match kind {
        InstallKind::AppImage(target) => {
            std::fs::rename(&staged, target)
                .map_err(|e| format!("could not replace {}: {e}", target.display()))?;
            make_executable(target);
            Ok(Outcome::Replaced)
        }
        InstallKind::Bundle(bundle) => {
            let unpacked = unpack_beside(&release.asset_name, &staged, bundle)?;
            let mut replaced = Vec::new();
            for name in ["refrain", "yt-dlp_linux", "qjs"] {
                let from = unpacked.join(name);
                if from.is_file() {
                    let to = bundle.join(name);
                    copy_atomically(&from, &to)?;
                    if name == "refrain" {
                        make_executable(&to);
                    }
                    replaced.push(name.to_string());
                }
            }
            let _ = std::fs::remove_file(&staged);
            let _ = std::fs::remove_dir_all(&unpacked);
            if replaced.is_empty() {
                return Err("the download did not contain anything to install".to_string());
            }
            Ok(Outcome::Replaced)
        }
        InstallKind::Managed | InstallKind::Download => {
            let path = keep_download(&staged, &release.asset_name)?;
            Ok(Outcome::Saved(
                path,
                "this copy is not one the app may replace, so it was downloaded instead"
                    .to_string(),
            ))
        }
    }
}

/// Move a staged download to its final name, in the same directory.
fn keep_download(staged: &Path, name: &str) -> Result<PathBuf, String> {
    let Some(dir) = staged.parent() else {
        return Err("the download has no directory".to_string());
    };
    let path = dir.join(name);
    std::fs::rename(staged, &path)
        .map_err(|e| format!("could not save {}: {e}", path.display()))?;
    Ok(path)
}

fn download_dir() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .filter(|dir| dir.is_dir())
        .or_else(|| Some(std::env::temp_dir()))
}

/// Unpack a release tarball into a directory beside the install, so the files
/// it replaces stay on one filesystem.
fn unpack_beside(name: &str, archive: &Path, beside: &Path) -> Result<PathBuf, String> {
    if !name.ends_with(".tar.gz") {
        return Err(format!("do not know how to unpack {name}"));
    }
    let dir = beside.join(".refrain-update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    tar::Archive::new(decoder)
        .unpack(&dir)
        .map_err(|e| format!("could not unpack the download: {e}"))?;
    // Release tarballs hold one directory; flatten it.
    Ok(single_directory(&dir).unwrap_or_else(|| dir.clone()))
}

/// Copy a file into place through a temporary beside it, so an interrupted
/// copy leaves the old file rather than half a new one.
fn copy_atomically(from: &Path, to: &Path) -> Result<(), String> {
    let Some(dir) = to.parent() else {
        return Err(format!("{} has no directory", to.display()));
    };
    let staging = dir.join(format!(
        ".{}.new",
        to.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("refrain")
    ));
    std::fs::copy(from, &staging)
        .map_err(|e| format!("could not write {}: {e}", staging.display()))?;
    if let Ok(meta) = std::fs::metadata(to) {
        let _ = std::fs::set_permissions(&staging, meta.permissions());
    }
    std::fs::rename(&staging, to).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("could not replace {}: {e}", to.display())
    })
}
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}
/// The single top-level directory a release tarball contains.
fn single_directory(dir: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(dir).ok()?.flatten();
    let first = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    let path = first.path();
    path.is_dir().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this exists for: 0.1.10 is newer than 0.1.9, and a string
    /// comparison says otherwise — which would quietly stop offering updates
    /// after the tenth patch.
    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.9", "0.1.10"));
        assert!(!is_newer("0.1.6", "0.1.6"));
    }

    /// The API returns tags with a `v`; the manifest does not.
    #[test]
    fn a_leading_v_is_not_a_version() {
        assert!(is_newer("v0.1.7", "0.1.6"));
        assert!(!is_newer("v0.1.6", "0.1.6"));
    }

    #[test]
    fn nonsense_is_not_newer() {
        assert!(!is_newer("", "0.1.6"));
        assert!(!is_newer("latest", "0.1.6"));
        assert!(!is_newer("0.1", "0.1.6"));
        assert!(!is_newer("0.1.7", "not a version"));
    }

    #[test]
    fn the_asset_matches_the_platform() {
        assert_eq!(
            asset_name("0.1.7", "linux", "x86_64").as_deref(),
            Some("refrain-0.1.7-linux-x86_64.tar.gz")
        );
        assert_eq!(
            asset_name("0.1.7", "macos", "aarch64").as_deref(),
            Some("refrain-0.1.7-macos-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            asset_name("0.1.7", "windows", "x86_64").as_deref(),
            Some("refrain-0.1.7-windows-x86_64.zip")
        );
        assert_eq!(asset_name("0.1.7", "linux", "aarch64"), None);
    }

    /// An AppImage is its own installation: the file the runtime says it is.
    #[test]
    fn an_appimage_updates_the_file_it_runs_from() {
        let dir = std::env::temp_dir().join("refrain-update-test");
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("Refrain.AppImage");
        std::fs::write(&image, b"x").unwrap();
        assert_eq!(
            install_kind_for(
                Path::new("/home/someone/.local/bin/refrain"),
                Some(&image.to_string_lossy())
            ),
            InstallKind::AppImage(image)
        );
    }

    /// A package manager owns its files; an app that overwrites them is how an
    /// installation ends up with two owners and a version that is neither.
    #[test]
    fn package_managed_copies_are_left_alone() {
        assert_eq!(
            install_kind_for(Path::new("/usr/bin/refrain"), None),
            InstallKind::Managed
        );
        assert_eq!(
            install_kind_for(Path::new("/app/bin/refrain"), None),
            InstallKind::Managed
        );
        assert_eq!(
            install_kind_for(
                Path::new("/home/someone/Projects/refrain/target/debug/refrain"),
                None
            ),
            InstallKind::Download
        );
    }

    #[test]
    fn an_unpacked_bundle_is_updated_in_place() {
        assert_eq!(
            install_kind_for(Path::new("/home/someone/.local/bin/refrain"), None),
            InstallKind::Bundle(PathBuf::from("/home/someone/.local/bin"))
        );
    }

    /// The whole path against the real release: ask, download, check the
    /// checksum, unpack, and swap into a directory. Ignored by default because
    /// it needs the network — CI should not fail because GitHub is having a
    /// day — so run it by hand:
    ///
    ///     cargo test -- --ignored installs_for_real
    ///
    /// It installs into a temporary directory, never into this checkout.
    #[tokio::test]
    #[ignore]
    async fn installs_for_real() {
        let http = reqwest::Client::builder()
            .user_agent("refrain-update-test")
            .build()
            .expect("client");
        // Pretend to be older than anything published, so a release is always
        // offered and this test does not quietly stop testing anything.
        let release = check(&http, "0.0.1")
            .await
            .expect("the check should succeed")
            .expect("a release should be offered");
        assert!(
            release.checksum_url.is_some(),
            "no checksum to verify against"
        );

        let dir = std::env::temp_dir().join("refrain-install-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let outcome = install(&http, &release, &InstallKind::Bundle(dir.clone()))
            .await
            .expect("the install should succeed");
        assert!(matches!(outcome, Outcome::Replaced), "got {outcome:?}");

        for name in ["refrain", "yt-dlp_linux", "qjs"] {
            let path = dir.join(name);
            let size = std::fs::metadata(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                .len();
            assert!(size > 0, "{} is empty", path.display());
        }
        println!("installed {} into {}", release.version, dir.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A download that does not match its published checksum must not be
    /// installed, whatever else is true about it.
    #[tokio::test]
    #[ignore]
    async fn refuses_a_download_that_does_not_match() {
        let http = reqwest::Client::builder()
            .user_agent("refrain-update-test")
            .build()
            .expect("client");
        let mut release = check(&http, "0.0.1")
            .await
            .expect("check")
            .expect("release");
        // A checksum for something else entirely.
        release.checksum_url = Some(
            "https://github.com/4875636B30/Refrain/releases/download/v0.1.5/refrain-0.1.5-windows-x86_64.zip.sha256"
                .to_string(),
        );
        let dir = std::env::temp_dir().join("refrain-install-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let err = install(&http, &release, &InstallKind::Bundle(dir.clone()))
            .await
            .expect_err("a mismatched checksum must be refused");
        assert!(err.contains("checksum"), "unexpected error: {err}");
        assert!(
            !dir.join("refrain").exists(),
            "nothing should have been installed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_checksum_is_the_published_one() {
        use sha2::Digest as _;
        // sha256 of "abc", from any reference table.
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hex_digest(hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
