# Flathub: `io.github._4875636B30.Refrain`

The manifest here is a **template**, not a working submission. Flathub apps live
in their own repository (`flathub/io.github.4875636B30.Refrain`) containing the
manifest at the root and `cargo-sources.json` beside it. Nothing here has been
built in a Flatpak — there is no SDK or `flatpak-builder` on the machine this
was written on — so treat the manifest as reviewed, not tested.

## Before submitting this

**Do not submit the manifest here as it stands.** Flathub's [Generative AI
policy](https://docs.flathub.org/docs/for-app-authors/requirements#generative-ai-policy)
says a Flathub manifest must not contain AI-generated or AI-assisted content,
and disclosure does not exempt a manifest from that rule. This draft was written
with AI assistance, so treat it as a reference for what the manifest has to
contain, and write the one you submit yourself.

Three things the draft gets wrong, all reported by Flathub's own linter
(`flatpak run --command=flatpak-builder-lint org.flatpak.Builder`):

- **The app id.** A component beginning with a digit needs a leading
  underscore, so the id is `io.github._4875636B30.Refrain`. Without it,
  `flatpak build-init` refuses the name outright and the linter reports
  `appid-component-wrong-syntax`.
- **The runtime.** It must be the latest at the time of submission. The linter
  currently reports
  `runtime-update-available-to-org.freedesktop.Platform-26.08`.
- **yt-dlp is downloaded prebuilt.** Every runtime dependency in a manifest has
  to be built from source, so this needs a build step, or an accepted exception.

The metainfo also has to be installed as `%{id}.metainfo.xml` — that is,
`io.github._4875636B30.Refrain.metainfo.xml` — matching the manifest's `id`
exactly.

## The pieces

- `io.github.4875636B30.Refrain.yml` — the manifest.
- `cargo-sources.json` — every crate, vendored, so the build works with no
  network. Generated from `Cargo.lock`; regenerate after any dependency change:
  ```sh
  flatpak-cargo-generator.py ../../Cargo.lock -o cargo-sources.json
  ```
  It also carries the `.cargo/config` that redirects crates.io at the vendored
  copy, so the manifest needs no extra step.

## Submitting

1. Fork `flathub/flathub`, branch from `new-pr`, and open a pull request with the
   manifest and `cargo-sources.json` added. Flathub's own CI builds it there —
   that build is the real test, and it is where the questions below get settled.
2. Flathub verifies app ids, so `io.github.4875636B30.Refrain` needs the GitHub
   account to match (it does).

## Two things that are not settled

### Signing in

Inside a sandbox the client cannot simply read a browser's cookies, and there are
trade-offs rather than a right answer:

- **Firefox works as written.** The manifest grants read-only access to
  `~/.mozilla`, and Firefox cookies need no keyring, so sign-in works out of the
  box.
- **Chromium-family browsers (Chrome, Brave, Edge, …) need more.** Their cookies
  are encrypted with a key in the host keyring. The manifest grants
  `--talk-name=org.freedesktop.secrets`, but yt-dlp also needs the
  `secretstorage` Python module, which the runtime does not ship. Adding it means
  a `flatpak-pip-generator` module (and `cryptography` with it). The alternative
  is the client's `file` cookie mode: export a `cookies.txt` once and point the
  config at it, which needs no sandbox hole at all.
- **Read-only access to browser profiles** is a broad grant, and a reviewer may
  push back on it. The narrowest honest version is Firefox-only plus the
  cookies-file route.

### yt-dlp goes stale

The manifest pins a yt-dlp release, and inside a sandbox the client cannot see
the host's copy — so the flatpak's yt-dlp ages between manifest bumps. This is
unavoidable for a sandboxed app, and it is the reason the manifest is pinned to a
tag rather than tracking latest: the bump has to be deliberate. Expect to bump it
whenever YouTube changes something, which is roughly whenever yt-dlp has a
release.

## A note on QuickJS

The manifest bundles QuickJS (about 1 MB) rather than Node (about 50 MB) because
yt-dlp accepts either, and the client looks for a runtime *beside its own
executable* before looking on `PATH` — so the bundled one is picked up with no
configuration. That lookup was added for exactly this: it also makes the
AppImage and the tarball self-sufficient for anyone who installs them on a
machine without Node.
