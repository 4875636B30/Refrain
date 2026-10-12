//! Desktop media controls: MPRIS on Linux, plus the platform equivalents on
//! macOS and Windows.
//!
//! This is what puts the track in the desktop's media widget and lets media
//! keys, panel buttons and `playerctl` drive playback. A machine without a
//! session bus is not an error — the app is perfectly usable without any of
//! this — so every failure here degrades to "no desktop controls" rather than
//! propagating.

use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

/// How far a bare "seek" media key moves, when the desktop doesn't say.
const SEEK_STEP: Duration = Duration::from_secs(5);

/// A request from the desktop's media controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    /// Move relative to the current position.
    SeekBy(i64),
    /// Jump to an absolute position.
    SeekTo(Duration),
    SetVolume(f64),
}

/// Owns the desktop's media-control connection, if there is one.
pub struct Controls {
    inner: Option<MediaControls>,
}

impl Controls {
    /// Connect to the desktop, forwarding requests to `on_command`.
    ///
    /// `hwnd` is the window handle, which Windows requires for the media
    /// transport controls; it is ignored elsewhere. Without one the platform
    /// call fails and the app simply has no media controls, which is why this
    /// is worth plumbing rather than leaving as `None`.
    pub fn new(
        hwnd: Option<*mut std::ffi::c_void>,
        on_command: impl Fn(Command) + Send + 'static,
    ) -> Self {
        let config = PlatformConfig {
            dbus_name: "refrain",
            display_name: "Refrain",
            hwnd,
        };

        let Ok(mut inner) = MediaControls::new(config) else {
            return Self { inner: None };
        };

        let handler = move |event: MediaControlEvent| {
            let command = match event {
                MediaControlEvent::Play => Command::Play,
                MediaControlEvent::Pause => Command::Pause,
                MediaControlEvent::Toggle => Command::Toggle,
                MediaControlEvent::Next => Command::Next,
                MediaControlEvent::Previous => Command::Previous,
                MediaControlEvent::Stop => Command::Stop,
                MediaControlEvent::Seek(direction) => Command::SeekBy(signed(SEEK_STEP, direction)),
                MediaControlEvent::SeekBy(direction, amount) => {
                    Command::SeekBy(signed(amount, direction))
                }
                MediaControlEvent::SetPosition(position) => Command::SeekTo(position.0),
                MediaControlEvent::SetVolume(volume) => Command::SetVolume(volume),
                // Raising the window, opening a URI and quitting are all things
                // the desktop may ask for and this client declines to do.
                MediaControlEvent::OpenUri(_)
                | MediaControlEvent::Raise
                | MediaControlEvent::Quit => return,
            };
            on_command(command);
        };

        if inner.attach(handler).is_err() {
            return Self { inner: None };
        }
        Self { inner: Some(inner) }
    }

    /// Whether the desktop actually accepted us.
    pub fn is_connected(&self) -> bool {
        self.inner.is_some()
    }

    /// Publish the track the desktop should display.
    pub fn set_track(
        &mut self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration: Option<Duration>,
        art_url: Option<&str>,
    ) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let _ = inner.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album,
            cover_url: art_url,
            duration,
        });
    }

    /// Publish whether playback is running, and where it is.
    ///
    /// MPRIS has no "position changed" signal, so clients poll `Position`; this
    /// is called periodically to keep that answer current.
    pub fn set_playback(&mut self, playing: bool, position: Duration) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let progress = Some(MediaPosition(position));
        let playback = if playing {
            MediaPlayback::Playing { progress }
        } else {
            MediaPlayback::Paused { progress }
        };
        let _ = inner.set_playback(playback);
    }

    pub fn set_stopped(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            let _ = inner.set_playback(MediaPlayback::Stopped);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        if let Some(inner) = self.inner.as_mut() {
            let _ = inner.set_volume(f64::from(volume));
        }
    }
}

fn signed(amount: Duration, direction: SeekDirection) -> i64 {
    let amount = amount.as_millis() as i64;
    match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_direction_carries_the_sign() {
        assert_eq!(signed(Duration::from_secs(5), SeekDirection::Forward), 5000);
        assert_eq!(
            signed(Duration::from_secs(5), SeekDirection::Backward),
            -5000
        );
        assert_eq!(signed(Duration::ZERO, SeekDirection::Backward), 0);
    }
}
