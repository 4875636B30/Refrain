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

/// The MPRIS name clients look for. souvlaki builds it as
/// `org.mpris.MediaPlayer2.<dbus_name>`, so the two have to agree.
const MPRIS_NAME: &str = "org.mpris.MediaPlayer2.refrain";
const DBUS_NAME: &str = "refrain";

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
            dbus_name: DBUS_NAME,
            display_name: "Refrain",
            hwnd,
        };

        // souvlaki's `attach` returns before it knows whether the name was free,
        // and panics inside its own thread when it was not — so a second
        // instance printed a panic and then had no media controls at all, while
        // still reporting that it had connected. Ask the bus first and leave the
        // controls to whoever holds them.
        if mpris_name_is_taken(MPRIS_NAME) {
            return Self { inner: None };
        }

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

    /// Publish the volume to the desktop, where the desktop has such a thing.
    ///
    /// MPRIS exposes volume as a property the desktop can also set, and
    /// souvlaki implements it only there: its macOS and Windows backends have
    /// `new`, `attach`, `set_metadata` and `set_playback` and nothing else. The
    /// condition below is souvlaki's own, since that is what decides whether
    /// the method exists — and on the platforms without it, the app's volume is
    /// the only volume there is.
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    pub fn set_volume(&mut self, volume: f32) {
        if let Some(inner) = self.inner.as_mut() {
            let _ = inner.set_volume(f64::from(volume));
        }
    }

    #[cfg(not(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    )))]
    pub fn set_volume(&mut self, _volume: f32) {}
}

fn signed(amount: Duration, direction: SeekDirection) -> i64 {
    let amount = amount.as_millis() as i64;
    match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    }
}

/// Whether another instance already holds the MPRIS name.
///
/// `GetNameOwner` fails when nobody owns the name, which is the answer we want —
/// so anything other than a successful reply reads as "not taken", and the app
/// tries as it always did. Not being able to ask at all (no session bus) is the
/// same: souvlaki will fail to connect and that is already handled.
#[cfg(target_os = "linux")]
fn mpris_name_is_taken(name: &str) -> bool {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return false;
    };
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(name,),
        )
        .is_ok()
}

/// MPRIS is Linux's; elsewhere souvlaki uses a platform backend with no name to
/// collide over.
#[cfg(not(target_os = "linux"))]
fn mpris_name_is_taken(_name: &str) -> bool {
    false
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
