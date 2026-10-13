//! Thin playback layer over `rodio`.
//!
//! rodio 0.22 replaced `OutputStream`/`Sink` with a `Mixer` + `Player` pair.
//! We keep one output device for the process lifetime and swap out the `Player`
//! for each track.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::audio::stream::{PreparedStream, Starvation, StreamStats};
use crate::ytm::Track;

pub struct AudioEngine {
    /// Held for the lifetime of the engine; dropping it closes the output device.
    device: MixerDeviceSink,
    player: Option<Player>,
    volume: f32,
    pub now_playing: Option<Track>,
    pub total: Option<Duration>,
    /// Read statistics for the stream that is playing, if it came from the
    /// network rather than a cached file.
    stream: Option<StreamStats>,
    /// Set by the device's error callback (e.g. ALSA `POLLERR`).
    device_error: Arc<Mutex<Option<String>>>,
    /// Buffer underruns the device has reported since this engine was built.
    xruns: Arc<AtomicU64>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let device_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&device_error);
        let xruns: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let xrun_count = Arc::clone(&xruns);

        // Take over the error callback so device failures don't spam the log;
        // the app polls `take_device_error` and can rebuild the stream.
        let mut device = DeviceSinkBuilder::from_default_device()
            .context("finding the default audio output device")?
            .with_error_callback(move |error| {
                let text = format!("{error}");
                // Counted here rather than where the message is read: the slot
                // below keeps only the first pending error, so a burst of XRuns
                // would be undercounted by whoever polls it.
                if AudioEngine::is_underrun(&text) {
                    xrun_count.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(mut slot) = slot.lock()
                    && slot.is_none()
                {
                    *slot = Some(text);
                }
            })
            .open_stream()
            .context("opening the default audio output device")?;
        device.log_on_drop(false);

        Ok(Self {
            device,
            player: None,
            volume: 0.8,
            now_playing: None,
            total: None,
            stream: None,
            device_error,
            xruns,
        })
    }

    /// Take a pending output-device error, if any.
    pub fn take_device_error(&self) -> Option<String> {
        self.device_error.lock().ok()?.take()
    }

    /// How many buffer underruns the output device has reported.
    pub fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    /// Whether an error message describes a transient glitch (a buffer
    /// underrun) rather than the device going away.
    ///
    /// ALSA reports an XRun two ways: rodio's own "Buffer underrun/overrun
    /// occurred." and `poll()` on the PCM returning `POLLERR`. They arrive as
    /// separate callbacks for the same event, so treating only the first as
    /// transient meant the other looked like a dead device and rebuilt the
    /// output stream — interrupting playback to fix a glitch that had already
    /// passed.
    pub fn is_underrun(message: &str) -> bool {
        let lower = message.to_lowercase();
        lower.contains("underrun") || lower.contains("overrun") || lower.contains("pollerr")
    }

    /// Decode `path` and start playing it, replacing any current track.
    pub fn play_file(&mut self, path: &Path, track: Track) -> Result<()> {
        self.stop();
        // A cached file cannot starve, so drop the previous stream's numbers
        // rather than leave them looking like this track's.
        self.stream = None;

        let file =
            File::open(path).with_context(|| format!("opening audio file {}", path.display()))?;
        let decoder = rodio::Decoder::new(BufReader::new(file))
            .context("decoding audio (is ffmpeg available for m4a?)")?;

        self.total = decoder.total_duration();

        let player = Player::connect_new(self.device.mixer());
        player.set_volume(self.volume);
        player.append(decoder);
        player.play();
        self.player = Some(player);
        self.now_playing = Some(track);
        Ok(())
    }

    /// Play an already-opened stream (see [`crate::audio::stream::RemoteSource`]).
    pub fn play_decoder(&mut self, prepared: PreparedStream, track: Track) -> Result<()> {
        self.stop();
        let PreparedStream { decoder, stats } = prepared;
        self.total = decoder.total_duration();

        let player = Player::connect_new(self.device.mixer());
        player.set_volume(self.volume);
        player.append(decoder);
        player.play();
        self.player = Some(player);
        self.stream = Some(stats);
        self.now_playing = Some(track);
        Ok(())
    }

    /// Read statistics for the current stream, or `None` when what is playing
    /// came from a cached file rather than the network.
    pub fn stream_starvation(&self) -> Option<Starvation> {
        self.stream.as_ref().map(StreamStats::starvation)
    }

    pub fn stop(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.total = None;
    }

    /// Whether audio is coming out right now: a track is loaded, it is not
    /// paused, and it has not reached its end.
    ///
    /// `Player::empty()` looks like the wrong question — it asks how many
    /// sources are queued, which for a single appended source might mean
    /// "already taken by the audio thread". It does not: rodio wraps every
    /// appended source in `Done`, which decrements that count when the source
    /// *finishes*. So `empty()` is true exactly when nothing is still playing,
    /// and the three states agree with each other.
    pub fn is_playing(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| !p.is_paused() && !p.empty())
            .unwrap_or(false)
    }

    /// Whether playback is held, whatever is loaded. Kept separate from
    /// [`Self::is_playing`] because a paused track is still the current one —
    /// the row keeps its marker and the bars simply stop moving.
    pub fn is_paused(&self) -> bool {
        self.player.as_ref().map(|p| p.is_paused()).unwrap_or(false)
    }

    /// True once a track has been loaded and has finished playing.
    pub fn finished(&self) -> bool {
        self.player.as_ref().map(|p| p.empty()).unwrap_or(false)
    }

    pub fn toggle_pause(&self) {
        if let Some(player) = &self.player {
            if player.is_paused() {
                player.play();
            } else {
                player.pause();
            }
        }
    }

    pub fn position(&self) -> Duration {
        self.player
            .as_ref()
            .map(|p| p.get_pos())
            .unwrap_or_default()
    }

    pub fn seek(&self, pos: Duration) {
        if let Some(player) = &self.player {
            let _ = player.try_seek(pos);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(player) = &self.player {
            player.set_volume(self.volume);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AudioEngine;

    /// ALSA reports a single XRun twice: as rodio's underrun message and as
    /// `POLLERR` from `poll()` on the PCM. Both must read as transient, or the
    /// second one looks like a dead device and rebuilds the output stream
    /// mid-track — interrupting playback to "fix" a glitch that already passed.
    #[test]
    fn both_reports_of_an_xrun_are_transient() {
        assert!(AudioEngine::is_underrun(
            "Buffer underrun/overrun occurred."
        ));
        assert!(AudioEngine::is_underrun(
            "A backend-specific error has occurred: `alsa::poll()` returned POLLERR"
        ));
    }

    /// A device that has genuinely gone away is not a glitch, and must still
    /// take the recovery path.
    #[test]
    fn a_missing_device_is_not_transient() {
        assert!(!AudioEngine::is_underrun("No such device"));
        assert!(!AudioEngine::is_underrun("Device is suspended"));
    }
}
