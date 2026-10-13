# AUR: `refrain-music-bin`

The AUR is a git repository per package, so this directory is a *template* to
copy into `ssh://aur@aur.archlinux.org/refrain-music-bin.git` — nobody can push it but
the maintainer named in the PKGBUILD.

## Before pushing

1. **Fill in the maintainer line.** The AUR wants a name and a contact address;
   without one there is nobody to blame when it breaks.
2. **Bump `pkgver` and the checksum** when a new release lands. The checksum is
   the `.sha256` published beside the tarball:
   ```sh
   curl -sL https://github.com/4875636B30/Refrain/releases/download/v0.2.0/refrain-0.2.0-linux-x86_64.tar.gz.sha256
   ```
3. **Regenerate `.SRCINFO`** — hand-written files drift:
   ```sh
   makepkg --printsrcinfo > .SRCINFO
   ```

## Then

```sh
git clone ssh://aur@aur.archlinux.org/refrain-music-bin.git
cp PKGBUILD .SRCINFO refrain-music-bin/
cd refrain-music-bin && git add PKGBUILD .SRCINFO
git commit -m "Initial import: refrain-music-bin 0.1.4"
git push
```

## Why not `refrain-bin`

`refrain` on the AUR is an unrelated package — Discord Rich Presence for Apple
Music — so `refrain-bin` would read as its binary build. `refrain-music-bin`
says what it is instead, and nothing is claimed from the plain name.

## What the package does and does not ship

It installs the binary, the launcher entry, the icon, the AppStream metadata and
the licence — and deliberately **not** the bundle's own `yt-dlp`, because the
package depends on Arch's. That is the better copy anyway: it is updated far more
often than a release, and the client prefers whichever `yt-dlp` is newer, so the
packaged one wins even when a bundle ships one.

`alsa-lib` is the only library the binary links beyond glibc. X11, Wayland and
GL are loaded at runtime, so they are listed as dependencies even though `ldd`
does not mention them: without them there is no window.

## The JavaScript runtime

yt-dlp cannot solve YouTube's challenges without a JavaScript runtime, and
without one playback fails. It is an `optdepends` rather than a `depends`
because there is a choice — `quickjs-ng` (the small one, and first in the list),
`nodejs` or `deno` — and hard-depending on one would drag Node onto systems that
already have another.

The release tarball *does* carry a QuickJS beside the binary, but this package
installs only the binary: putting a private `qjs` in `/usr/bin` would be rude,
and Arch packages the real thing.
