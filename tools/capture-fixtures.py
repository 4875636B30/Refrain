#!/usr/bin/env python3
"""Capture real InnerTube responses as parser fixtures.

The parser walks YouTube's renderer tree by name. That is what makes it
tolerant of shape changes, and it is also how it fails: when a renderer is
renamed or a field moves, extraction returns nothing and the app goes quietly
empty instead of erroring. These fixtures are what turn that into a failing
test.

Re-run this when the parser learns to read something new, or when a test
starts failing because YouTube changed shape:

    python3 tools/capture-fixtures.py

Then read the diff before committing it. The fixtures are frozen snapshots —
the tests assert against them exactly, so a shape change shows up as a
failure rather than as an empty shelf.

They are stored gzipped because they are verbatim responses, and a real one
is a few hundred kilobytes of thumbnail URLs. Nothing structural is removed,
so what the parser sees is what YouTube sent. To read one:

    zcat tests/fixtures/search_all.json.gz | python3 -m json.tool | less

Two keys are dropped: `trackingParams` and `frameworkUpdates`, opaque blobs
that no code in this repository reads and that dominate the payload.

No account is involved: the requests are anonymous, which is also how the app
behaves when nobody has signed in.
"""

import gzip
import json
import pathlib
import urllib.request

ENDPOINT = "https://music.youtube.com/youtubei/v1"
CLIENT_VERSION = "1.20250630.00.00"
CLIENT = {
    "clientName": "WEB_REMIX",
    "clientVersion": CLIENT_VERSION,
    "hl": "en",
    "gl": "US",
}
HEADERS = {
    "Content-Type": "application/json",
    "Origin": "https://music.youtube.com",
    "Referer": "https://music.youtube.com/",
    "X-Goog-Api-Format-Version": "1",
    "X-YouTube-Client-Name": "67",
    "X-YouTube-Client-Version": CLIENT_VERSION,
    "User-Agent": (
        "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"
    ),
}

# From src/ytm/client.rs — the opaque protobuf token for the Songs filter.
SONGS_PARAMS = "EgWKAQIIAWoSEAUQCRADEAoQBBAQEBUQDhAR"

# A well-known artist, so the results are stable enough to assert against and
# nobody's library is involved.
QUERY = "daft punk"

# The second track of that album, which has lyrics. The first does not, which
# is why the tests also cover a track with no lyrics tab at all.
LYRICS_TRACK = "ajGKWk0auOc"

DROP_KEYS = {"trackingParams", "frameworkUpdates"}

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures"


def post(endpoint: str, body: dict) -> dict:
    payload = dict(body)
    payload["context"] = {
        "client": CLIENT,
        "user": {"lockedSafetyMode": False},
        "request": {"useSsl": True},
    }
    request = urllib.request.Request(
        f"{ENDPOINT}/{endpoint}?prettyPrint=false",
        data=json.dumps(payload).encode(),
        headers=HEADERS,
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def strip(node):
    """Remove the two blobs nothing reads. Everything else is left alone."""
    if isinstance(node, dict):
        return {
            name: strip(value)
            for name, value in node.items()
            if name not in DROP_KEYS
        }
    if isinstance(node, list):
        return [strip(value) for value in node]
    return node


def collect_ids(node, out: dict) -> None:
    """Find the first album, artist and playlist in a search response.

    Album and artist results arrive as rows, not as two-row items, so this
    looks at any browse endpoint carrying a page type rather than at a
    particular renderer.
    """
    if isinstance(node, dict):
        endpoint = node.get("browseEndpoint")
        if isinstance(endpoint, dict):
            browse_id = endpoint.get("browseId")
            page_type = (
                endpoint.get("browseEndpointContextSupportedConfigs", {})
                .get("browseEndpointContextMusicConfig", {})
                .get("pageType", "")
            )
            if browse_id:
                for kind in ("ALBUM", "ARTIST", "PLAYLIST"):
                    if kind in page_type:
                        out.setdefault(kind.lower(), browse_id)
        for value in node.values():
            collect_ids(value, out)
    elif isinstance(node, list):
        for value in node:
            collect_ids(value, out)


def save(name: str, payload: dict) -> None:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    path = FIXTURES / f"{name}.json.gz"
    body = json.dumps(strip(payload), separators=(",", ":")).encode()
    # mtime=0 so re-capturing an unchanged response produces an identical file.
    with gzip.GzipFile(path, "wb", mtime=0) as handle:
        handle.write(body)
    print(f"  {path.name:26} {path.stat().st_size / 1024:6.0f} KB gz  ({len(body) / 1024:.0f} KB raw)")


def main() -> int:
    print("capturing:")
    search = post("search", {"query": QUERY})
    save("search_all", search)
    save("search_songs", post("search", {"query": QUERY, "params": SONGS_PARAMS}))

    ids: dict = {}
    collect_ids(search, ids)
    print(f"  ids found: {ids}")

    for kind in ("album", "artist", "playlist"):
        if kind in ids:
            save(f"browse_{kind}", post("browse", {"browseId": ids[kind]}))
    save("browse_home", post("browse", {"browseId": "FEmusic_home"}))

    # The player response and the lyrics page it points at. The watch-next
    # response carries no words itself — only a browse endpoint for a track
    # that has them — so both halves are kept.
    player = post("next", {"videoId": LYRICS_TRACK})
    save("next", player)
    lyrics_id = lyrics_browse_id(player)
    print(f"  lyrics browse id: {lyrics_id}")
    if lyrics_id:
        save("lyrics", post("browse", {"browseId": lyrics_id}))

    # The same track's endless radio, as the player panel returns it: this is
    # what autoplay plays when a queue runs out.
    radio_id = radio_playlist_id(player)
    print(f"  radio playlist id: {radio_id}")
    if radio_id:
        save(
            "radio",
            post(
                "next",
                {
                    "videoId": LYRICS_TRACK,
                    "playlistId": radio_id,
                    "playlistPanelOffset": 0,
                    "isAudioOnly": True,
                },
            ),
        )
    return 0


def radio_playlist_id(node) -> str | None:
    found = None

    def walk(value) -> None:
        nonlocal found
        if found is not None:
            return
        if isinstance(value, dict):
            for key in ("watchPlaylistEndpoint", "watchEndpoint"):
                endpoint = value.get(key)
                if isinstance(endpoint, dict):
                    playlist_id = endpoint.get("playlistId")
                    if isinstance(playlist_id, str) and playlist_id.startswith("RD"):
                        found = playlist_id
                        return
            for child in value.values():
                walk(child)
        elif isinstance(value, list):
            for child in value:
                walk(child)

    walk(node)
    return found


def lyrics_browse_id(node) -> str | None:
    found = None

    def walk(value) -> None:
        nonlocal found
        if found is not None:
            return
        if isinstance(value, dict):
            endpoint = value.get("browseEndpoint")
            if isinstance(endpoint, dict):
                config = (
                    endpoint.get("browseEndpointContextSupportedConfigs", {})
                    .get("browseEndpointContextMusicConfig", {})
                )
                if config.get("pageType") == "MUSIC_PAGE_TYPE_TRACK_LYRICS":
                    found = endpoint.get("browseId")
                    return
            for child in value.values():
                walk(child)
        elif isinstance(value, list):
            for child in value:
                walk(child)

    walk(node)
    return found


if __name__ == "__main__":
    raise SystemExit(main())
