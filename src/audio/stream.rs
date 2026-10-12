//! Streaming playback source.
//!
//! We run `yt-dlp -o -` and read the media from its stdout. Going through
//! yt-dlp (rather than fetching a resolved URL ourselves) means it handles all
//! of YouTube's media-request requirements — headers, IP binding, rotating
//! hosts and expiring URLs — which a hand-rolled HTTP fetch gets wrong.
//!
//! The pipe is not seekable, so a background thread buffers everything. Two
//! details matter for glitch-free playback:
//!
//! * The buffer is a list of chunks rather than one growing `Vec`. Appending to
//!   a growing `Vec` periodically reallocates and copies the whole thing while
//!   holding the lock, and since `rodio` pulls samples on the audio thread that
//!   stalls the audio callback — which shows up as ALSA underruns.
//! * Readers wait on a condvar, so they are woken the instant bytes arrive
//!   instead of polling.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Bytes read from the subprocess at a time.
const SUB_CHUNK: usize = 32 * 1024;

struct Window {
    chunks: Vec<Vec<u8>>,
    total: u64,
    read_pos: u64,
    done: bool,
    error: Option<String>,
}

impl Window {
    /// Find which chunk holds `position`, and the offset within it.
    fn locate(&self, position: u64) -> Option<(usize, usize)> {
        if position >= self.total {
            return None;
        }
        let mut start = 0;
        for (index, chunk) in self.chunks.iter().enumerate() {
            let end = start + chunk.len() as u64;
            if position < end {
                return Some((index, (position - start) as usize));
            }
            start = end;
        }
        None
    }

    fn append(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        self.chunks.push(bytes.to_vec());
    }

    fn read_into(&mut self, out: &mut [u8]) -> usize {
        let mut written = 0;
        while written < out.len() {
            let Some((index, offset)) = self.locate(self.read_pos + written as u64) else {
                break;
            };
            let chunk = &self.chunks[index];
            let n = (out.len() - written).min(chunk.len() - offset);
            out[written..written + n].copy_from_slice(&chunk[offset..offset + n]);
            written += n;
        }
        self.read_pos += written as u64;
        written
    }
}

/// A streaming, seekable view over a `yt-dlp` subprocess.
pub struct RemoteSource {
    shared: Arc<(Mutex<Window>, Condvar)>,
    stop: Arc<AtomicBool>,
}

impl RemoteSource {
    /// Spawn `yt-dlp` with `args` and stream its stdout. Returns immediately.
    pub fn new(args: Vec<String>, cache: Option<PathBuf>) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let ytdlp = crate::audio::resolver::locate_ytdlp()
            .context("yt-dlp not found, so streaming playback is unavailable")?;

        let mut child = Command::new(ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning yt-dlp for streaming")?;
        let stdout = child.stdout.take().context("capturing yt-dlp stdout")?;
        let stderr = child.stderr.take();

        let shared = Arc::new((
            Mutex::new(Window {
                chunks: Vec::new(),
                total: 0,
                read_pos: 0,
                done: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("refrain-stream".into())
            .spawn(move || prefetch(child, stdout, stderr, cache, thread_shared, thread_stop))
            .context("spawning stream reader thread")?;

        Ok(Self { shared, stop })
    }
}

/// A cheap handle to a stream's failure state, kept after the source itself has
/// been handed to a decoder.
///
/// Needed because rodio reports a reader that errors as "the format of the data
/// has not been recognized", which says nothing about the cause. Asking this
/// afterwards recovers the downloader's own explanation.
#[derive(Clone)]
pub struct StreamFailure(Arc<(Mutex<Window>, Condvar)>);

impl StreamFailure {
    pub fn get(&self) -> Option<String> {
        self.0.0.lock().ok().and_then(|window| window.error.clone())
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.shared.1.notify_all();
    }
}

impl RemoteSource {
    /// Keep this to find out why a stream failed, after the decoder has it.
    pub fn failure_handle(&self) -> StreamFailure {
        StreamFailure(Arc::clone(&self.shared))
    }
}

impl Read for RemoteSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let (window_lock, ready) = &*self.shared;
        let mut window = window_lock.lock().unwrap();
        loop {
            if let Some(error) = &window.error {
                return Err(io::Error::other(error.clone()));
            }
            if window.read_pos < window.total {
                return Ok(window.read_into(out));
            }
            if window.done {
                return Ok(0); // clean EOF
            }
            // Woken as soon as the downloader appends, so we don't stall the
            // audio callback on a polling loop.
            let (guard, _) = ready
                .wait_timeout(window, Duration::from_millis(100))
                .unwrap();
            window = guard;
        }
    }
}

impl Seek for RemoteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (window_lock, _) = &*self.shared;
        let mut window = window_lock.lock().unwrap();
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => window.total as i64 + n,
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

/// Reads the subprocess output into the shared window (and the cache file).
fn prefetch(
    mut child: Child,
    mut stdout: impl Read,
    stderr: Option<impl Read + Send + 'static>,
    cache: Option<PathBuf>,
    shared: Arc<(Mutex<Window>, Condvar)>,
    stop: Arc<AtomicBool>,
) {
    let (window_lock, ready) = &*shared;

    // Drain stderr on its own thread. Left unread, a full pipe would block
    // yt-dlp writing to it and it would never finish.
    let stderr_text = Arc::new(Mutex::new(String::new()));
    let drain = stderr.and_then(|mut pipe| {
        let sink = Arc::clone(&stderr_text);
        std::thread::Builder::new()
            .name("refrain-stream-err".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut text) = sink.lock() {
                        text.push_str(&String::from_utf8_lossy(&buffer[..n]));
                    }
                }
            })
            .ok()
    });

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
                // Short critical section: pushing a chunk never copies the
                // rest of the buffer.
                window_lock.lock().unwrap().append(&buffer[..n]);
                ready.notify_all();
            }
            Err(e) => {
                let mut window = window_lock.lock().unwrap();
                window.error = Some(format!("stream read failed: {e}"));
                drop(window);
                ready.notify_all();
                break;
            }
        }
    }

    // Stop yt-dlp if we bailed out early (track changed, app closing…).
    let stopped_early = stop.load(Ordering::Relaxed);
    if stopped_early {
        let _ = child.kill();
    }
    let status = child.wait();
    if let Some(handle) = drain {
        let _ = handle.join();
    }

    // A downloader that exited badly should say why. This has to be decided
    // *before* `done` is set: a reader that sees EOF first reports a decode
    // error instead, which says nothing about the real cause.
    let succeeded = status.map(|status| status.success()).unwrap_or(false);
    let failed = !stopped_early && !succeeded;
    let failure = failed.then(|| {
        let text = stderr_text
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default();
        crate::audio::resolver::explain_failure(&text)
    });

    {
        let mut window = window_lock.lock().unwrap();
        match failure {
            Some(message) => {
                window.error = Some(message);
                // Whatever arrived is a truncated file, not worth caching.
                complete = false;
            }
            None => window.done = true,
        }
    }
    ready.notify_all();

    drop(file);
    if let Some((scratch, final_path)) = cache_paths {
        if complete {
            let _ = std::fs::rename(&scratch, &final_path);
        } else {
            let _ = std::fs::remove_file(&scratch);
        }
    }
}
