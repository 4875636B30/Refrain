//! Thin playback layer over `rodio`.
//!
//! rodio 0.22 replaced `OutputStream`/`Sink` with a `Mixer` + `Player` pair.
//! We keep one output device for the process lifetime and swap out the `Player`
//! for each track.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::ytm::Track;

pub struct AudioEngine {
    /// Held for the lifetime of the engine; dropping it closes the output device.
    device: MixerDeviceSink,
    player: Option<Player>,
    volume: f32,
    pub now_playing: Option<Track>,
    pub total: Option<Duration>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let device = DeviceSinkBuilder::open_default_sink()
            .context("opening the default audio output device")?;
        Ok(Self {
            device,
            player: None,
            volume: 0.8,
            now_playing: None,
            total: None,
        })
    }

    /// Decode `path` and start playing it, replacing any current track.
    pub fn play_file(&mut self, path: &Path, track: Track) -> Result<()> {
        self.stop();

        let file = File::open(path)
            .with_context(|| format!("opening audio file {}", path.display()))?;
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

    /// Stream from a remote source, starting playback as soon as data arrives.
    pub fn play_remote(
        &mut self,
        source: crate::audio::stream::RemoteSource,
        track: Track,
    ) -> Result<()> {
        self.stop();

        let decoder = rodio::Decoder::new(source).context("decoding remote stream")?;
        self.total = decoder.total_duration();

        let player = Player::connect_new(self.device.mixer());
        player.set_volume(self.volume);
        player.append(decoder);
        player.play();
        self.player = Some(player);
        self.now_playing = Some(track);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.total = None;
    }

    pub fn is_playing(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| !p.is_paused() && !p.empty())
            .unwrap_or(false)
    }

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
        self.player.as_ref().map(|p| p.get_pos()).unwrap_or_default()
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
