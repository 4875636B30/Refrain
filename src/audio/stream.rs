//! Streaming playback source.
//!
//! We run `yt-dlp -o -` and read the media from its stdout. Going through
//! yt-dlp (rather than fetching a resolved URL ourselves) means it handles all
//! of YouTube's media-request requirements — headers, IP binding, rotating
//! hosts and expiring URLs — which a hand-rolled HTTP fetch gets wrong.
//!
//! The pipe is not seekable, so a background thread copies it to a file and the
//! decoder reads from that file. It used to keep every byte in memory as well,
//! which cost a whole track — several megabytes, for as long as it played — and
//! bought nothing: the file is written anyway, because everything played is
//! cached. The file *is* the buffer.
//!
//! Two details matter for glitch-free playback:
//!
//! * The reader does its file I/O outside the lock. It holds the lock only to
//!   find out how much has arrived, so reading never delays the thread writing
//!   the next chunk — which is what starves the audio callback and shows up as
//!   an ALSA underrun.
//! * The length is published with a release store *after* the bytes are
//!   written, so a reader that sees the length also sees the bytes. Both sides
//!   wait on a condvar rather than polling.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Bytes read from the subprocess at a time.
const SUB_CHUNK: usize = 32 * 1024;

/// How long a reader waits before looking again. The condvar wakes it the
/// instant bytes arrive; this is only a backstop.
const WAIT: Duration = Duration::from_millis(100);

/// What the thread filling the buffer and the decoder draining it share.
struct Shared {
    /// Bytes written to the file, and therefore readable. Published after the
    /// write completes, so seeing the length means seeing the bytes.
    written: AtomicU64,
    /// Reads that had to wait for the downloader, and how long they waited.
    /// The audio thread is the one waiting, so this is the measure of whether
    /// the download is keeping up with playback.
    starved: AtomicU64,
    waited_nanos: AtomicU64,
    longest_nanos: AtomicU64,
    flags: Mutex<Flags>,
    ready: Condvar,
}

#[derive(Default)]
struct Flags {
    done: bool,
    error: Option<String>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            written: AtomicU64::new(0),
            starved: AtomicU64::new(0),
            waited_nanos: AtomicU64::new(0),
            longest_nanos: AtomicU64::new(0),
            flags: Mutex::new(Flags::default()),
            ready: Condvar::new(),
        })
    }

    fn error(&self) -> Option<String> {
        self.flags.lock().ok().and_then(|flags| flags.error.clone())
    }

    /// Note a read that had to wait for the downloader.
    ///
    /// Waits under a millisecond are scheduling noise rather than starvation,
    /// so they are not counted — otherwise a perfectly healthy stream reports
    /// hundreds of "starved" reads that nobody could have heard.
    fn note_wait(&self, elapsed: Duration) {
        if elapsed < Duration::from_millis(1) {
            return;
        }
        let ns = elapsed.as_nanos() as u64;
        self.starved.fetch_add(1, Ordering::Relaxed);
        self.waited_nanos.fetch_add(ns, Ordering::Relaxed);
        // High-water mark. Losing the race just means someone recorded a longer
        // wait, which is still the right answer.
        let mut longest = self.longest_nanos.load(Ordering::Relaxed);
        while ns > longest {
            match self.longest_nanos.compare_exchange_weak(
                longest,
                ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => longest = seen,
            }
        }
    }
}

/// A streaming, seekable view over a `yt-dlp` subprocess.
pub struct RemoteSource {
    shared: Arc<Shared>,
    file: std::fs::File,
    read_pos: u64,
    /// Whether any bytes have been handed to the decoder yet. The first wait is
    /// the stream starting up, which is not a dropout.
    produced: bool,
    stop: Arc<AtomicBool>,
}

impl RemoteSource {
    /// Spawn `yt-dlp` with `args` and stream its stdout. Returns immediately.
    ///
    /// `cache` is where the finished track is kept. Without one the buffer is a
    /// scratch file that is removed when the stream ends; with one, the same
    /// file is renamed into place when the download completes, so nothing is
    /// ever copied.
    pub fn new(args: Vec<String>, cache: Option<PathBuf>) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let ytdlp = crate::audio::resolver::locate_ytdlp()
            .context("yt-dlp not found, so streaming playback is unavailable")?;

        let (scratch, cached) = match cache {
            Some(final_path) => (final_path.with_extension("part"), Some(final_path)),
            None => (
                std::env::temp_dir().join(format!("refrain-stream-{}.part", std::process::id())),
                None,
            ),
        };
        // Two handles, because the reader needs read access: `File::create` is
        // write-only, and reading from it fails with a bad file descriptor.
        // Separate handles also mean neither side moves the other's position.
        let writer = std::fs::File::create(&scratch)
            .with_context(|| format!("creating the stream buffer {}", scratch.display()))?;
        let file = std::fs::File::open(&scratch)
            .with_context(|| format!("opening the stream buffer {}", scratch.display()))?;

        let mut child = Command::new(ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning yt-dlp for streaming")?;
        let stdout = child.stdout.take().context("capturing yt-dlp stdout")?;
        let stderr = child.stderr.take();

        let shared = Shared::new();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("refrain-stream".into())
            .spawn(move || {
                prefetch(
                    child,
                    stdout,
                    stderr,
                    writer,
                    scratch,
                    cached,
                    thread_shared,
                    thread_stop,
                )
            })
            .context("spawning stream reader thread")?;

        Ok(Self {
            shared,
            file,
            read_pos: 0,
            produced: false,
            stop,
        })
    }
}

/// A cheap handle to a stream's failure state, kept after the source itself has
/// been handed to a decoder.
///
/// Needed because rodio reports a reader that errors as "the format of the data
/// has not been recognized", which says nothing about the cause. Asking this
/// afterwards recovers the downloader's own explanation.
#[derive(Clone)]
pub struct StreamFailure(Arc<Shared>);

impl StreamFailure {
    pub fn get(&self) -> Option<String> {
        self.0.error()
    }
}

impl RemoteSource {
    /// Keep this to find out why a stream failed, after the decoder has it.
    pub fn failure_handle(&self) -> StreamFailure {
        StreamFailure(Arc::clone(&self.shared))
    }

    /// Keep this to find out how often the downloader fell behind playback.
    pub fn stats_handle(&self) -> StreamStats {
        StreamStats(Arc::clone(&self.shared))
    }
}

/// What a stream's reads cost: how often the decoder had to wait for the
/// downloader, and for how long.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Starvation {
    /// Reads that waited a millisecond or more for the next bytes.
    pub waits: u64,
    /// Total time spent waiting across those reads.
    pub waited: Duration,
    /// The longest single wait — the one most likely to have been heard.
    pub longest: Duration,
}

impl Starvation {
    /// Whether any single wait was long enough to plausibly be audible. ALSA's
    /// buffer is tens of milliseconds; a stall longer than that drains it.
    pub fn is_audible(&self) -> bool {
        self.longest >= Duration::from_millis(20)
    }
}

/// A cheap handle to a stream's read statistics, kept after the source itself
/// has been handed to a decoder.
#[derive(Clone)]
pub struct StreamStats(Arc<Shared>);

impl StreamStats {
    pub fn starvation(&self) -> Starvation {
        Starvation {
            waits: self.0.starved.load(Ordering::Relaxed),
            waited: Duration::from_nanos(self.0.waited_nanos.load(Ordering::Relaxed)),
            longest: Duration::from_nanos(self.0.longest_nanos.load(Ordering::Relaxed)),
        }
    }
}

/// A decoder over a remote stream, with the statistics that explain a dropout.
///
/// The two travel together because the statistics live in the stream's shared
/// state, and the decoder hides that state once it owns it.
pub struct PreparedStream {
    pub decoder: rodio::Decoder<RemoteSource>,
    pub stats: StreamStats,
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
    }
}

impl Read for RemoteSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Set when this read first has to wait, so the wait is measured once
        // per read rather than once per 100 ms wake-up.
        let mut waited_from: Option<Instant> = None;
        loop {
            // A failed download explains itself before anything else, because
            // whatever arrived is a truncated file.
            if let Some(error) = self.shared.error() {
                return Err(io::Error::other(error));
            }
            let available = self.shared.written.load(Ordering::Acquire);
            if self.read_pos < available {
                // This read waited for the downloader and is now returning
                // data. The first wait is the stream starting up rather than a
                // dropout: nothing has played yet, so nothing was interrupted.
                if let Some(from) = waited_from
                    && self.produced
                {
                    self.shared.note_wait(from.elapsed());
                }
                let want = out.len().min((available - self.read_pos) as usize);
                self.file.seek(SeekFrom::Start(self.read_pos))?;
                let read = self.file.read(&mut out[..want])?;
                self.read_pos += read as u64;
                self.produced = true;
                return Ok(read);
            }
            let flags = self
                .shared
                .flags
                .lock()
                .map_err(|_| io::Error::other("stream buffer lock poisoned"))?;
            if flags.done {
                return Ok(0); // clean EOF
            }
            waited_from.get_or_insert_with(Instant::now);
            // Woken as soon as the downloader appends, so we don't stall the
            // audio callback on a polling loop.
            let _ = self
                .shared
                .ready
                .wait_timeout(flags, WAIT)
                .map_err(|_| io::Error::other("stream buffer lock poisoned"))?;
        }
    }
}

impl Seek for RemoteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let written = self.shared.written.load(Ordering::Acquire);
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => written as i64 + n,
            SeekFrom::Current(n) => self.read_pos as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of stream",
            ));
        }
        self.read_pos = target as u64;
        Ok(self.read_pos)
    }
}

/// Copies the subprocess output into the buffer file.
#[allow(clippy::too_many_arguments)]
fn prefetch(
    mut child: Child,
    mut stdout: impl Read,
    stderr: Option<impl Read + Send + 'static>,
    mut writer: std::fs::File,
    scratch: PathBuf,
    cached: Option<PathBuf>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) {
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

    let mut buffer = vec![0u8; SUB_CHUNK];
    let mut complete = false;
    let mut written: u64 = 0;
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
                if writer.write_all(&buffer[..n]).is_err() {
                    let mut flags = shared.flags.lock().unwrap();
                    flags.error = Some("could not write the stream buffer".to_string());
                    drop(flags);
                    shared.ready.notify_all();
                    break;
                }
                written += n as u64;
                // After the bytes, never before: a reader that sees the length
                // must be able to read that many bytes.
                shared.written.store(written, Ordering::Release);
                shared.ready.notify_all();
            }
            Err(e) => {
                let mut flags = shared.flags.lock().unwrap();
                flags.error = Some(format!("stream read failed: {e}"));
                drop(flags);
                shared.ready.notify_all();
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
        let mut flags = shared.flags.lock().unwrap();
        match failure {
            Some(message) => {
                flags.error = Some(message);
                // Whatever arrived is a truncated file, not worth caching.
                complete = false;
            }
            None => flags.done = true,
        }
    }
    shared.ready.notify_all();

    drop(writer);
    // A finished stream becomes the cached track; anything else is scratch, and
    // a truncated file is not worth keeping.
    match (complete, &cached) {
        (true, Some(final_path)) => {
            let _ = std::fs::rename(&scratch, final_path);
        }
        _ => {
            let _ = std::fs::remove_file(&scratch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A source over a scratch file, without a subprocess: the reader is the
    /// part worth testing, and this is the only way to reach it.
    fn source(dir: &Path) -> (RemoteSource, PathBuf) {
        let path = dir.join("buffer.part");
        std::fs::File::create(&path).expect("create buffer");
        let file = std::fs::File::open(&path).expect("open buffer");
        let shared = Shared::new();
        let stop = Arc::new(AtomicBool::new(false));
        (
            RemoteSource {
                shared,
                file,
                read_pos: 0,
                produced: false,
                stop,
            },
            path,
        )
    }

    fn write(source: &RemoteSource, path: &Path, bytes: &[u8]) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open for append");
        file.write_all(bytes).expect("write");
        let written = source.shared.written.load(Ordering::Acquire) + bytes.len() as u64;
        source.shared.written.store(written, Ordering::Release);
        source.shared.ready.notify_all();
    }

    #[test]
    fn reads_what_has_been_written() {
        let dir = std::env::temp_dir().join("refrain-stream-test-read");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);

        write(&source, &path, b"hello ");
        write(&source, &path, b"world");

        let mut out = [0u8; 16];
        let n = source.read(&mut out).expect("read");
        assert_eq!(&out[..n], b"hello world");
    }

    /// The reader must wait for bytes that have not arrived yet, rather than
    /// reporting the end of the stream.
    #[test]
    fn waits_for_the_rest_instead_of_ending_early() {
        let dir = std::env::temp_dir().join("refrain-stream-test-wait");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        write(&source, &path, b"first");

        let writer_path = path.clone();
        let shared = Arc::clone(&source.shared);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .expect("open");
            file.write_all(b"second").expect("write");
            shared.written.store(11, Ordering::Release);
            shared.ready.notify_all();
        });

        let mut out = [0u8; 16];
        let first = source.read(&mut out).expect("first read");
        assert_eq!(&out[..first], b"first");
        let second = source.read(&mut out).expect("second read");
        assert_eq!(&out[..second], b"second");
    }

    #[test]
    fn ends_only_when_the_writer_says_so() {
        let dir = std::env::temp_dir().join("refrain-stream-test-eof");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        write(&source, &path, b"tail");

        let mut out = [0u8; 16];
        assert_eq!(source.read(&mut out).expect("read"), 4);

        // Nothing left, and the writer has not finished: the read has to wait
        // rather than report the end of the stream. Finish it from elsewhere
        // and the wait ends.
        let shared = Arc::clone(&source.shared);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            shared.flags.lock().unwrap().done = true;
            shared.ready.notify_all();
        });
        let started = std::time::Instant::now();
        assert_eq!(source.read(&mut out).expect("eof"), 0);
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "returned before the writer finished"
        );
    }

    /// A failed download has to explain itself; rodio's own message for a
    /// broken reader says nothing about the cause.
    #[test]
    fn reports_the_failure_before_anything_else() {
        let dir = std::env::temp_dir().join("refrain-stream-test-error");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        write(&source, &path, b"truncated");

        source.shared.flags.lock().unwrap().error = Some("yt-dlp: video unavailable".to_string());
        let mut out = [0u8; 16];
        let error = source.read(&mut out).expect_err("should fail");
        assert!(error.to_string().contains("video unavailable"));
        assert_eq!(
            source.failure_handle().get().as_deref(),
            Some("yt-dlp: video unavailable")
        );
    }

    #[test]
    fn seeks_forward_and_back() {
        let dir = std::env::temp_dir().join("refrain-stream-test-seek");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        write(&source, &path, b"0123456789");

        let mut out = [0u8; 4];
        source.seek(SeekFrom::Start(4)).expect("seek");
        source.read_exact(&mut out).expect("read");
        assert_eq!(&out, b"4567");

        source.seek(SeekFrom::Current(-2)).expect("seek back");
        source.read_exact(&mut out).expect("read");
        assert_eq!(&out, b"6789");

        source.seek(SeekFrom::End(-1)).expect("seek from end");
        source.read_exact(&mut out[..1]).expect("read");
        assert_eq!(&out[..1], b"9");

        assert!(source.seek(SeekFrom::Start(0)).is_ok());
        assert!(source.seek(SeekFrom::Current(-1)).is_err());
    }

    /// A read that had to wait for the downloader is the moment a dropout would
    /// be heard, so it has to be visible. Sub-millisecond waits are scheduling
    /// noise and must not be counted.
    #[test]
    fn counts_the_reads_that_waited_for_the_downloader() {
        let dir = std::env::temp_dir().join("refrain-stream-test-starve");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        let stats = source.stats_handle();

        // Start the stream first: a wait before anything has played is the
        // stream starting up, and the test below covers that case.
        write(&source, &path, b"first");
        let mut out = [0u8; 8];
        assert_eq!(source.read(&mut out).expect("read"), 5);
        assert_eq!(stats.starvation().waits, 0);

        // Now the download is behind: this read blocks until it catches up.
        let writer_path = path.clone();
        let shared = Arc::clone(&source.shared);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .expect("open");
            file.write_all(b"late").expect("write");
            shared.written.store(9, Ordering::Release);
            shared.ready.notify_all();
        });

        let n = source.read(&mut out).expect("read");
        assert_eq!(&out[..n], b"late");

        let starvation = stats.starvation();
        assert_eq!(starvation.waits, 1, "the wait was not recorded");
        assert!(starvation.waited >= Duration::from_millis(100));
        assert!(starvation.longest >= Duration::from_millis(100));
        assert!(starvation.is_audible());
    }

    /// The wait for the stream to start is not a dropout: nothing has played
    /// yet, so nothing was interrupted. Counting it made a healthy track report
    /// a multi-second dropout it never had.
    #[test]
    fn the_startup_wait_is_not_starvation() {
        let dir = std::env::temp_dir().join("refrain-stream-test-startup");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        let stats = source.stats_handle();

        // Nothing written yet: the very first read blocks until bytes arrive.
        let writer_path = path.clone();
        let shared = Arc::clone(&source.shared);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .expect("open");
            file.write_all(b"start").expect("write");
            shared.written.store(5, Ordering::Release);
            shared.ready.notify_all();
        });

        let mut out = [0u8; 8];
        assert_eq!(source.read(&mut out).expect("read"), 5);
        assert_eq!(
            stats.starvation().waits,
            0,
            "the startup wait was counted as starvation"
        );
    }

    /// Reading what is already there must not look like starvation, or a
    /// healthy stream would report dropouts it never had.
    #[test]
    fn a_read_that_did_not_wait_is_not_starvation() {
        let dir = std::env::temp_dir().join("refrain-stream-test-nostarve");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut source, path) = source(&dir);
        write(&source, &path, b"ready");

        let mut out = [0u8; 8];
        assert_eq!(source.read(&mut out).expect("read"), 5);
        assert_eq!(source.stats_handle().starvation().waits, 0);
    }
}
