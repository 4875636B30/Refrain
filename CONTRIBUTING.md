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

## Scope

The project aims to be a good *client*: fast to start, light on memory, and
honest about what it can't do. It deliberately does not reimplement `yt-dlp`'s
work — [here's why](docs/architecture.md#why-the-cipher-isnt-reimplemented) —
and it won't pretend to be YouTube Music or use their marks.
