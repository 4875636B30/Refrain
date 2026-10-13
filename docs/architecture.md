# How Refrain works

Notes on the design, and on the parts that turned out to be less obvious than
they looked. The [README](../README.md) covers installing and using it.

## Locale

InnerTube takes the language and region as `hl`/`gl`, which are taken from the
POSIX locale environment. Both are *validated* rather than passed through:
`LANG=C` — a minimal container, a CI runner, a script that sets `LC_ALL=C` —
parses to a "language" of `C`, and YouTube answers `hl=C` with HTTP 400
"Request contains an invalid argument", which breaks search and browsing
entirely. `C` and `POSIX` are exactly the locales with no language code to send,
so anything that is not two or three letters falls back to `en`/`US`.

The daily canary found this, in CI, on the first tagged release — which is
precisely the kind of environment-dependent breakage it exists for.

## Talking to YouTube Music

YouTube Music has no public API. Like other third-party clients, Refrain talks to
the same internal **InnerTube** endpoints (`music.youtube.com/youtubei/v1/*`) the
web app uses, presenting a spoofed `WEB_REMIX` client identity.

```
src/
  main.rs              eframe bootstrap + the `--probe`/`--page`/`--stream`/… diagnostics
  app.rs               app state, background tasks, egui UI
  config.rs            ~/.config/refrain/config.toml
  auth.rs              cookie extraction, SAPISIDHASH, browser auto-detect
  media.rs             desktop media controls (MPRIS et al) via souvlaki
  thumbs.rs            artwork fetch (reqwest) + decode (image)
  ui.rs                theme + reusable widgets (cover grid, list rows, transport)
  ytm/
    client.rs          InnerTube HTTP client (search / browse / like / playlist edits)
    extract.rs         tolerant renderer-tree walking + parsing
    domain.rs          clean domain types (Track, Page, SearchItem, LikeStatus)
  audio/
    resolver.rs        yt-dlp: stream args, URL resolution, ranged fetches, cache budget
    stream.rs          yt-dlp stdout + seekable buffer (streaming playback)
    engine.rs          rodio playback (play/pause/seek/volume)
```

### Parsing strategy

YouTube's response JSON is deeply nested and gets reshuffled regularly. Rather
than modelling every layer with `serde` structs — which breaks on every change —
the parser walks the tree looking for renderer names it recognises
(`musicResponsiveListItemRenderer`, `musicResponsiveHeaderRenderer`, …). Unknown
wrappers are simply skipped, so the client keeps working when the layout shifts.

The cost of that tolerance is a quiet failure mode: if a renderer is renamed or
a field moves, extraction returns nothing and the app shows an empty shelf
rather than an error. So the parsing is tested against **captured responses** —
real `search` and `browse` payloads in `tests/fixtures/`, gzipped because a
genuine response is mostly thumbnail URLs. The tests assert the contract the app
depends on rather than exact content: counts are floors, titles are non-empty,
ids look like ids. That way re-capturing does not break them unless the shape
really changed, which is the signal worth having.

`tools/capture-fixtures.py` re-captures them; run it when a fixture test fails,
and read the diff before committing. The fixtures are anonymous responses, so
they carry nobody's library.

### Paging

Feeds, long playlists and expanded search categories all page, but not
identically, and one of them looks like it pages when it does not:

- **Feeds and long playlists** use `nextContinuationData` from the browse
  response, and the playlist case needs an anchor track (the offset is ignored).
- **A single-category search** offers a real `nextContinuationData` token worth
  20 more rows.
- **A grouped "All" search** offers only `reloadContinuationData`, which looks
  like a paging token and is not: sending it back returns an empty page. Its
  results are already complete. So "Load more results" appears in the expanded
  view only, and an empty page reports "no more results" rather than appearing
  to ignore the click.

### Feeds

The Home and Explore feeds only return a handful of shelves in the first
response; the rest arrive as you scroll, via continuation tokens. The client
pages those in automatically (up to a few rounds), so Home ends up with the
shelves the real YouTube Music home shows rather than just the first three.

Long playlists have a related wrinkle: `browse` caps at 100 tracks, and the
remainder only come from the `next` endpoint anchored on the last known track
(the offset is ignored). That is what "Load more tracks" does.

## Playback

Pressing play runs `yt-dlp -f bestaudio[ext=m4a] -o -` and reads the media from
its **stdout**. `RemoteSource` presents that pipe to `rodio` as a `Read + Seek`
by buffering from offset 0 in a background thread, so playback starts as soon as
the first bytes arrive instead of after a full download — and seeking works even
though a pipe isn't seekable. That pipe is also the safety net: if anything in
the faster path below fails, playback still works.

### Resolving is yt-dlp's job; fetching is ours

A `googlevideo` URL cannot be had without solving YouTube's JS signature
challenge, so `yt-dlp -g` does the resolving (about 6 s). Fetching that URL,
though, turned out not to be the fragile thing it first looked like:

- **Fetch it with an explicit `Range` header.** Without one, YouTube throttles
  the *same* URL to ~33 KB/s — which presents as a stall or a 403, and is what
  made direct fetching look unreliable for a long time. With one, a whole track
  arrives in about a second at 5–9 MB/s. Measured: `no range` 79 s, bounded range
  0.33 s. Ranges are always *bounded*; an open-ended `bytes=0-` is refused
  outright for some agent/route combinations.
- Any user agent works. The request is not bound to the client that resolved it.
- Resolved URLs are reused for ~5 h, so fetching a track again skips `yt-dlp`.
- Audio is mirrored into `~/.cache/refrain/audio/<video_id>.m4a`, so repeats play
  straight from disk.

### Keeping resolution off the critical path

- Opening a page resolves and caches the first several tracks in the background;
  hovering a track for ~150 ms warms it, and starting a track queues the next few.
- Streams are additionally opened **ahead of time**, so pressing play starts in
  milliseconds rather than waiting on `yt-dlp`.
- The two tracks you play most are warmed at startup, ranked by local play counts
  (`~/.config/refrain/plays.tsv`), falling back to your YouTube Music history.
- The warm pool runs **two** tracks at a time. Three measured 1.67x against 1.58x
  for two, which is not worth a third concurrent `node` — those processes are
  CPU-heavy enough to starve the audio reader thread, and a buffer underrun is
  much more noticeable than warming the next track a second later.
- Batching several tracks into one `yt-dlp` invocation was measured and rejected:
  it saves only ~15%, because the marginal cost per track is the per-video JS
  challenge, not process startup.

The one unavoidable cost is the first `yt-dlp` extraction for a track (~6 s:
~1.6 s webpage, ~2 s solving YouTube's JS challenges, ~1.5 s startup). Prefetching
keeps it off the critical path, so it only shows up if you click a track nothing
has warmed.

### Why the cipher isn't reimplemented

Getting the URL without `yt-dlp` would mean reimplementing YouTube's JS
signature deciphering, and that was investigated rather than assumed:

- `sig` **is** crackable in pure Rust. It is not cryptography: YouTube ships a
  character-shuffled base64 signature, and "deciphering" is a table lookup. The
  permutation can be recovered by feeding `chr(0)..chr(n)` through the player JS
  once, and applying it reproduces a signature with the same video-specific
  header as a genuine one.
- But `n` is not, and every client that returns a usable format requires it.
  `n` is a per-request transform with a random input, so it cannot be cached —
  it needs real JS execution on every resolve.
- The machinery for that is now a hard dependency of `yt-dlp` itself
  (`yt-dlp-ejs`, ~150 KB of minified JS run under node/bun/deno/quickjs). So
  "drop `yt-dlp`" means shipping a JS engine *and* that blob, or reimplementing
  it — strictly worse, and it would break on YouTube's schedule rather than
  yt-dlp's.

### Cache budget

Cached audio is capped at 2 GB, evicted oldest first, both at startup and at the
quiet moment after the warm queue drains. Anything touched in the last ten
minutes is never evicted: it may be the track that is playing, one being warmed,
or a fetch still in flight, and the cache would rather sit briefly over budget
than delete something in use.

## Failure messages

yt-dlp's stderr is written for developers, and several of its failures have a
specific fix worth naming. `explain_failure` maps the recognisable ones — a
missing JS runtime, a bot check, a region block, an age gate, no playable format,
no network — to a sentence that says what to do, falling back to yt-dlp's own
last `ERROR:` line for anything unrecognised. The mapping is a pure function with
tests, and the ordering matters: an age-restricted message also says "sign in to
confirm", so the specific case has to be recognised first.

A failed *stream* needed more than a mapping. rodio reports a reader that errors
as "the format of the data has not been recognized", which hides the cause
entirely, so the downloader's stderr is drained on its own thread and the
decision to report a failure is made *before* the stream is marked done — a
reader that sees EOF first will surface rodio's message instead. `StreamFailure`
exists so a caller can ask afterwards why it failed, once the decoder has taken
ownership of the source.

## The demo mode

`--demo` drives the app through a fixed sequence: search for a well-known
artist, open the first album it finds, play it, open the queue. It exists for
two reasons — a screenshot or GIF can be reproduced on demand rather than
hand-assembled, and the screenshot in the README does not have to be anyone's
home feed, which is personalised and therefore publishes their taste.

Two details it learned the hard way. The steps are gated on **conditions**, not
a timer: authentication, a search and a page load all take a variable amount of
time, and the first version fired "open an album" before the search had
returned, so the demo never left the search view. And the first step waits for
sign-in to *settle* — signing in re-runs the last search, which yanks the view
back if the script has already navigated away.

## Memory

Measured on a release build with the home feed loaded: **~205 MB resident**,
steady rather than creeping. Of that, roughly 83 MB is *shared* — the NVIDIA
driver and `libLLVM`, mapped by every process that touches them — and the bulk
of the rest is the GL driver's own private allocations. The app's own data is
tens of megabytes, not hundreds.

### Artwork is fetched only for what is on screen

`ensure_thumbnails` used to scan the whole view, so a feed of fifteen shelves
queued artwork for hundreds of items nobody had scrolled to. It now fetches what
was actually *drawn*, which the widgets report as they draw it — a placeholder
counts, which is what bootstraps the very first frame, when nothing has been
drawn and so nothing is visible yet.

Measured on the home feed: **212 → 20 images requested**, and textures held
159 → 20 (about 14 MB of pixel data). Resident memory only moved 212 → 206 MB,
because the driver dominates the total and glibc keeps freed pages in its arena
rather than returning them to the OS — so the win here is requests, data and
decode work, not the headline number. Worth knowing before promising a memory
saving from it.

A side effect that is a genuine improvement: the eviction guard now protects
only artwork that is visible, instead of everything in the view, so scrolling
away from a shelf lets its textures go.

## Authentication

Cookies are reused for two independent jobs:

- **Playback** — forwarded straight to `yt-dlp` (`--cookies-from-browser` /
  `--cookies`) to get past YouTube's bot check.
- **Catalogue** — sent to InnerTube with a `SAPISIDHASH` `Authorization` header,
  which is what makes your library visible.

To avoid re-implementing browser cookie decryption, Refrain drives
`yt-dlp --cookies-from-browser` to materialise a `cookies.txt`, then parses it.
Both jobs share that one extraction.

On Linux, Chromium-family cookies are encrypted with a key held in the OS
keyring, and yt-dlp guesses which keyring from `XDG_CURRENT_DESKTOP`. That guess
is unreliable — `BASICTEXT` in particular silently fails to decrypt the `v11`
cookies (you get a handful of cookies and no `SAPISID`). So Refrain tries
`GNOMEKEYRING`, `KWALLET6`, `KWALLET` and `BASICTEXT` explicitly, keeps whichever
yields a `SAPISID`, and remembers it. Pin one with the `keyring` config option if
you prefer.

Extraction takes ~10 s of `yt-dlp`, so the dumped jar is **cached** and reused for
45 minutes, and InnerTube verifies the cached session still works — a rejected jar
triggers a fresh extraction. That makes sign-in ready in well under a second on
subsequent launches.

When several browsers are present, `auto` picks the first one that actually has a
**YouTube session**, so a logged-out Firefox won't shadow a signed-in Brave.

## Playlist editing

Adding to a playlist, creating, renaming, deleting and removing tracks all go
through InnerTube's `browse/edit_playlist` (plus `playlist/create` and
`playlist/delete`). Two details cost real debugging:

- **Ids.** Playlists arrive as browse ids (`VLPL…`) but those endpoints want the
  bare id (`PL…`), and sending the wrong one is a *silent no-op*. The prefix is
  stripped in one place, with tests.
- **Removing needs the entry id, not the video id.** The same track can appear in
  a playlist more than once, so entries have their own id — the row's
  `playlistItemData.playlistSetVideoId` — and `ACTION_REMOVE_VIDEO` is rejected
  with a bare 400 without it. `Track` carries it as `set_video_id`.

Which playlists offer the destructive action is decided by whether the header's
facepile links to a `MUSIC_PAGE_TYPE_USER_CHANNEL`: editorial playlists have no
linked owner. That distinguishes *a person's* playlist from YouTube Music's, but
not *your* playlist from someone else's — a Google account can control several
channels and the playlist's owner need not be the account's active one, so
comparing against the signed-in channel does not work. The server stays the real
authority; a refusal is shown rather than swallowed.

## Platforms

The same code builds for Linux, macOS and Windows; CI builds and tests all
three, because several pieces cannot be exercised on one of them.

- **Finding `yt-dlp`** looks for the platform's name beside the executable
  (`yt-dlp_linux`, `yt-dlp_macos`, `yt-dlp.exe`), and a binary on `PATH` is
  looked up *with* the platform's suffix — on Windows the file is `node.exe`,
  and looking for `node` finds nothing, which would silently leave yt-dlp
  without a JavaScript runtime.
- **Browser profiles** are discovered explicitly on Linux only. Elsewhere the
  browser *name* is handed to yt-dlp and it finds the profile itself: it knows
  those layouts and keeps them current, and the keyring quirk that makes an
  explicit path necessary is a Linux one. `BROWSER+KEYRING` is likewise only
  built on Linux, where yt-dlp uses it.
- **Media controls** go through souvlaki, which needs a window handle on
  Windows for the system transport controls — and only there. It is taken from
  eframe's `CreationContext` and passed in; without one the platform call fails
  and the app simply has no media controls.
- **The Windows build has no console** in release (`windows_subsystem`), so no
  black window opens beside the GUI. Debug builds keep it, because that is where
  the diagnostics go.

None of this is verified by running the app on those platforms — only by CI
building and testing them. macOS and Windows runtime behaviour is the part to
be suspicious of.

## Typography

Roboto is bundled (`assets/fonts/`, Apache-2.0) rather than taken from the
system, so the app looks the same on every desktop — the default egui stack is
whatever the distribution happened to install.

Two faces are shipped because YouTube Music uses both: Roboto Regular for body
text and Roboto Medium for headings. egui has no concept of font weight, so the
heavier face is registered as its own family (`roboto-medium`) and headings ask
for it explicitly through `ui::title`.

Fonts are installed during construction, not on the first frame, and that is
load-bearing rather than stylistic: `Context::set_fonts` only takes effect on the
*next* frame, and asking for a family that is not bound yet is a panic in egui,
not a fallback. Installing them on the first frame meant any heading drawn in
that same frame — a dialog opened at startup, say — took the process down. It
survived normal startup only because the first frame happens to draw no
headings.

Roboto goes *first* in the proportional family and egui's own stack stays behind
it. That stack carries the emoji and the transport glyphs the UI draws — and, as
a lesson from a bug where the repeat button looked dead, a glyph that is missing
from every font in the chain renders as nothing at all.

## Artwork

Thumbnails are fetched with `reqwest` (the same stack the API client uses) and
uploaded to egui as textures, downscaled to 160 px. egui's built-in HTTP loader
is *not* used — the `ehttp` backend it pulls in failed to fetch these URLs.

## Desktop integration

Media keys, the panel's media widget and `playerctl` work through MPRIS, via
`souvlaki` with its `zbus` backend — zbus is pure Rust, so packaging needs no
libdbus headers. Requests arrive on the same channel as every other input and are
handled on the UI thread, so there is no shared mutable state to get wrong. The
same code drives the platform equivalents on macOS and Windows.

MPRIS has no position-changed signal, so position is refreshed once a second.
`LoopStatus` and `Shuffle` are not exposed — souvlaki doesn't implement those
optional properties, so repeat and shuffle remain app-only.

## The JavaScript runtime

yt-dlp cannot solve YouTube's challenges without one, and the failure without it
is a challenge error that says nothing about the cause. It accepts `deno`, `node`,
`quickjs` and `bun`, and the client looks for them in that order — except that a
runtime **beside its own executable wins over anything on `PATH`**.

That lookup order is what makes a bundle self-sufficient without bundling Node:
QuickJS is about 2 MB against Node's 50, and every release carries one —
quickjs-ng publishes prebuilt binaries for all three platforms, so it is a
download rather than a build. The Flatpak manifest builds it from source for the
same reason. It is also why the runtime is worth *naming* in diagnostics:
`--canary` reports which one was found, and fails when there is none, which is
what keeps a release from shipping without one.

## Lyrics

Two requests, and neither is the one you would expect:

1. The watch-next response — the same one the like status comes from — lists
   the player's tabs. A track with lyrics has a "Lyrics" tab whose endpoint is
   a browse with `MUSIC_PAGE_TYPE_TRACK_LYRICS`; a track without has no such
   tab, which is the usual case for instrumentals.
2. Following that browse id returns a `musicDescriptionShelfRenderer` holding
   the words as text runs, newlines and all, plus a footer crediting the
   source.

The catch: **there is no timing in either response.** YouTube Music's web
client shows synced lyrics, but it does not get them from here, and nothing in
this API surface carries them. So the panel shows the words as words. Anything
else would be inventing a sync.

One subtlety worth keeping: an album's Wikipedia summary is *also* a
`musicDescriptionShelfRenderer`, so `parse_lyrics` picks the shelf by shape —
the collapsible one with a source credit, not the one with a "more" button.
Reading the wrong one shows an album blurb as the words of a song, which the
fixture test for it catches.

## Updating itself

Settings checks GitHub for a newer release — once a day automatically, and
whenever the button is pressed — and offers to install it. Two decisions shape
that:

- **A checksum is required.** Every asset is published with a `.sha256` beside
  it, and a download that does not match is refused rather than installed. An
  updater that installs an unverified binary is a worse idea than no updater,
  so a release without a checksum is downloaded and left for the user instead.
- **Only where the app owns the files.** An AppImage is its own installation, so
  it is replaced in place; a tarball unpacked by hand is the same, and the tools
  beside the executable are replaced with it. A copy under `/usr`, `/app` or
  `/nix` belongs to a package manager and is left alone — two owners of one file
  is how installations rot — and a development build is never replaced at all.

Replacing means writing a temporary file in the same directory and renaming it
over the target, so an interrupted write leaves the old file in place rather
than half a new one. The running process keeps the old inode, which is why the
card says to restart; the Restart button starts the new build and steps aside.

## Packaging

The binary looks for `yt-dlp` in this order, which makes a self-contained
distribution possible:

1. `$YTDLP_PATH` — an instruction rather than a guess, so it wins outright
2. **next to the executable** (ship `yt-dlp_linux` beside the binary)
3. `~/.local/bin/yt-dlp`
4. anywhere on `$PATH`

The first three are *candidates*, and the one written most recently wins, with
that order breaking ties. This matters more than it looks: the bundle ships a
copy frozen at release time, and yt-dlp is exactly the part that breaks when
YouTube changes — so preferring the bundled copy unconditionally would leave a
released build unable to play anything until the app itself was updated.
Freshness is judged by mtime, which costs a `stat`; running `--version` instead
would add over a second per candidate to every startup.

Settings shows which copy was chosen, its version and how old it is, and warns
past 90 days — so a stale copy is visible rather than mysterious.

A release bundle is therefore the binary plus `yt-dlp_linux`, plus `ffmpeg`
(needed only for the download fallback).

A second build step wraps the same pieces in an **AppImage**: an `AppDir` with
`usr/bin/refrain` and `usr/bin/yt-dlp_linux`, a desktop file, an icon and an
`AppRun` symlink, packed by `appimagetool`. Inside the image the binary runs
from a temporary mount point, so `beside_executable()` resolves to
`<mount>/usr/bin` and the bundled `yt-dlp` is found — verified by running the
image's own `--canary`, which reports the path it used.

It is an AppImage rather than a Flatpak for a specific reason: yt-dlp needs a
JavaScript runtime to solve YouTube's challenges, and a sandbox would need one
*inside* it — which means bundling a whole Node. An AppImage has no such
problem, because it can use the host's.

Package-manager templates live in `packaging/`:

- `packaging/aur/` — a `refrain-music-bin` PKGBUILD, which installs the release tarball
  and depends on the distribution's `yt-dlp` rather than shipping the bundle's
  (the packaged copy is updated far more often, and the client prefers whichever
  is newer). The AUR is a git repository per package, so this is a template to
  copy; nobody but the maintainer can push it.
- `packaging/flathub/` — a manifest and the vendored crates Flathub needs for an
  offline build. Neither has been built in a Flatpak: there is no SDK on the
  machine this was written on, and Flathub's CI is the real test. The open
  questions — sandboxed sign-in, and a pinned `yt-dlp` that ages — are written
  down in `packaging/flathub/README.md`.

Tagging a commit (`git tag v0.1.1 && git push --tags`) runs
`.github/workflows/release.yml`, which builds with `--locked`, fetches the current
`yt-dlp_linux`, assembles the bundle, checks that the unpacked binary still
resolves a track, and attaches it to a GitHub release. The bundle is about 50 MB,
most of it `yt-dlp`.

```
refrain-<version>-linux-x86_64/
  refrain              the app
  yt-dlp_linux         found next to the binary
  refrain.desktop      launcher entry
  refrain.svg          icon
  refrain.metainfo.xml AppStream metadata
  README.md  LICENSE
```

`packaging/refrain.metainfo.xml` carries placeholder `example` URLs and a
placeholder reverse-DNS id — replace both with the project's real home before
publishing. `desktop-file-validate` and `appstreamcli validate` both pass as they
stand.

## Configuration

`~/.config/refrain/config.toml` (created once you change a setting):

```toml
[auth]
# auto   = use the first signed-in browser detected (default)
# browser = force a browser source
# file   = read an exported cookies.txt
# none   = never send cookies
mode = "auto"
browser = "firefox"          # firefox|chrome|chromium|brave|edge|opera|vivaldi|whale
keyring = ""                 # BASICTEXT|GNOMEKEYRING|KWALLET|KWALLET5|KWALLET6
profile = ""                 # explicit profile name or path
cookies_file = ""            # used when mode = "file"

[ui]
volume = 0.8
```

`auto` looks in both native and **Flatpak** locations, including Firefox's modern
XDG path (`~/.config/mozilla/firefox`) and Flatpak data dirs such as
`~/.var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser`.

Environment variables still work as a fallback: `YTDLP_COOKIES`,
`YTDLP_COOKIES_FROM_BROWSER`.

## Diagnostics

Everything below runs without a GUI, which is what makes them useful on a
machine with no display:

```sh
cargo run -- --demo                   # scripted run, for screenshots and GIFs
cargo run -- --canary                 # check the live API and yt-dlp still work
cargo run -- --auth-check             # sign-in diagnostics
cargo run -- --probe "daft punk"      # exercise search + browse
cargo run -- --page FEmusic_home      # dump a page's tracks and shelves
cargo run -- --stream <video_id>      # stream + report time-to-first-audio
cargo run -- --download <video_id>    # resolve, cache, and decode one track
cargo run -- --decode <path>          # decode a local audio file
cargo run -- --thumb <url>            # verify the artwork pipeline
cargo run -- --like-status <video_id> # read a track's rating (read-only)
```

If `--auth-check` reports `anonymous ❌`, open <https://music.youtube.com> in the
browser you're extracting from, sign in, confirm you can see your account, then
re-run. A Google login alone is not enough — InnerTube needs a real **YouTube**
session (`LOGIN_INFO`).

To force a fresh cookie extraction:

```sh
cargo run -- --auth-check --refresh
```
