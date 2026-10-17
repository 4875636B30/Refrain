# Changelog

What changed, release by release. Each version is a git tag with a matching
[release](../../releases) carrying the binaries.

The 0.1.x series was written and released inside a week, so its dates are
clustered; this file exists so the shape of that week is readable in one place.
Entries describe what a user would notice, not every commit.

## 0.2.0 — 2026-10-17

**Offline.** The cache was invisible. It is files named after video ids, so
nothing could say what was in it, there was no way to keep something
deliberately, and no way to remove it. There is now a Downloads view listing what
is cached with sizes and a total, a "keep offline" flag the eviction planner
respects, and rows that play straight from disk — so it works with no network at
all.

Four fixes alongside it, all found by using the thing:

- **Settings could not be closed at a small window size.** The panel grew to fit
  its cards, and since egui draws it inside the app window, a panel taller than
  that window had its close button drawn off the top edge.
- **A second copy of the app panicked.** souvlaki's MPRIS backend panics in its
  own thread when the D-Bus name is taken, so a second instance printed a panic
  and then had no media controls while reporting that it had connected. It now
  leaves the controls to whoever holds them.
- **Killed runs left whole tracks in the cache.** Scratch files are renamed into
  place on success and deleted on failure, so anything left is from a run that
  was killed — and the cleaner only knew about one of the three shapes. One
  machine had 40 MB of audio nobody could play, invisible to the eviction planner
  because it only looks at `.m4a`.
- **Signing in was not noticed.** A cached cookie jar with no YouTube session was
  reused as-is, so a browser signed in since the last extraction went unnoticed
  until the cache expired. It now searches only when the browser's cookie store
  has actually been written since, which is both the correct test and a much
  cheaper one.

## 0.1.19 — 2026-10-14

Signing in is picked up again: the fix above, on its own.

## 0.1.18 — 2026-10-14

The queue can be edited — play next, add to queue, move an entry up or down,
remove one — where it used to be a list you could only read. Starting up no
longer re-extracts browser cookies when there is no YouTube session to find,
which was costing 12 seconds before the first frame.

## 0.1.17 — 2026-10-13

Playback dropouts are reported rather than handled in silence: the buffer
underruns the output device reports, and the reads that had to wait for the
downloader. Settings shows both, and `--underrun-test <id>` plays a track aloud
and prints the same numbers.

## 0.1.16 — 2026-10-13

An AppImage install fetches the AppImage. It used to fetch the tarball and put it
in the AppImage's place, leaving a file that could not be executed. A download
whose contents do not match the kind of installation is now refused before
anything is replaced, and restarting no longer freezes the window while it
watches the new build start.

## 0.1.15 — 2026-10-13

Scrolling the feed has momentum: a flick glides to rest instead of stopping the
instant the wheel does.

## 0.1.14 — 2026-10-13

Streaming playback buffers to disk rather than to memory, so a track that plays
for four minutes no longer sits in RAM while it does — about 8 MB while something
is playing.

## 0.1.13 — 2026-10-13

Updates download to disk instead of into memory: a 52 MB release peaked at 125 MB
and now peaks at 19. Artwork is decoded at the size it is drawn rather than
always at the largest, which cuts the memory a row of cover art costs by six.

## 0.1.12 — 2026-10-13

The playing row's equaliser animates at the frame rate the platform delivers
rather than the four frames a second the rest of the player bar needs. `--fps`
reports that rate, for when an animation looks wrong and it matters whether the
app or the display is to blame.

## 0.1.11 — 2026-10-13

When a queue or an album ends, playback continues with related tracks from the
track's radio, the way YouTube Music does. A setting turns it off.

## 0.1.10 — 2026-10-13

Restarting after an update starts the new build without the environment the old
one was running in, which could stop it coming up — and if it fails, the reason
is shown rather than left in a corner.

## 0.1.9 — 2026-10-13

Lyrics for the track that is playing, in a tab beside the queue. Plain text:
YouTube's responses carry no timing, so there is nothing to follow along with.

## 0.1.8 — 2026-10-13

The category selector is its own control beside the search field again, rather
than a chip inside it.

## 0.1.7 — 2026-10-13

Settings checks for a newer release and installs it in place where that is safe:
an AppImage, or a tarball unpacked by hand. A copy owned by a package manager is
left to its package manager. Downloads are checked against the published
checksum before anything is replaced.

## 0.1.6 — 2026-10-13

Playback keeps the queue when the audio device is rebuilt. It used to resume the
current track but forget everything after it, so playback stopped for good when
that track ended — after a suspend, a headset switch, or the sound server
restarting.

## 0.1.5 — 2026-10-13

The volume is saved when you change it, rather than only when something else
happened to save the config. The window carries its icon on Windows and macOS,
and the desktop entry matches the window on Linux.

## 0.1.4 — 2026-10-13

The search field and the category picker are one control, so they can no longer
overlap. Settings is grouped into sections, with the JavaScript runtime reported
alongside yt-dlp and a meter for the audio cache.

## 0.1.3 — 2026-10-13

Corrects the application ID in the AppStream metadata: a component beginning with
a digit needs a leading underscore, and without one the ID is not a valid Flatpak
application name. The scripted demo no longer stalls waiting for a repaint.

## 0.1.2 — 2026-10-13

Downloads carry QuickJS, so playback no longer needs a JavaScript runtime
installed. A cookies file can be picked in the app, which is how signing in works
where reading a browser's store is not possible.

## 0.1.1 — 2026-10-12

Packaged as an AppImage as well as a tarball. Artwork is fetched only for what is
on screen, and sign-in can be configured in the app rather than in the config
file.

## 0.1.0 — 2026-10-11

First release.
