# Contributing

Issues and pull requests are both welcome. This is a small project, so a short
note explaining what you want to change goes a long way.

## Reporting a bug

The most useful report says which build it is, and what the diagnostics think:

```sh
cargo run -- --version        # or the tag you downloaded
cargo run -- --auth-check     # sign-in: which browser, whether it resolved
```

Then what you did, what happened, and what you expected. If playback is
involved, `--stream <video_id>` reproduces it outside the GUI, which makes the
difference between "the app is broken" and "this one track is unavailable".

## Getting set up

You need Rust (stable), plus the native libraries for audio and windowing:

```sh
# Fedora
sudo dnf install alsa-lib-devel libxkbcommon-devel wayland-devel \
                 libX11-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel

# Debian/Ubuntu
sudo apt-get install libasound2-dev libxkbcommon-dev libwayland-dev \
                     libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
```

No D-Bus development headers are needed: the media controls use `zbus`, which is
pure Rust.

At runtime you also need **`yt-dlp`** and a **JavaScript runtime** (`node` or
`deno`) — see the [README](README.md#requirements) for why.

```sh
cargo run --release
```

## Before you open a pull request

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three run in CI, and CI is the gate. `clippy` is set to deny warnings, so a
warning is a failure.

Some tests run against captured YouTube responses in `tests/fixtures/`. If one
fails after a change of yours, it is usually telling you the parser stopped
finding something — read the assertion before assuming the fixture is stale. If
YouTube really did change shape, re-capture with
`python3 tools/capture-fixtures.py` and read the diff before committing it.

## Checking the other platforms

CI builds and tests Linux, macOS and Windows, but that takes a round trip. To
lint another platform's code paths locally, flip the platform gates — the
Linux-only blocks vanish and the others appear, which is what those targets
compile:

```sh
sed -i 's/target_os = "linux"/any()/g; s/not(any())/all()/g' src/*.rs   # then revert
```

The usual failure this catches is code that is only *used* on Linux becoming
dead on the others, which `-D warnings` turns into a build failure.

What it cannot catch is a **dependency whose API differs per platform**: flipping
our own `cfg`s does not change what `souvlaki`, say, exposes. `MediaControls`
has `set_volume` on MPRIS and no such method on macOS or Windows, which only a
real build there reveals. To check that class of problem, read the backend's
public methods — `grep 'pub fn' src/platform/<backend>*.rs` in the crate's
source is enough, and it beats waiting for CI.

## How the code is laid out

[`docs/architecture.md`](docs/architecture.md) explains the design and the parts
that turned out to be less obvious than they looked — worth reading before
changing the parser, the streaming path or authentication. The short version:

```
src/app.rs        app state, background tasks, the whole UI
src/ui.rs         theme, reusable widgets
src/ytm/          InnerTube client, tolerant parsing, domain types
src/audio/        yt-dlp resolution, streaming, rodio playback
src/media.rs      desktop media controls (MPRIS et al)
```

## Things worth knowing before you change something

**The parser is deliberately tolerant.** YouTube reshuffles its response JSON
regularly. `ytm/extract.rs` walks the tree looking for renderer names it
recognises and skips unknown wrappers, rather than modelling every layer with
`serde` structs. Strict models break on every reshuffle; please don't add one.

**The InnerTube endpoints are unofficial.** They can change without notice, and
a change that looks like our bug often isn't.

**Prefer measuring to guessing.** Several decisions here came from a measurement
that contradicted the obvious answer — batching `yt-dlp` calls saves only ~15%,
a `Range` header is the difference between 33 KB/s and 8 MB/s, and the "missing"
font glyph turned out to be present. Where a comment states a number, it was
measured; keep that habit, and say what you measured in the commit message.

**Comments explain why, not what.** The `what` is in the code.

**Tests cover decisions, not plumbing.** The interesting ones are pure functions
that encode a rule: which track plays next, which cached file may be evicted,
which playlist ids are normalised, how a yt-dlp failure is explained. If you add
a rule like that, put it in a function and test it — several bugs here were only
findable once the rule was extracted from the UI loop.

## Help wanted

Concrete and bounded, roughly by size. None of these need a plan agreed first —
open an issue saying which one you are taking.

**Good first issues**

- **A like shortcut.** Thumbs already work; there is no key for them. One
  binding in `handle_shortcuts`, next to the existing ones.
- **A fixture for a page type that has none.** `tests/fixtures/` covers search,
  album, artist, playlist and home. A podcast page, a radio playlist, or a
  signed-in library page would each be a capture plus a test in
  `src/ytm/extract.rs`, and `tools/capture-fixtures.py` does the fetching.
- **Remember the window size and position.** The viewport is fixed at 1120×740
  in `main.rs`. The config already persists the theme and the volume, so this is
  two more fields and a `ViewportBuilder` call.

**Medium**

- **An offline view.** `--download` caches a track, and the cache is budgeted and
  pruned, but nothing in the UI lists what is cached or offers to keep it. A
  "Downloads" list in the sidebar would make the offline story real.
- **Timed lyrics.** The words are fetched and shown, but the timing is not:
  YouTube's web responses carry none, and its own client gets synced lyrics
  from somewhere else. Finding that source — another client identity, another
  endpoint — is what would let the panel follow along.

**Larger, or needs care**

- **Flatpak sign-in.** See `packaging/flathub/README.md`: Chromium-family
  browsers need the `secretstorage` Python module inside the sandbox, and there
  is a choice to make between bundling it and steering users to a `cookies.txt`.
- **MPRIS `LoopStatus` and `Shuffle`.** souvlaki does not implement those
  optional properties, so repeat and shuffle are app-only. Either upstream, or a
  small zbus interface of our own alongside it.
- **Packaging beyond the AppImage and the AUR**: a `.deb`, a Nix flake, or an
  AppImage that updates itself.

## Recording a demo

`--demo` runs a scripted sequence — search, open an album, play it, show the
queue — so a screenshot or a GIF can be reproduced on demand. It is also the
reason the screenshot in the README shows an album page rather than a home feed:
a home feed is personalised, and publishing one publishes the author's taste.

```sh
cargo run --release -- --demo
```

It plays audio, so mute first if that matters. For a still, capture once the
queue has appeared (about ten seconds in, depending on how long sign-in takes —
the steps wait for each other rather than running on a timer, so it is safe to
start recording whenever).

## Scope

The project aims to be a good *client*: fast to start, light on memory, and
honest about what it can't do. It deliberately does not reimplement `yt-dlp`'s
work — [here's why](docs/architecture.md#why-the-cipher-isnt-reimplemented) —
and it won't pretend to be YouTube Music or use their marks.
