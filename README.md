# youtube-music-rs

A desktop **YouTube Music** client written in Rust, using [`egui`](https://github.com/emilk/egui)
for the UI and [`rodio`](https://github.com/RustAudio/rodio) for playback.

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
  config.rs            ~/.config/youtube-music-rs/config.toml
  auth.rs              cookie extraction, SAPISIDHASH, browser auto-detect
  thumbs.rs            artwork fetch (reqwest) + decode (image)
  ytm/
    client.rs          InnerTube HTTP client (search / browse, authenticated)
    extract.rs         tolerant renderer-tree walking + parsing
    domain.rs          clean domain types (Track, Page, SearchItem)
  audio/
    resolver.rs        yt-dlp -> cached .m4a stream, on disk
    engine.rs          rodio playback (play/pause/seek/volume)
```

### Parsing strategy

YouTube's response JSON is deeply nested and reshuffled regularly. Rather than
modelling every layer with `serde` structs (which breaks on every change), the
parser walks the tree looking for renderer names it recognises
(`musicResponsiveListItemRenderer`, `musicResponsiveHeaderRenderer`, …). Unknown
wrappers are simply skipped, so the client keeps working when the layout shifts.

### Playback

`yt-dlp` resolves YouTube's signature/`n`-param obfuscation, ffmpeg normalises
audio to `m4a` (AAC), and `rodio` decodes it. Files are cached in
`~/.cache/youtube-music-rs/audio/<video_id>.m4a`, so replays are instant and
seeking works. The next track is pre-fetched while the current one plays.

### Authentication

Cookies are reused for two independent jobs:

- **Playback** — forwarded straight to `yt-dlp` (`--cookies-from-browser` /
  `--cookies`) to get past YouTube's bot check.
- **Catalogue** — sent to InnerTube with a `SAPISIDHASH` `Authorization` header,
  which makes your library visible.

To avoid re-implementing browser cookie decryption, we drive
`yt-dlp --cookies-from-browser` to materialise a `cookies.txt`, then parse it.
Both jobs share that one extraction.

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

`~/.config/youtube-music-rs/config.toml` (created once you change a setting):

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
cargo run -- --page VLLM              # dump a page's tracks and shelves
cargo run -- --download dQw4w9WgXcQ   # resolve, cache, and decode one track
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
- [x] yt-dlp → ffmpeg → `m4a` → rodio decode pipeline
- [x] Playback UI: play/pause, prev/next, seek, volume, auto-advance, prefetch
- [x] Cookie extraction from Firefox (incl. XDG path) and Chromium-family
      (incl. Flatpak), plus `cookies.txt` import
- [x] `SAPISIDHASH`-authenticated InnerTube requests
- [x] Library view — the **Library** button opens `FEmusic_library_landing`
      (grid of Liked Music / playlists), and those open into normal playlists

When several browsers are present, `auto` picks the first one that actually has
a **YouTube session**, so a logged-out Firefox won't shadow a signed-in Brave.

Not built yet:

- [ ] In-app settings panel for choosing the cookie source (edit `config.toml` for now)

Known limitations:

- Anonymous playback is bot-blocked for much of the music catalogue; a signed-in
  browser session is required for reliable playback.
- The InnerTube endpoint/params are unofficial and may need updating over time.

## Legal

For personal, educational use. Respect YouTube's Terms of Service and the rights
of content owners.
