# Refrain

A fast, low-memory desktop client for **YouTube Music**, written in Rust with
[`egui`](https://github.com/emilk/egui) and [`rodio`](https://github.com/RustAudio/rodio).

Search the catalogue, open albums / artists / playlists, and play tracks — with
or without signing in.

## How it works

YouTube Music has no public API, so like other third-party clients this one talks
to the same internal **InnerTube** endpoints (`music.youtube.com/youtubei/v1/*`)
that the web app uses, presenting a spoofed `WEB_REMIX` client identity.

```
src/
  main.rs              eframe bootstrap + `--probe`/`--download`/`--auth-check`/
                       `--page`/`--thumb` debug modes
  app.rs               app state, background tasks, egui UI
  config.rs            ~/.config/refrain/config.toml
  auth.rs              cookie extraction, SAPISIDHASH, browser auto-detect
  thumbs.rs            artwork fetch (reqwest) + decode (image)
  ui.rs                theme + reusable widgets (cover grid, list rows)
  ytm/
    client.rs          InnerTube HTTP client (search / browse, authenticated)
    extract.rs         tolerant renderer-tree walking + parsing
    domain.rs          clean domain types (Track, Page, SearchItem)
  audio/
    resolver.rs        yt-dlp: stream args, URL resolution + ranged fetches
    stream.rs          yt-dlp stdout + seekable buffer (streaming playback)
    engine.rs          rodio playback (play/pause/seek/volume)
```

### Parsing strategy

YouTube's response JSON is deeply nested and reshuffled regularly. Rather than
modelling every layer with `serde` structs (which breaks on every change), the
parser walks the tree looking for renderer names it recognises
(`musicResponsiveListItemRenderer`, `musicResponsiveHeaderRenderer`, …). Unknown
wrappers are simply skipped, so the client keeps working when the layout shifts.

### Feeds

The Home/Explore feeds only return a handful of shelves in the first response —
the rest arrive as you scroll, via continuation tokens. The client pages those
in automatically (up to a few rounds), so Home ends up with the same shelves the
real YouTube Music home shows rather than just the first three.

### Playback & streaming

Pressing play runs `yt-dlp -f bestaudio[ext=m4a] -o -` and reads the media from
its **stdout**. `RemoteSource` presents that pipe to `rodio` as a `Read + Seek`
by buffering everything from offset 0 in a background thread, so playback starts
as soon as the first bytes arrive instead of after a full download — and seeking
works even though a pipe isn't seekable. That pipe is also the safety net: if
anything in the faster path below fails, playback still works.

**Resolving is yt-dlp's job; fetching is ours.** A `googlevideo` URL can't be
had without solving YouTube's JS signature challenge, so `yt-dlp -g` does the
resolving (about 6 s). Fetching that URL, though, turned out not to be the
fragile thing it first looked like:

- Fetch it with an explicit `Range` header. Without one YouTube throttles the
  *same* URL to ~33 KB/s — which presents as a stall or a 403, and is what made
  direct fetching look unreliable. With one, a whole track arrives in about a
  second at 5–9 MB/s.
- Any user agent works; the request isn't bound to the client that resolved it.
- Resolved URLs are reused for ~5 h, so fetching a track again skips `yt-dlp`
  entirely.
- Audio is mirrored into `~/.cache/refrain/audio/<video_id>.m4a`, so repeats
  play straight from disk.
- Opening a page resolves and caches the first several tracks in the background
  (each is ~1 s of work once resolved), hovering a track for ~150 ms warms it,
  and starting a track queues the next few.
- Streams are additionally opened **ahead of time** — when a page or search
  loads, after a hover, and for the next track in the queue — so pressing play
  starts in milliseconds instead of waiting on yt-dlp.
- The two tracks you play most are warmed at startup. Ranking comes from local
  play counts (`~/.config/refrain/plays.tsv`); with none recorded yet,
  your YouTube Music history is used, most recent first.
- If no `m4a` stream is offered, playback falls back to the download path, where
  ffmpeg transcodes.

The one unavoidable cost is the first `yt-dlp` extraction for a track (roughly
6 s: ~1.6 s webpage, ~2 s solving YouTube's JS challenges, ~1.5 s startup).
That work is moved off the critical path by prefetching, so it only shows up if
you click a track you never hovered and whose page never warmed it.

Getting the URL without yt-dlp is still not an option: every InnerTube client
that can play returns `signatureCipher` URLs, so it would mean reimplementing
YouTube's JS signature deciphering — the part yt-dlp exists to maintain.

### Memory

- Artwork is decoded to at most 160 px and cached with a bounded (320-entry)
  LRU, so browsing large feeds doesn't grow memory without limit.
- Streams are read from `yt-dlp`'s stdout into memory as needed, and at most two
  are held open for prefetching.

### Packaging

The binary looks for `yt-dlp` in this order, which makes a self-contained
distribution possible:

1. `$YTDLP_PATH`
2. **next to the executable** (ship `yt-dlp_linux` beside the binary)
3. `~/.local/bin/yt-dlp`
4. anywhere on `$PATH`

A release bundle is therefore the binary plus `yt-dlp_linux`, plus `ffmpeg`
(needed only for the download fallback).

### UI

A sidebar (Home / Search / Library) with cover-art grids for albums, artists and
playlists, list rows for tracks, and a now-playing bar with artwork, transport,
seek and volume. Dark and light themes, switchable in the sidebar and persisted
in the config.

### Authentication

Cookies are reused for two independent jobs:

- **Playback** — forwarded straight to `yt-dlp` (`--cookies-from-browser` /
  `--cookies`) to get past YouTube's bot check.
- **Catalogue** — sent to InnerTube with a `SAPISIDHASH` `Authorization` header,
  which makes your library visible.

To avoid re-implementing browser cookie decryption, we drive
`yt-dlp --cookies-from-browser` to materialise a `cookies.txt`, then parse it.
Both jobs share that one extraction.

On Linux, Chromium-family cookies are encrypted with a key held in the OS
keyring, and yt-dlp guesses which keyring from `XDG_CURRENT_DESKTOP`. That guess
is unreliable — and `BASICTEXT` in particular silently fails to decrypt the
`v11` cookies (you get a handful of cookies and no `SAPISID`). So this client
tries `GNOMEKEYRING`, `KWALLET6`, `KWALLET` and `BASICTEXT` explicitly, keeps
whichever yields a `SAPISID`, and remembers it. Pin one with the `keyring`
config option if you prefer.

Extraction takes ~10 s of `yt-dlp`, so the dumped jar is **cached** and reused
for 45 minutes, and InnerTube is used to verify the cached session still works —
a rejected jar triggers a fresh extraction. That makes sign-in ready in well
under a second on subsequent launches. To force a re-extract:

```sh
cargo run -- --auth-check --refresh
```

## Requirements

- Rust (stable)
- **`yt-dlp`** — on `PATH`, at `~/.local/bin/yt-dlp`, or set `YTDLP_PATH`
- **A JavaScript runtime** — `node` or `deno` on `PATH`. yt-dlp needs one to
  solve YouTube's JS challenges; the client passes `--js-runtimes` automatically.
- **`ffmpeg`** — `sudo dnf install ffmpeg`
- **`alsa-lib-devel`** on Fedora — `sudo dnf install alsa-lib-devel`
- For Chromium-family browsers on Linux, Python **`secretstorage`** may be needed
  to decrypt cookies from the OS keyring.

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

`auto` looks in both native and **Flatpak** locations, including Firefox's
modern XDG path (`~/.config/mozilla/firefox`) and Flatpak data dirs such as
`~/.var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser`.

Environment variables still work as a fallback: `YTDLP_COOKIES`,
`YTDLP_COOKIES_FROM_BROWSER`.

## Diagnosing sign-in

```sh
# Lists detected browsers, shows whether cookies resolve, and reports whether
# InnerTube actually recognises you as logged in.
cargo run -- --auth-check
```

If it reports `anonymous ❌`, open <https://music.youtube.com> in the browser
you're extracting from, sign in, confirm you can see your account, then re-run.
A Google login alone is not enough — InnerTube needs a real **YouTube** session
(`LOGIN_INFO`).

## Build & run

```sh
cargo run --release
```

Other non-GUI checks:

```sh
cargo run -- --probe "daft punk"      # exercise search + browse
cargo run -- --page FEmusic_home       # dump a page's tracks and shelves
cargo run -- --stream <video_id>      # stream + report time-to-first-audio
cargo run -- --download <video_id>    # resolve, cache, and decode one track
cargo run -- --decode <path>          # decode a local audio file
cargo run -- --thumb <url>            # verify the artwork pipeline
cargo run -- --auth-check             # sign-in diagnostics
```

### Artwork

Thumbnails are fetched with `reqwest` (the same stack the API client uses) and
uploaded to egui as textures, downscaled to 256px. egui's built-in HTTP loader
is *not* used — the `ehttp` backend it pulls in failed to fetch these URLs.

## Status

Verified working:

- [x] Filtered search (All / Songs / Albums / Artists / Playlists)
- [x] Album, playlist and artist detail pages with artwork
- [x] Browsable carousels — artist albums/singles/videos, featured playlists, related artists
- [x] **Streaming playback** via a `yt-dlp` stdout pipe (`RemoteSource`), with
      on-disk write-through, next-track prefetch and a download fallback
- [x] Resolve-then-fetch: `yt-dlp -g` supplies the URL, but the media is fetched
      by us with a `Range` header (~1 s per track) and reused for ~5 h
- [x] Playback UI: centred transport controls, custom seek bar, volume
- [x] Sidebar (Home / Search / Library), cover-art grids, dark + light themes
- [x] Cookie extraction from Firefox (incl. XDG path) and Chromium-family
      (incl. Flatpak), with automatic keyring selection, plus `cookies.txt` import
- [x] `SAPISIDHASH`-authenticated InnerTube requests
- [x] Library and Home feeds
- [x] Feed shelves paged in via continuations (Home shows the full set, not just
      the first three)
- [x] Artwork fetched and decoded off the UI thread (no window freezes)

When several browsers are present, `auto` picks the first one that actually has
a **YouTube session**, so a logged-out Firefox won't shadow a signed-in Brave.

Not built yet:

- [ ] In-app settings panel for choosing the cookie source (edit `config.toml` for now)

Known limitations:

- Anonymous playback is bot-blocked for much of the music catalogue; a signed-in
  browser session is required for reliable playback.
- The *first* play of a given track waits a few seconds while `yt-dlp` resolves
  the stream URL; repeats and track changes are then near-instant.
- Streaming needs an `m4a` stream; otherwise playback falls back to downloading.
- The InnerTube endpoint/params are unofficial and may need updating over time.

## Legal

For personal, educational use. Respect YouTube's Terms of Service and the rights
of content owners.
