//! Range-streaming reader for YouTube audio.
//!
//! `yt-dlp -g` yields a direct `googlevideo` URL that supports HTTP range
//! requests, and YouTube's `m4a` streams put the `moov` atom first — so a
//! decoder can start after a few hundred KB instead of the whole file. This
//! reader presents that stream to `rodio` as an ordinary `Read + Seek`, while a
//! background thread keeps a look-ahead window filled and mirrors the bytes
//! into the on-disk cache.
//!
//! Everything is buffered from offset 0, so seeking backwards is free and there
//! is no restart thrashing (individual tracks are only a few MB).

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How far ahead of the read cursor the background thread tries to stay.
const READAHEAD: u64 = 2 * 1024 * 1024;
/// Bytes requested per ranged HTTP call.
const CHUNK: u64 = 256 * 1024;
/// Bytes buffered before publishing to the reader, so playback can begin after
/// the first network round trip rather than a whole chunk.
const SUB_CHUNK: usize = 32 * 1024;

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

struct Window {
    data: Vec<u8>,
    read_pos: u64,
    total: Option<u64>,
    done: bool,
    error: Option<String>,
}

/// A lazily-streamed remote audio file.
pub struct RemoteSource {
    window: Arc<Mutex<Window>>,
    stop: Arc<AtomicBool>,
}

impl RemoteSource {
    /// Start streaming `url`. Returns immediately; the body is fetched in the
    /// background. If `cache` is given, the stream is also written there so the
    /// next play is instant.
    pub fn new(url: String, cache: Option<PathBuf>) -> Self {
        let window = Arc::new(Mutex::new(Window {
            data: Vec::new(),
            read_pos: 0,
            total: None,
            done: false,
            error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_window = Arc::clone(&window);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("ytm-stream".into())
            .spawn(move || prefetch(url, cache, thread_window, thread_stop))
            .expect("spawning stream thread");

        Self { window, stop }
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
            // Not enough buffered yet: let the prefetcher catch up.
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Seek for RemoteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut window = self.window.lock().unwrap();
        let end = window.total.unwrap_or(window.data.len() as u64) as i64;
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

/// Background prefetcher: fills the window and mirrors bytes into the cache.
fn prefetch(
    url: String,
    cache: Option<PathBuf>,
    window: Arc<Mutex<Window>>,
    stop: Arc<AtomicBool>,
) {
    let client = match reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(e) => return fail(&window, format!("http client: {e}")),
    };

    let cache_paths = cache.map(|final_path| {
        let scratch = final_path.with_extension("part");
        (scratch, final_path)
    });
    let mut file = cache_paths
        .as_ref()
        .and_then(|(scratch, _)| std::fs::File::create(scratch).ok());

    let mut next: u64 = 0;
    let mut buffer = vec![0u8; SUB_CHUNK];

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let (read_pos, have, done) = {
            let window = window.lock().unwrap();
            (window.read_pos, window.data.len() as u64, window.done)
        };
        if done {
            break;
        }
        if have >= read_pos + READAHEAD {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        next = next.max(have);

        let range = format!("bytes={}-{}", next, next + CHUNK - 1);
        let response = match client.get(&url).header("Range", range).send() {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                return fail(&window, format!("HTTP {} fetching audio", response.status()));
            }
            Err(e) => return fail(&window, format!("request failed: {e}")),
        };

        // A 200 to a ranged request means the server ignored the range; that is
        // only safe when we are still at the start of the file.
        if response.status().as_u16() == 200 && next > 0 {
            return fail(&window, "server ignored range request".to_string());
        }

        let total = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .or_else(|| response.content_length());
        {
            let mut window = window.lock().unwrap();
            if window.total.is_none() {
                window.total = total;
            }
        }

        let mut limited = response.take(CHUNK);
        loop {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            let n = match limited.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return fail(&window, format!("read failed: {e}"));
                }
            };
            if let Some(file) = file.as_mut() {
                let _ = file.write_all(&buffer[..n]);
            }
            window.lock().unwrap().data.extend_from_slice(&buffer[..n]);
            next += n as u64;
        }
    }

    // Finished (or stopped) — publish a complete file into the cache.
    drop(file);
    if window.lock().unwrap().error.is_none() {
        if let Some((scratch, final_path)) = cache_paths {
            let _ = std::fs::rename(&scratch, &final_path);
        }
    }
}

fn fail(window: &Arc<Mutex<Window>>, message: String) {
    window.lock().unwrap().error = Some(message);
}
