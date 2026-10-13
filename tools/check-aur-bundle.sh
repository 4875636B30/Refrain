#!/bin/bash
# Check the AUR package against a real release, before pushing it.
#
# The PKGBUILD names files inside the release bundle, and the release workflow
# renames some of them — the AppStream file is installed as the component id,
# not as `refrain.metainfo.xml`. A mismatch like that is invisible until
# `makepkg` fails on somebody else's machine, which is exactly what happened.
# So check the names against the artefact that was actually published.
#
# usage: check-aur-bundle.sh [version]
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
pkgbuild="$here/packaging/aur/PKGBUILD"

version=${1:-$(sed -n 's/^pkgver=//p' "$pkgbuild")}
expected=$(sed -n "s/^sha256sums=('\([0-9a-f]*\)').*/\1/p" "$pkgbuild")

if [ -z "$version" ] || [ -z "$expected" ]; then
  echo "  could not read pkgver/sha256sums from $pkgbuild" >&2
  exit 2
fi

url="https://github.com/4875636B30/Refrain/releases/download/v${version}/refrain-${version}-linux-x86_64.tar.gz"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "  fetching v${version}"
curl -fsSL -o "$work/bundle.tar.gz" "$url"

actual=$(sha256sum "$work/bundle.tar.gz" | cut -d' ' -f1)
if [ "$actual" != "$expected" ]; then
  echo "  checksum mismatch: the PKGBUILD says $expected, the release is $actual" >&2
  exit 1
fi
echo "  checksum matches"

tar tzf "$work/bundle.tar.gz" \
  | sed "s|^refrain-${version}-linux-x86_64/||" \
  | grep -v '/$' | sort > "$work/in-bundle.txt"

grep -oE '\$\{bundle\}/[^"]+' "$pkgbuild" \
  | sed 's|\${bundle}/||' | sort > "$work/expected.txt"

missing=$(comm -23 "$work/expected.txt" "$work/in-bundle.txt")
if [ -n "$missing" ]; then
  echo "  the PKGBUILD installs files that are not in the bundle:" >&2
  echo "$missing" | sed 's/^/    /' >&2
  exit 1
fi
echo "  every file the PKGBUILD installs is present"
