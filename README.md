<p align="center">
  <img src="packaging/refrain.svg" width="104" alt="">
</p>

<h1 align="center">Refrain</h1>

<p align="center"><b>YouTube Music, native and fast.</b><br>
A lightweight desktop client for YouTube Music, written in Rust.</p>

<p align="center">
  <a href="https://github.com/4875636B30/Refrain/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/4875636B30/Refrain/actions/workflows/ci.yml/badge.svg"></a>
  <a href="../../releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/4875636B30/Refrain?style=flat-square&color=ff0000"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-2b2b2b?style=flat-square"></a>
  <img alt="Built with Rust" src="https://img.shields.io/badge/built_with-Rust-000000?style=flat-square&logo=rust&logoColor=white">
  <img alt="Platforms" src="https://img.shields.io/badge/platform-Linux_%C2%B7_macOS_%C2%B7_Windows-ff0000?style=flat-square">
</p>

![Refrain's home feed: quick picks, cover art, and the player bar](docs/screenshot.png)

Refrain plays your YouTube Music library without a browser engine. It talks to
the same internal endpoints the web app uses, and streams audio through
`yt-dlp`. No web view and no Electron: the window is up in about half a second
(0.49 s measured), and the release build holds a steady ~205 MB resident with
the home feed loaded — it does not creep as you browse. Most of that is the GPU
driver; artwork is fetched only for what is actually on screen.

Sign in by reusing the session your browser already has — no separate login, no
password to hand over.

| | |
| --- | --- |
| **Search** | Songs, videos, albums, artists and playlists, grouped into categories with "See more" to expand one. |
| **Browse** | Home and Library feeds, album / artist / playlist pages, cover-art grids, carousels and "quick picks". |
| **Playback** | Streams through `yt-dlp` and caches to disk, so replays are instant. Seek, volume, shuffle, repeat, auto-advance. |
| **Your library** | Thumbs up and down, playlists you can add to, rename, delete or remove tracks from, and an "Up next" queue. |
| **Desktop integration** | Media keys, the panel's media widget and `playerctl`, through MPRIS. |
| **Themes** | Light and dark, persisted in the config. Roboto is bundled, so it looks the same on every desktop. |
| **Settings** | Theme, sign-in source, cache size and clearing it, and yt-dlp status — all in-app, no config editing. |
| **Keyboard** | Space, arrows, `Shift`+arrows, `/` and `Esc` — see [shortcuts](#keyboard-shortcuts). |

## Install

### AppImage (Linux x86_64)

One file, nothing to install — download it from [Releases](../../releases) and
run it:

```sh
chmod +x refrain-*-x86_64.AppImage
./refrain-*-x86_64.AppImage
```

It carries `yt-dlp` and QuickJS inside, so playback works without installing
anything else. `ffmpeg` is still needed, but only for the fallback path that
transcodes a stream the fast path could not use.

### macOS

Download the tarball from [Releases](../../releases) and move `refrain.app` to
your Applications folder. It is not signed, so macOS will refuse the first
launch: right-click the app, choose **Open**, and confirm. (Or
`xattr -dr com.apple.quarantine refrain.app`.)

Both Apple silicon and Intel builds are published.

### Windows

Download the `.zip` from [Releases](../../releases) and unzip it wherever you
like — `refrain.exe` runs from there, with `yt-dlp.exe` beside it. SmartScreen
will ask before running an unknown publisher's executable.

### Release bundle (Linux x86_64)

Download the tarball from [Releases](../../releases), then install it for your
user:

```sh
tar xzf refrain-*-linux-x86_64.tar.gz
cd refrain-*-linux-x86_64

install -Dm755 refrain ~/.local/bin/refrain
install -Dm644 refrain.svg ~/.local/share/icons/hicolor/scalable/apps/refrain.svg
install -Dm644 refrain.desktop ~/.local/share/applications/refrain.desktop
install -Dm644 refrain.metainfo.xml ~/.local/share/metainfo/refrain.metainfo.xml

# yt-dlp ships beside the binary. A newer copy anywhere else wins over it, so
# playback keeps working as yt-dlp is updated:
cp yt-dlp_linux ~/.local/bin/yt-dlp_linux
```

The bundle contains `yt-dlp` so you don't have to install it yourself. `ffmpeg`
is still needed, but only for the fallback that transcodes a stream the fast path
couldn't use.

### From source

```sh
git clone https://github.com/4875636B30/Refrain.git
cd refrain
cargo run --release
```

## Requirements

**To run it:**

- **`yt-dlp`** — found on `PATH`, at `~/.local/bin/yt-dlp`, or set `YTDLP_PATH`.
  Every release bundle ships one for its platform.
  The release bundle ships one, and if you have a *newer* copy elsewhere that
  one is used instead: yt-dlp is the part that breaks when YouTube changes, so
  a stale bundled copy would otherwise take playback down with it. Settings
  shows which copy is in use and how old it is.
- **A JavaScript runtime** — **not needed for a downloaded build**: every
  release carries QuickJS beside the executable, which is what yt-dlp uses to
  solve YouTube's JS challenges. Building from source, or installing from a
  package manager that does not bundle one, means supplying your own: `node`,
  `deno` or `quickjs`, on `PATH` or beside the executable. Refrain passes
  `--js-runtimes` for you.
- **`ffmpeg`** — only for the download fallback.
- **A signed-in browser** for your library and reliable playback. Anonymous
  playback is bot-blocked for much of the catalogue.

**To build it:**

- Rust (stable)
- On Linux: `alsa-lib-devel` for `rodio`, and X11/Wayland development libraries
  for `eframe`/`winit`. macOS and Windows need nothing beyond the toolchain —
  CoreAudio and WASAPI ship with the OS
- For Chromium-family browsers on Linux, Python **`secretstorage`** may be needed
  to decrypt cookies from the OS keyring

No D-Bus development headers are needed: the media controls use `zbus`, which is
pure Rust.

### Packages

| | |
| --- | --- |
| **AUR** | `refrain-bin` — see [packaging/aur](packaging/aur) for the PKGBUILD and how to publish it |
| **Flathub** | manifest drafted in [packaging/flathub](packaging/flathub); not submitted yet, and the sandbox's sign-in story is still open |

## First run

Refrain looks for a signed-in browser session on startup — Firefox, and the
Chromium family including Flatpak installs — and reuses whichever one actually
has a **YouTube** session. A Google login alone isn't enough; InnerTube needs a
real YouTube session.

If your library looks empty, check what it found:

```sh
cargo run -- --auth-check
```

It lists the browsers it detected, whether cookies resolve, and whether
InnerTube recognises you as logged in. If it reports `anonymous ❌`, open
<https://music.youtube.com> in that browser, sign in, confirm you can see your
account, and re-run. To force a fresh extraction:

```sh
cargo run -- --auth-check --refresh
```

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Space` | Play / pause |
| `→` / `←` | Seek forward / back 5 s |
| `Shift`+`→` / `Shift`+`←` | Next / previous track |
| `/` | Focus search |
| `Q` | Show / hide the queue |
| `,` | Settings |
| `Esc` | Close a dialog, or go back |

## Status

Working today:

- Filtered search, grouped results, "See more" per category, and paging within
  an expanded category
- Home and Library feeds, with shelves paged in via continuations
- Album, artist and playlist pages, with artwork and carousels
- Streaming playback with on-disk write-through, prefetching and a download fallback
- Thumbs up / down, playlist editing, shuffle, repeat, "Up next" queue
- MPRIS media controls, media keys and `playerctl`
- Light and dark themes, with Roboto bundled rather than assumed
- An in-app settings window: theme, sign-in refresh, cache size and clearing
- Feed shelves as sideways-scrolling carousels, as YouTube Music shows them
- Artwork fetched only for items actually on screen (a feed asks for ~20 images
  instead of ~200)
- Sign-in configurable in-app: source, browser and keyring
- Linux (AppImage and tarball), macOS (`.app`, both architectures) and Windows
  (`.zip`), each with `yt-dlp` shipped beside the executable
- Desktop media controls on all three: MPRIS, and the macOS and Windows
  equivalents through the same code

Not built yet:

- Packaging beyond the tarball (no Flatpak, AUR or `.deb` yet)

Known limitations:

- **Anonymous playback is unreliable.** YouTube bot-blocks much of the catalogue
  without a signed-in session.
- **The first play of a track takes a few seconds** while `yt-dlp` resolves the
  stream URL. Prefetching hides this for tracks you hover or that a page warms;
  repeats are instant.
- **`yt-dlp` is a hard dependency.** Getting the stream URL means solving
  YouTube's JS challenges, which is the part `yt-dlp` exists to maintain —
  [why that isn't reimplemented](docs/architecture.md#why-the-cipher-isnt-reimplemented).
- The InnerTube endpoints are unofficial and may need updating over time.

## Development

```sh
cargo run --release          # run it
cargo fmt --all --check      # formatting
cargo clippy --all-targets -- -D warnings
cargo test
```

Useful diagnostics, all of which run without a GUI:

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

Design notes, and the parts that turned out to be less obvious than they looked,
are in **[docs/architecture.md](docs/architecture.md)**.

Contributions are welcome — issues and pull requests both. See
[CONTRIBUTING.md](CONTRIBUTING.md) for how to set up, what CI checks, and the few
conventions worth knowing (the parser is deliberately tolerant rather than
strict, because YouTube reshuffles its JSON regularly).

## Legal

Refrain is an independent project and is not affiliated with, endorsed by, or
sponsored by YouTube or Google. "YouTube" is a trademark of Google LLC.

It is intended for personal, educational use. Respect YouTube's Terms of Service
and the rights of content owners.

## License

[MIT](LICENSE).

## Acknowledgements

Built on [egui](https://github.com/emilk/egui) and
[eframe](https://github.com/emilk/egui/tree/master/crates/eframe),
[rodio](https://github.com/RustAudio/rodio),
[yt-dlp](https://github.com/yt-dlp/yt-dlp),
[souvlaki](https://github.com/Sinono3/souvlaki) and
[zbus](https://gitlab.freedesktop.org/dbus/zbus).

The name and icon are this project's own — deliberately not YouTube's
play-button mark, which is their trademark.
