# How Refrain works

Notes on the design, and on the parts that turned out to be less obvious than
they looked. The [README](../README.md) covers installing and using it.

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

## Memory

- Artwork is decoded to at most 160 px and cached with a bounded LRU, so browsing
  large feeds doesn't grow memory without limit.
- The release profile strips symbols and uses thin LTO. An unstripped binary was
  41 MB and its text and rodata are mapped into every running process, so it cost
  resident memory as well as download size; stripped and LTO'd it is 29 MB.
- Media is written to disk in bounded 16 MB chunks as it arrives, rather than
  being buffered whole in memory.
- Streams are read from `yt-dlp`'s stdout into memory as needed, and at most a
  couple are held open for prefetching.

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

## Packaging

The binary looks for `yt-dlp` in this order, which makes a self-contained
distribution possible:

1. `$YTDLP_PATH`
2. **next to the executable** (ship `yt-dlp_linux` beside the binary)
3. `~/.local/bin/yt-dlp`
4. anywhere on `$PATH`

A release bundle is therefore the binary plus `yt-dlp_linux`, plus `ffmpeg`
(needed only for the download fallback).

Tagging a commit (`git tag v0.1.0 && git push --tags`) runs
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
