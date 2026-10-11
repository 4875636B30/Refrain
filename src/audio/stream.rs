//! Streaming playback source.
//!
//! We run `yt-dlp -o -` and read the media from its stdout. Going through
//! yt-dlp (rather than fetching a resolved URL ourselves) means it handles all
//! of YouTube's media-request requirements — headers, IP binding, rotating
//! hosts and expiring URLs — which a hand-rolled HTTP fetch gets wrong (we saw
//! intermittent 403s).
//!
//! The pipe is not seekable, so a background thread buffers everything from
//! offset 0. That suits `rodio`, which wants `Read + Seek`: backwards seeks are
//! served from memory and forwards seeks simply wait for the buffered data to
//! catch up. Bytes are mirrored into the on-disk cache as they arrive.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

/// Bytes published to the reader at a time.
const SUB_CHUNK: usize = 32 * 1024;

struct Window {
    data: Vec<u8>,
    read_pos: u64,
    done: bool,
    error: Option<String>,
}

/// A streaming, seekable view over a `yt-dlp` subprocess.
pub struct RemoteSource {
    window: Arc<Mutex<Window>>,
    stop: Arc<AtomicBool>,
}

impl RemoteSource {
    /// Spawn `yt-dlp` with `args` and stream its stdout. Returns immediately.
    pub fn new(args: Vec<String>, cache: Option<PathBuf>) -> Result<Self> {
        let ytdlp = crate::audio::resolver::locate_ytdlp()
            .context("yt-dlp not found, so streaming playback is unavailable")?;

        let mut child = Command::new(ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning yt-dlp for streaming")?;
        let stdout = child.stdout.take().context("capturing yt-dlp stdout")?;

        let window = Arc::new(Mutex::new(Window {
            data: Vec::new(),
            read_pos: 0,
            done: false,
            error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_window = Arc::clone(&window);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("ytm-stream".into())
            .spawn(move || prefetch(child, stdout, cache, thread_window, thread_stop))
            .context("spawning stream reader thread")?;

        Ok(Self { window, stop })
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Read for RemoteSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut window = self.window.lock().unwrap();
                if let Some(error) = &window.error {
                    return Err(io::Error::other(error.clone()));
                }
                if window.read_pos < window.data.len() as u64 {
                    let offset = window.read_pos as usize;
                    let n = out.len().min(window.data.len() - offset);
                    out[..n].copy_from_slice(&window.data[offset..offset + n]);
                    window.read_pos += n as u64;
                    return Ok(n);
                }
                if window.done {
                    return Ok(0); // clean EOF
                }
            }
            // Wait for the download to reach us.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Seek for RemoteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut window = self.window.lock().unwrap();
        let end = window.data.len() as i64;
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => end + n,
            SeekFrom::Current(n) => window.read_pos as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of stream",
            ));
        }
        window.read_pos = target as u64;
        Ok(window.read_pos)
    }
}

/// Reads the subprocess output into the window (and the cache file).
fn prefetch(
    mut child: Child,
    mut stdout: impl Read,
    cache: Option<PathBuf>,
    window: Arc<Mutex<Window>>,
    stop: Arc<AtomicBool>,
) {
    let cache_paths = cache.map(|final_path| {
        let scratch = final_path.with_extension("part");
        (scratch, final_path)
    });
    let mut file = cache_paths
        .as_ref()
        .and_then(|(scratch, _)| std::fs::File::create(scratch).ok());

    let mut buffer = vec![0u8; SUB_CHUNK];
    let mut complete = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match stdout.read(&mut buffer) {
            Ok(0) => {
                complete = true;
                break;
            }
            Ok(n) => {
                if let Some(file) = file.as_mut() {
                    let _ = file.write_all(&buffer[..n]);
                }
                window.lock().unwrap().data.extend_from_slice(&buffer[..n]);
            }
            Err(e) => {
                window.lock().unwrap().error = Some(format!("stream read failed: {e}"));
                break;
            }
        }
    }
    window.lock().unwrap().done = true;

    // Stop yt-dlp if we bailed out early (track changed, app closing…).
    let _ = child.kill();
    let _ = child.wait();

    drop(file);
    if let Some((scratch, final_path)) = cache_paths {
        if complete && window.lock().unwrap().error.is_none() {
            let _ = std::fs::rename(&scratch, &final_path);
        } else {
            let _ = std::fs::remove_file(&scratch);
        }
    }
}
