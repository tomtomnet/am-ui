#!/usr/bin/env python3
"""What the Python command line prints, for the Rust port (crates/engine/src/cli) to compare with.

    python3 -I crates/engine/tests/reference/cli_reference.py > crates/engine/tests/fixtures/cli_reference.json

The commands of amui/cli_library.py and amui/cli_pipeline.py are run by the Python engine itself over
a scratch folder (its own AMUI_VAR, a copy of the workspace tools, its own HOME) and a stand-in
network that answers from the table below. Every resource is made up. Nothing real is touched: a
socket that is opened ends the run, and so does a program that is started, with one exception: bash
on the stand-in render script this run writes into its scratch folder (`render`). Nothing outside
the scratch folder is written; of the repository the engine's sources and the workspace tools are
read. The sink and the wrapper service are never asked: each is a table in this script (`SINK`
takes what the downmix tool would set, `SERVICE` answers for the service on loopback).

The output holds

* "answers": what the stand-in network serves, by URL (the Rust tests serve the same);
* "groups": a state of the scratch folder and the command lines run in it, each with its exit
  status and what it printed. The scratch data folder reads "<var>" and the scratch folder itself
  "<scratch>", the time the cache listing prints for an entry reads "<time>", and of the progress
  lines of a render only the last one of a run is kept (how many there are depends on how fast the
  stand-in renderer is);
* "values": what the pure helpers return (`_clip`, `_table`, `_mmss`, `_hms`, `_fmt_bytes`, the
  probe module's `track_id`), the doctor's report lines, the progress lines `acquire` prints for a
  list of reports, and how Python spells values that came out of a JSON file.

The tests of crates/engine/src/cli/reference.rs run the same command lines over the same states.
"""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SCRATCH = Path(tempfile.mkdtemp(prefix="amui-cli-reference-"))
(SCRATCH / "workspace").mkdir()
shutil.copytree(ROOT / "workspace/tools", SCRATCH / "workspace/tools")  # the engine tools, state-free
(SCRATCH / "home").mkdir()
for name in ("AMUI_SIGNIN", "AMUI_RENDER_SCRIPT", "AMUI_BASS_MANAGE", "AMUI_LOSSLESS_MAX_RATE", "AMUI_DOTNET",
             "CAVERN_LAB_OPTIONS", "DISPLAY", "WAYLAND_DISPLAY"):
    os.environ.pop(name, None)
RENDER_SCRIPT = SCRATCH / "render.sh"  # a stand-in renderer, written by the groups that render
os.environ.update({"AMUI_WORKSPACE": str(SCRATCH / "workspace"), "AMUI_VAR": str(SCRATCH / "var"),
                   "AMUI_RENDER_SCRIPT": str(RENDER_SCRIPT), "HOME": str(SCRATCH / "home"), "LOSSLESS_PORT": "9",
                   "TZ": "UTC"})
time.tzset()
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT))


def _refuse(*_args, **_kwargs):
    raise AssertionError("the reference run reaches nothing outside its scratch folder")


_popen = subprocess.Popen


def _start(argv, *args, **kwargs):
    """The one program this run starts: bash on the stand-in render script of the scratch folder."""
    if list(argv[:2]) != ["bash", str(RENDER_SCRIPT)]:
        _refuse()
    return _popen(argv, *args, **kwargs)


socket.socket.connect = _refuse
subprocess.Popen = _start

from amui import acquire, cache, cli_library, cli_pipeline, config, doctor, downmix, lossless, web  # noqa: E402

assert config.VAR == SCRATCH / "var" and config.WORKSPACE == SCRATCH / "workspace", "not on the scratch folder"
config.DRM_HELPER = SCRATCH / "no-drm-helper"  # whatever this checkout has built

API = "https://amp-api.music.apple.com"
STOREFRONT = "zz"  # a made-up storefront id
ART = {"url": "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/{w}x{h}bb.jpg", "width": 1200, "height": 1200,
       "bgColor": "102030"}
NS = ('xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" '
      'xmlns:itunes="http://music.apple.com/lyric-ttml-internal"')
LONG = "A name that goes on for much longer than the forty-eight characters a column has"


# --- synthetic resources --------------------------------------------------------
def lib_album(n: int, name: str, artist: str, **attrs) -> dict:
    return {"id": f"l.AbCdEf{n}", "type": "library-albums",
            "attributes": {"name": name, "artistName": artist, "artwork": ART, **attrs}}


def lib_playlist(n: int, name: str, **attrs) -> dict:
    return {"id": f"p.AbCdEf{n}", "type": "library-playlists", "attributes": {"name": name, "canEdit": True, **attrs}}


def lib_song(n: int, name: str, artist: str, millis: int | None, catalog: str | None = None, **attrs) -> dict:
    play = {"id": f"i.AbCdEf{n}", "isLibrary": True, "kind": "song"}
    if catalog:
        play["catalogId"] = catalog
    attributes = {"name": name, "artistName": artist, "albumName": "Album A", "playParams": play, **attrs}
    if millis is not None:
        attributes["durationInMillis"] = millis
    return {"id": f"i.AbCdEf{n}", "type": "library-songs", "attributes": attributes}


def lib_artist(n: int, name: str) -> dict:
    return {"id": f"r.AbCdEf{n}", "type": "library-artists", "attributes": {"name": name}}


def song(song_id: str, name: str, artist: str, millis: int, traits: list[str] | None = None, **attrs) -> dict:
    attributes = {"name": name, "artistName": artist, "albumName": "Album A", "durationInMillis": millis,
                  "playParams": {"id": song_id, "kind": "song"},
                  "url": f"https://music.apple.com/{STOREFRONT}/album/album-a/1000000010?i={song_id}", **attrs}
    if traits is not None:
        attributes["audioTraits"] = traits
    return {"id": song_id, "type": "songs", "attributes": attributes}


def album(album_id: str, name: str, artist: str, **attrs) -> dict:
    return {"id": album_id, "type": "albums", "attributes": {"name": name, "artistName": artist, "artwork": ART, **attrs}}


def playlist(playlist_id: str, name: str, curator: str | None, **attrs) -> dict:
    attributes = {"name": name, **attrs}
    if curator:
        attributes["curatorName"] = curator
    return {"id": playlist_id, "type": "playlists", "attributes": attributes}


def artist(artist_id: str, name: str) -> dict:
    return {"id": artist_id, "type": "artists", "attributes": {"name": name, "genreNames": ["Genre A"]}}


WORD_TTML = f"""<?xml version="1.0" encoding="UTF-8"?>
<tt {NS} xml:lang="en" itunes:timing="Word">
 <head><metadata><ttm:agent type="person" xml:id="v1"/><ttm:agent type="person" xml:id="v2"/></metadata></head>
 <body dur="2:10.000">
  <div begin="0:01.000" end="0:10.000">
   <p begin="0:01.000" end="0:04.000" ttm:agent="v1"><span begin="0:01.000" end="0:01.500">First</span> <span begin="0:01.500" end="0:04.000">line</span></p>
   <p begin="0:04.500" end="0:08.000" ttm:agent="v2"><span begin="0:04.500" end="0:05.000">Second</span> <span begin="0:05.000" end="0:08.000">line</span></p>
   <p begin="1:05.250" end="1:08.000" ttm:agent="v1" ttm:role="x-bg"><span begin="1:05.250" end="1:06.000">(in</span> <span begin="1:06.000" end="1:08.000">back)</span></p>
   <p begin="2:00.000" end="2:09.999" ttm:agent="v1"><span begin="2:00.000" end="2:09.999">Last</span></p>
  </div>
 </body>
</tt>
"""
PLAIN_TTML = f"""<tt {NS} itunes:timing="None">
<body><div><p>One untimed line</p><p>Another untimed line</p></div></body></tt>
"""

ASSET_URL = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_hvc-abc_m.m3u8?a=1&sig=xyz"
MASTER_URL = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ab/cd/P1000_default.m3u8?a=1&sig=xyz"
ASSET_URL_2 = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ef/gh/P2000_hvc-abc_m.m3u8?a=1&sig=xyz"
MASTER_URL_2 = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ef/gh/P2000_default.m3u8?a=1&sig=xyz"
# what the wrapper service hands out: the account's device playlists (plain links)
DEVICE_MASTER_URL = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/ij/kl/P3000_default.m3u8"
LOSSY_MASTER_URL = "https://aod.itunes.apple.com/itunes-assets/HLSMusic/v4/mn/op/P4000_default.m3u8"
AAC_FILE_URL = "https://streamingaudio.itunes.apple.com/itunes-assets/Music/v4/ab/cd/mzaf_1000.aac.a.m4p?accessKey=k"


def master(groups: list[tuple[str, str, int]]) -> str:
    lines = ["#EXTM3U", "#EXT-X-VERSION:7", "#EXT-X-INDEPENDENT-SEGMENTS"]
    for group, codecs, bandwidth in groups:
        lines.append(f'#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH={bandwidth - 100},BANDWIDTH={bandwidth},CODECS="{codecs}",'
                     f'AUDIO="{group}"')
        lines.append(f"{group}/P1000_default.m3u8?a=1&sig=xyz")
    return "\n".join(lines) + "\n"


def assets(song_id: str) -> str:
    query = f"id={song_id}&kind=song&includeLicenseUrls=true&hlsEncryption=CBC&hlsProfile=enhancedHls"
    return f"{API}/v1/play/assets?{query}"


CATALOG = f"{API}/v1/catalog/{STOREFRONT}"
SEARCH_TYPES = "types=library-songs,library-albums,library-artists,library-playlists"
SONG_A = song("1000000001", "Song A", "Artist A", 201000, ["atmos", "lossless"], trackNumber=3, discNumber=1,
              genreNames=["Genre A"], releaseDate="2020-05-06", isrc="ZZZZZ2000001", contentRating="clean",
              hasLyrics=True, artwork=ART)
SONG_A["relationships"] = {"albums": {"data": [{"id": "1000000010", "type": "albums"}]},
                           "artists": {"data": [{"id": "2000000001", "type": "artists"}]}}
SONG_B = song("1000000002", "Song B", "Artist B", 59999, ["lossless"])
SONG_C = song("1000000003", "Song C", "Artist A", 3599000)
BARE_SONG = {"id": "1000000004", "type": "songs", "attributes": {"name": "Song D", "artistName": "Artist B"}}

ANSWERS: dict[str, object] = {
    f"{API}/v1/me/storefront": {"data": [{"id": STOREFRONT, "type": "storefronts"}]},
    # --- library pages
    f"{API}/v1/me/library/albums?limit=100": {
        "data": [lib_album(1, "Album A", "Artist A", trackCount=12),
                 lib_album(2, LONG, "An artist whose name is longer than its column", trackCount=0),
                 lib_album(3, "Álbum\nÑ", "Ártist Ö")],
        "next": "/v1/me/library/albums?offset=100", "meta": {"total": 257}},
    f"{API}/v1/me/library/albums?limit=2&offset=100": {
        "data": [lib_album(4, "Album D", "Artist A", trackCount=1)], "meta": {"total": 257}},
    f"{API}/v1/me/library/albums?limit=5&offset=300": {"data": []},
    f"{API}/v1/me/library/playlists?extend[library-playlists]=tags&limit=100": {
        "data": [lib_playlist(1, "List A"),
                 lib_playlist(2, "Favourite Songs", canEdit=False, tags=["favorited"], trackCount=40),
                 {"id": "p.AbCdEf3", "type": "library-playlists",
                  "attributes": {"name": "List C", "playParams": {"id": "p.AbCdEf3", "globalId": "pl.u-abc"}}}]},
    f"{API}/v1/me/library/songs?limit=100": {
        "data": [lib_song(1, "Song A", "Artist A", 201000, "1000000001"),
                 lib_song(2, "Song B", "Artist B", 59999),
                 lib_song(3, "Song C", "Artist A", None, "1000000003")],
        "next": "/v1/me/library/songs?offset=3"},
    f"{API}/v1/me/library/artists?include=catalog&limit=100": {
        "data": [lib_artist(1, "Artist A"), lib_artist(2, "Artist B")], "meta": {"total": 2}},
    f"{API}/v1/me/library/recently-added?limit=25": {
        "data": [lib_album(1, "Album A", "Artist A", trackCount=12), lib_playlist(1, "List A"),
                 {"id": "x.1", "type": "library-music-videos", "attributes": {"name": "A Video"}}],
        "next": "/v1/me/library/recently-added?offset=25"},
    # --- tracks
    f"{API}/v1/me/library/albums/l.AbCdEf1/tracks?limit=100": {
        "data": [lib_song(1, "Song A", "Artist A", 201000, "1000000001"), lib_song(2, "Song B", "Artist B", 59999)]},
    f"{API}/v1/me/library/playlists/p.AbCdEf1/tracks?limit=2&offset=1": {
        "data": [lib_song(2, "Song B", "Artist B", 59999), lib_song(3, "Song C", "Artist A", 3599000, "1000000003")],
        "next": "/v1/me/library/playlists/p.AbCdEf1/tracks?offset=3", "meta": {"total": 9}},
    f"{CATALOG}/playlists/pl.abc123?include=tracks": {
        "data": [dict(playlist("pl.abc123", "List P", "Curator A"),
                      relationships={"tracks": {"data": [SONG_A, SONG_B]}})]},
    f"{CATALOG}/playlists/pl.u-abc?include=tracks": {
        "data": [dict(playlist("pl.u-abc", "List Q", None), relationships={"tracks": {"data": []}})]},
    f"{CATALOG}/albums/1000000010?include=tracks": {
        "data": [dict(album("1000000010", "Album A", "Artist A", audioTraits=["atmos", "lossless"],
                            releaseDate="2020-05-06", trackCount=3),
                      relationships={"tracks": {"data": [SONG_A, SONG_B, SONG_C]}})]},
    f"{CATALOG}/albums/1000000011?include=tracks": {
        "data": [dict(album("1000000011", "Album B", "Artist B"), relationships={"tracks": {"data": [BARE_SONG]}})]},
    # --- search
    f"{API}/v1/me/library/search?term=song+a&{SEARCH_TYPES}&limit=3": {"results": {
        "library-songs": {"data": [lib_song(1, "Song A", "Artist A", 201000, "1000000001")]},
        "library-albums": {"data": [lib_album(1, "Album A", "Artist A", trackCount=12)]},
        "library-playlists": {"data": [lib_playlist(1, "List A")]}}},
    f"{CATALOG}/search?term=song+a&types=songs,albums,artists,playlists&limit=3&with=topResults": {"results": {
        "topResults": {"data": [artist("2000000001", "Artist A")]},
        "songs": {"data": [SONG_A, SONG_B]},
        "albums": {"data": [album("1000000010", "Album A", "Artist A", trackCount=3)]},
        "artists": {"data": [artist("2000000001", "Artist A")]},
        "playlists": {"data": [playlist("pl.abc123", "List P", "Curator A", trackCount=25)]}}},
    f"{API}/v1/me/library/search?term=nothing&{SEARCH_TYPES}&limit=5": {"results": {}},
    f"{CATALOG}/search?term=nothing&types=songs,albums,artists,playlists&limit=5&with=topResults": {"results": {
        "top": {"data": [SONG_B]}}},
    # --- metadata and lyrics
    f"{CATALOG}/songs/1000000001": {"data": [SONG_A]},
    f"{CATALOG}/songs/1000000004": {"data": [BARE_SONG]},
    f"{CATALOG}/songs/1000000001?include=lyrics,syllable-lyrics&extend=ttmlLocalizations": {
        "data": [dict(SONG_A, relationships={
            "syllable-lyrics": {"data": [{"id": "1000000001", "type": "syllable-lyrics",
                                          "attributes": {"ttmlLocalizations": WORD_TTML}}]},
            "lyrics": {"data": [{"id": "1000000001", "type": "lyrics", "attributes": {"ttml": PLAIN_TTML}}]}})]},
    f"{CATALOG}/songs/1000000002?include=lyrics,syllable-lyrics&extend=ttmlLocalizations": {
        "data": [dict(SONG_B, relationships={
            "lyrics": {"data": [{"id": "1000000002", "type": "lyrics", "attributes": {"ttml": PLAIN_TTML}}]}})]},
    f"{CATALOG}/songs/1000000003?include=lyrics,syllable-lyrics&extend=ttmlLocalizations": {"data": [SONG_C]},
    # --- home
    f"{API}/v1/me/recent/played?types=albums,playlists&limit=4": {
        "data": [album("1000000010", "Album A", "Artist A", trackCount=3), playlist("pl.u-abc", "List Q", None)]},
    f"{API}/v1/me/recent/played?types=albums,playlists&limit=10": {
        "data": [album("1000000010", "Album A", "Artist A", trackCount=3),
                 playlist("pl.abc123", "List P", "Curator A", trackCount=25),
                 {"id": "ra.1", "type": "stations", "attributes": {"name": "Station A"}}]},
    f"{API}/v1/me/recent/played/tracks?types=songs&limit=10": {"data": [SONG_B, SONG_A]},
    f"{API}/v1/me/recommendations": {"data": [
        {"id": "6-27s5hU6azhJY", "type": "personal-recommendation",
         "attributes": {"title": {"stringForDisplay": "Made for You"}},
         "relationships": {"contents": {"data": [playlist("pl.abc123", "List P", "Curator A"),
                                                 album("1000000010", "Album A", "Artist A"),
                                                 playlist("pl.u-abc", "List Q", None)]}}},
        {"id": "6-27s5hU6azhJZ", "type": "personal-recommendation", "attributes": {"title": LONG},
         "relationships": {"contents": {"data": [album("1000000011", "Album B", "Artist B")]}}},
        {"id": "6-27s5hU6azhJ0", "type": "personal-recommendation", "attributes": {"title": "Stations only"},
         "relationships": {"contents": {"data": [{"id": "ra.1", "type": "stations", "attributes": {"name": "S"}}]}}}]},
    f"{CATALOG}/charts?types=albums,songs,playlists&limit=20": {"results": {
        "songs": [{"chart": "most-played", "name": "Top Songs", "data": [SONG_A, SONG_C]}],
        "albums": [{"chart": "most-played", "name": "Top Albums",
                    "data": [album("1000000010", "Album A", "Artist A", trackCount=3)]}],
        "playlists": [{"chart": "most-played", "data": [playlist("pl.abc123", "List P", "Curator A")]}]}},
    # --- what Apple refuses
    f"{API}/v1/me/library/songs?limit=7": {"__refused__": 401},
    # --- manifests
    assets("1000000001"): {"results": {"assets": [{"url": ASSET_URL}]}},
    MASTER_URL: {"__text__": master([("audio-stereo-256", "mp4a.40.2", 270424), ("audio-atmos-2768", "ec-3", 772001),
                                     ("audio-atmos-2448", "ec-3", 451640)])},
    assets("1000000002"): {"results": {"assets": [{"url": ASSET_URL_2}]}},
    MASTER_URL_2: {"__text__": master([("audio-stereo-256", "mp4a.40.2", 270424)])},
    assets("1000000003"): {"results": {"assets": []}},
    DEVICE_MASTER_URL: {"__text__": master([
        ("audio-stereo-256", "mp4a.40.2", 270717), ("audio-atmos-2768", "ec-3", 772543),
        ("audio-alac-stereo-44100-16", "alac", 900000), ("audio-alac-stereo-48000-24", "alac", 1995449),
        ("audio-alac-stereo-96000-24", "alac", 3800000), ("audio-alac-stereo-192000-24", "alac", 7000999)])},
    LOSSY_MASTER_URL: {"__text__": master([("audio-stereo-256", "mp4a.40.2", 270717), ("audio-atmos-2768", "ec-3", 772543)])},
}


def fake_request(method: str, url: str, *, auth: bool, **_options) -> bytes:
    host = web.validate_url(url, authenticated=auth)
    answer = ANSWERS.get(url)
    if method != "GET" or answer is None:
        raise web.NotFound(f"{host} returned HTTP 404")
    if isinstance(answer, dict) and "__refused__" in answer:
        status = answer["__refused__"]
        raise web.AuthError(f"{host} rejected the session (HTTP {status}); reconnect to Apple Music", status)
    if isinstance(answer, dict) and "__text__" in answer:
        return answer["__text__"].encode()
    return json.dumps(answer).encode()


web._request = fake_request


class Clock:
    """`time` for amui.cli_pipeline: a clock that moves by `step` at every look (it stands still at
    0), the real calendar."""

    now, step = 100.0, 0.0
    strftime = staticmethod(time.strftime)
    localtime = staticmethod(time.localtime)

    @staticmethod
    def monotonic() -> float:
        Clock.now += Clock.step
        return Clock.now


cli_pipeline.time = Clock

# --- the sink: a table instead of PipeWire ------------------------------------------
tool = downmix.workspace_module()
NODE_ID = 38
SINK: dict[str, float] = {}  # the controls of the downmix node, as the last `set-param` left them


def shown(params: dict) -> str:
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        tool.show(params)
    return out.getvalue()


def set_controls(params: dict) -> None:
    """What `pw-cli set-param` leaves on the node: the values in the spelling the tool hands over."""
    SINK.update({name: float(tool.node_value(value)) for name, value in tool.controls(params).items()})


class ToolRun:
    """`subprocess` for amui.downmix: the tool's `apply` command without a sink.

    The engine runs `downmix.py apply <key> --preamp <dB>`; the tool finds the node, sets its
    controls and prints the node and the matrix (crates/engine/tests/reference/generate.py holds the
    same account of it)."""

    DEVNULL = subprocess.DEVNULL
    TimeoutExpired = subprocess.TimeoutExpired
    SubprocessError = subprocess.SubprocessError

    @staticmethod
    def run(command, **_options):
        assert command[1:3] == [str(config.DOWNMIX_MODULE), "apply"] and command[4] == "--preamp", command
        params = tool.resolve(argparse.Namespace(preset=command[3], preamp=float(command[5])))
        set_controls(params)
        return subprocess.CompletedProcess(command, 0, f"applied to node {NODE_ID} ({tool.NODE})\n" + shown(params), "")


downmix.subprocess = ToolRun
downmix.sink_node_id = lambda: NODE_ID  # a sink that runs
# what `pw-cli enum-params` lists: every value with six decimals
downmix.live_controls = lambda: {name: float(f"{value:.6f}") for name, value in SINK.items()}

# --- the wrapper service: a table instead of the service on loopback ----------------
SERVICE: dict = {"regions": None, "dispatch": {}}  # the storefronts it reports (None: nothing answers), its /m3u8 data


def service_get(path: str, timeout: float) -> dict:
    if path != "/status":
        return SERVICE["dispatch"]
    if SERVICE["regions"] is None:
        raise lossless.LosslessUnavailable("the lossless service does not answer (ConnectionRefusedError)")
    return {"regions": SERVICE["regions"]}


lossless._get_json = service_get

COMMANDS = {**cli_library.COMMANDS, **cli_pipeline.COMMANDS}
TIME = re.compile(r"used \d{4}-\d\d-\d\d \d\d:\d\d")
PROGRESS = re.compile(r"(?:  t\+ *[0-9.]+s +[0-9]+ frames +[0-9.]+s[^\n]*\n)+")


def last_progress(text: str) -> str:
    """Of every run of render progress lines, the last line."""
    return PROGRESS.sub(lambda lines: lines.group(0).splitlines(keepends=True)[-1], text)


def run(argv: list[str]) -> dict:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            status = int(COMMANDS[argv[0]][1](argv[1:]) or 0)
        except SystemExit as stop:  # what the interpreter does with it at the top
            if isinstance(stop.code, int):
                status = stop.code
            else:
                print(stop.code, file=sys.stderr)
                status = 1
    acquire.forget_atmos()

    def clean(text: str) -> str:
        text = text.replace(str(config.VAR), "<var>").replace(str(SCRATCH), "<scratch>")
        return last_progress(TIME.sub("used <time>", text))

    return {"argv": argv, "status": status, "stdout": clean(out.getvalue()), "stderr": clean(err.getvalue())}


# --- the states of the scratch folder -----------------------------------------------
TOKEN = "A" * 40 + "synthetic"  # stands for a sign-in; the Rust tests write their own


def write(path: Path, text: str, mode: int = 0o600) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.write_text(text)
    path.chmod(mode)


def reset(state: dict) -> None:
    """A fresh data folder and workspace state, then what `state` describes."""
    shutil.rmtree(config.VAR, ignore_errors=True)
    shutil.rmtree(config.WORKSPACE / "builds", ignore_errors=True)
    web.reset_token_sources()
    config.ensure_dirs()
    lossless.clear_backoff()
    SERVICE.update(regions=None, dispatch={})
    if "lossless" in state:  # a built route: the rip helper, next to the control script of the repository
        route = state["lossless"]
        write(config.LOSSLESS_RIP_HELPER, "stand-in\n")
        for name in ("MUSIC_TOKEN", "STOREFRONT_ID", "DEV_TOKEN") if route["signed_in"] else ():
            write(config.LOSSLESS_DATA_DIR / name, TOKEN + "\n")
        SERVICE.update(regions=route["regions"], dispatch=route.get("dispatch", {}))
    SINK.clear()
    set_controls(tool.resolve(argparse.Namespace(preset="peqdb", preamp=-11.0)))  # as the installed sink starts
    RENDER_SCRIPT.unlink(missing_ok=True)
    if "render_script" in state:
        RENDER_SCRIPT.write_text(state["render_script"])
    for signin in state.get("signins", []):
        write(web._android_token_file() if signin == "android" else config.TOKEN_FILE, TOKEN + "\n")
    if "android_status" in state:
        write(config.ANDROID_SESSION_STATUS_FILE, state["android_status"])
    if "browser_status" in state:
        write(config.SESSION_STATUS_FILE, state["browser_status"])
    media = cache.MediaCache()
    for entry in state.get("media", []):
        source = config.TMP_DIR / "reference-media"
        source.write_bytes(b"\0" * entry["bytes"])
        media.put(entry["key"], source, entry["meta"])
        time.sleep(0.01)  # one after the other, for the order of the listing
    pcm = cache.PcmCache()
    for entry in state.get("pcm", []):
        directory = pcm.claim(entry["key"])
        for name, content in entry["files"].items():
            data = b"\0" * content["zeros"] if "zeros" in content else json.dumps(content["json"]).encode()
            (directory / name).write_bytes(data)
        time.sleep(0.01)
    art = cache.ArtCache()
    for entry in state.get("art", []):
        art.put(entry["url"], b"\0" * entry["bytes"])


def group(name: str, state: dict, command_lines: list[list[str]]) -> dict:
    reset(state)
    return {"name": name, "state": state, "cases": [run(argv) for argv in command_lines]}


ATMOS_META = {"catalog_id": "1000000001", "variant": "atmos", "audio_group": "audio-atmos-2768", "codecs": "ec-3",
              "bandwidth": "772001", "segments": 14, "encrypted_bytes": 5000, "bytes": 4096, "acquired_at": 1767323045.5,
              "duration_s": 201.386667, "codec": "eac3", "profile": None, "bit_rate": 768000, "sample_rate": 48000,
              "channels": 6, "container_bit_rate": 771234}
LOSSLESS_META = {"catalog_id": "1000000002", "variant": "lossless", "audio_group": "audio-alac-stereo-48000-24",
                 "codecs": "alac", "bandwidth": "1995349", "bytes": 1500000, "acquired_at": 1767323046.5,
                 "duration_s": 59.999, "codec": "alac", "profile": None, "bit_rate": 1900000, "sample_rate": 48000,
                 "channels": 2, "container_bit_rate": 1905000, "bit_depth": 24}
STEREO_META = {"catalog_id": "1000000002", "variant": "stereo", "audio_group": "audio-stereo-256", "codecs": "mp4a.40.2",
               "bandwidth": "270424", "bytes": 2048, "duration_s": 60, "codec": "aac", "profile": "LC",
               "bit_rate": 256000, "sample_rate": 44100, "channels": 2}
SUMMARY = {"frames": 480, "bytes": 480 * 64, "channels": 16, "sample_rate": 48000, "channel_fallback_blocks": 0,
           "nonfinite_samples": 0, "elapsed_seconds": 0.5, "realtime_factor": 20.0}
DECODE_META = {"channels": 2, "frames": 24000, "sample_rate": 48000, "source": "aac", "source_sample_rate": 44100,
               "resampled": True, "codec": "aac", "profile": "LC", "bit_rate": 256000, "duration_s": 0.5,
               "source_duration_s": 0.5}
CACHES = {
    "media": [{"key": "1000000001.atmos", "bytes": 4096, "meta": ATMOS_META},
              {"key": "1000000002.lossless", "bytes": 1500000, "meta": LOSSLESS_META},
              {"key": "1000000002.stereo", "bytes": 2048, "meta": STEREO_META},
              {"key": "1000000005.stereo", "bytes": 100, "meta": {}}],
    "pcm": [{"key": "1000000001.atmos", "files": {"atmos-916.f32le": {"zeros": 480 * 64},
                                                  "render-summary.json": {"json": SUMMARY}}},
            {"key": "1000000002.stereo", "files": {"stereo.f32le": {"zeros": 24000 * 8}, "meta.json": {"json": DECODE_META}}},
            {"key": "1000000005.stereo", "files": {"stereo.f32le": {"zeros": 1001}}}],
    "art": [{"url": "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/300x300bb.jpg", "bytes": 1234},
            {"url": "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/600x600bb.jpg", "bytes": 2048}],
}
LIBRARY = {"playlists": {"accessible": True, "sample_count": 1, "has_more": True},
           "songs": {"accessible": True, "sample_count": 0, "has_more": False}, "odd": "not a dictionary",
           "albums": {"accessible": None}}

# A stand-in renderer: a tenth of a second of silence and the summary a render ends with.
RENDER_SUMMARY = {
    "frames": 4800, "bytes": 4800 * 64, "channels": 16, "sample_rate": 48000, "channel_fallback_blocks": 0,
    "nonfinite_samples": 0, "elapsed_seconds": 0.123456, "realtime_factor": 23.4567, "duration_seconds": 0.1,
    "joc_object_count_min": 11, "joc_object_count_max": 15, "oamd_object_count_min": 13, "rendered_object_blocks": 42,
    "channels_stats": [{"name": "FL", "peak": 0.25, "nonzero_samples": 100},
                       {"name": "FR", "peak": 0.123456789, "nonzero_samples": 7},
                       {"name": "LFE", "peak": 0.5, "nonzero_samples": 0}, {"name": "TFL", "nonzero_samples": 0}]}
RENDERS = f"""set -eu
out=$2
echo "Rendering: stand-in"
head -c {4800 * 64} /dev/zero > "$out/atmos-916.f32le"
cat > "$out/render-summary.json" <<'EOF'
{json.dumps(RENDER_SUMMARY)}
EOF
echo "Finished: stand-in"
"""
RENDERS_QUIETLY = RENDERS.replace(json.dumps(RENDER_SUMMARY), json.dumps(
    {"frames": 4800, "bytes": 4800 * 64, "channels": 16, "sample_rate": 48000, "elapsed_seconds": 2, "channels_stats": [
        {"name": "FL", "peak": "0.5", "nonzero_samples": 1}]}))
FAILS = """set -eu
echo "Rendering: stand-in"
echo "Unsupported stream layout" >&2
exit 3
"""
RENDER_MEDIA = [{"key": "1000000006.atmos", "bytes": 64, "meta": {"duration_s": 0.1, "codec": "eac3"}},
                {"key": "1000000007.atmos", "bytes": 64, "meta": {}}]

GROUPS = [
    group("library", {"signins": ["android"]}, [
        ["albums"], ["albums", "--offset", "100", "--limit", "2"], ["albums", "--limit", "5", "--offset", "300"],
        ["playlists"], ["songs"], ["artists"], ["recently-added"],
        ["tracks", "l.AbCdEf1"], ["tracks", " p.AbCdEf1 ", "--offset", "1", "--limit", "2"], ["tracks", "pl.abc123"],
        ["tracks", "pl.u-abc"], ["tracks", "1000000010"], ["tracks", "1000000011"], ["tracks", "x.123"],
        ["tracks", "l.missing"], ["tracks", "p.missing"],
        ["search", "song", "a", "--limit", "3"], ["search", "nothing"],
        ["song", "1000000001"], ["song", "1000000004"], ["song", "1000000404"],
        ["lyrics", "1000000001"], ["lyrics", "1000000001", "--show"], ["lyrics", "1000000002", "--show"],
        ["lyrics", "1000000003"], ["lyrics", "1000000404"],
        ["recent", "--limit", "4"], ["recent", "--limit", "7"], ["home"], ["charts"],
        ["songs", "--limit", "7"],
    ]),
    group("no sign-in", {}, [["status"]]),
    group("native sign-in, checked", {
        "signins": ["android"],
        "android_status": json.dumps({"state": "connected", "checked_at": "2026-01-02T03:04:05.678901+00:00",
                                      "library": LIBRARY, "token": "not a key that is read"})}, [["status"]]),
    group("native sign-in, not checked", {"signins": ["android", "browser"]}, [["status"]]),
    group("browser session, rejected", {
        "signins": ["browser"],
        "browser_status": json.dumps({"state": "reconnect_required", "checked_at": "2026-01-02T03:04:05+00:00",
                                      "captured_at": "2025-12-01T00:00:00+00:00",
                                      "cookie_expires_at": "2026-06-01T00:00:00+00:00", "http_status": 403,
                                      "source": "browser"})}, [["status"]]),
    group("browser session, odd status file", {
        "signins": ["browser"], "browser_status": json.dumps({"state": "dancing", "library": ["a", "list"]})},
        [["status"]]),
    group("caches", CACHES, [
        ["cache"], ["acquire", "1000000001"], ["acquire", "1000000002", "--lossless"],
        ["acquire", "https://music.apple.com/zz/album/album-a/1000000010?i=1000000002", "--stereo"],
        ["acquire", "1000000003"], ["acquire", "https://music.apple.com/zz/album/album-a/1000000010"],
        ["acquire", "https://example.com/zz/song/song-a/1000000001"],
        ["render", "1000000001"], ["render", "1000000002"], ["decode", "1000000002"],
        ["decode", "1000000001"], ["decode", "1000000005", "--lossless"], ["decode", "nothing"],
        ["cache", "--evict"],
    ]),
    group("empty caches", {}, [
        ["cache"], ["lossless"], ["lossless", "--check", "1000000001"], ["lossless", "--start"],
        ["downmix", "--list"], ["downmix"],
        ["atmos-check", "1000000001"], ["atmos-check", "1000000002"], ["atmos-check", "1000000003"],
        ["atmos-check", "1000000404"],
        ["atmos-check", "https://music.apple.com/zz/song/song-a/1000000001"], ["atmos-check", "song-a"],
    ]),
    group("downmix", {}, [
        ["downmix", "--live"], ["downmix", "apple", "--preamp", "-9"], ["downmix"], ["downmix", "--live"],
        ["downmix", "--preamp", "-6.25"], ["downmix", "itu"], ["downmix", "--live", "--list"],
        ["downmix", "zz"], ["downmix", "itu", "--preamp", "30"], ["downmix", "--preamp", "-61"], ["downmix", "--live"],
    ]),
    group("render", {"media": RENDER_MEDIA, "render_script": RENDERS}, [
        ["render", "1000000006"], ["render", "1000000006"], ["render", "1000000006", "--force"],
        ["render", "1000000007"], ["cache"],
    ]),
    group("render, a summary with little in it", {"media": RENDER_MEDIA, "render_script": RENDERS_QUIETLY},
          [["render", "1000000006"]]),
    group("render fails", {"media": RENDER_MEDIA, "render_script": FAILS}, [["render", "1000000006"], ["cache"]]),
    group("no render script", {"media": RENDER_MEDIA}, [["render", "1000000006"]]),
    group("lossless route, not signed in", {"lossless": {"signed_in": False, "regions": None}}, [
        ["lossless"], ["lossless", "--start"], ["lossless", "--check", "1000000001"],
        ["acquire", "1000000001", "--lossless"],
    ]),
    group("lossless route, service signed out", {"lossless": {"signed_in": True, "regions": []}}, [
        ["lossless"], ["lossless", "--start"],
    ]),
    group("lossless route, running", {
        "lossless": {"signed_in": True, "regions": ["100000"],
                     "dispatch": {"adamId": "1000000001", "m3u8": DEVICE_MASTER_URL}}}, [
        ["lossless"], ["lossless", "--start"], ["lossless", "--check", "1000000001"],
        ["lossless", "--start", "--check", "https://music.apple.com/zz/song/song-a/1000000001"],
        ["lossless", "--check", "song-a"],
    ]),
    group("lossless route, a song without a lossless playlist", {
        "lossless": {"signed_in": True, "regions": ["100000", "100001"], "dispatch": {"m3u8": LOSSY_MASTER_URL}}},
        [["lossless", "--check", "1000000002"]]),
    group("lossless route, a song that is only an AAC file", {
        "lossless": {"signed_in": True, "regions": ["100000"], "dispatch": {"m3u8": AAC_FILE_URL}}},
        [["lossless", "--check", "1000000002"]]),
    group("lossless route, a service without an answer for the song", {
        "lossless": {"signed_in": True, "regions": ["100000"]}}, [["lossless", "--check", "1000000002"]]),
]


# --- the pure helpers -------------------------------------------------------------------
def printed(function, *args) -> str:
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        function(*args)
    return out.getvalue()


def track_id(value: str) -> dict:
    try:
        return {"value": value, "id": web.probe.track_id(value)}
    except web.probe.ProbeError as error:
        return {"value": value, "error": str(error)}


def report_lines() -> str:
    report = doctor.Report()
    report.section("A section")
    report.add("ok", "one", "there")
    report.add("MISSING", "two", "needed")
    report.add("MISSING", "three", "nice to have", required=False)
    report.add("absent", "four")
    report.add("MISSING", "an item with a name longer than its column", "x")
    report.section("Another (with: details)")
    report.add("ok", "ffmpeg", "/usr/bin/ffmpeg — no libsoxr (resampling falls back to swr)")
    return printed(report.print) + f"missing: {report.missing_required}\n"


# What an acquisition reports while it works: (phase, fraction).
REPORTS = [["manifest", None], ["license", None], ["license", None], ["download", 0.0], ["download", 0.04],
           ["download", 0.1], ["download", 0.19], ["download", 0.5], ["download", 0.55], ["download", 0.999],
           ["download", 1.0], ["download", 1.0], ["download", None], ["decrypt", None], ["decrypt", None],
           ["download", 0.3], ["done", 1.0]]
STEP = 0.26


def acquire_progress() -> dict:
    """The lines `acquire` prints for REPORTS, on a clock that moves by STEP seconds at every look."""
    from amui.types import AcquireResult

    def scripted(catalog_id, media_cache, *, variant="atmos", progress=None, cancel=None):
        for phase, fraction in REPORTS:
            progress(phase, fraction)
        return AcquireResult(catalog_id=catalog_id, path="nowhere", bytes=0, variant={}, duration_s=None, from_cache=False)

    real, acquire.acquire, Clock.step = acquire.acquire, scripted, STEP
    try:
        lines = run(["acquire", "1000000009"])["stdout"].splitlines(keepends=True)
    finally:
        acquire.acquire, Clock.step = real, 0.0
    assert lines[0] == "acquire 1000000009 (atmos)\n" and lines[-5].startswith("  from_cache: False"), lines
    return {"reports": REPORTS, "step": STEP, "printed": "".join(lines[1:-5])}


TEXTS = [None, True, False, 0, -7, 48000, 18446744073709551615, 2.0, 201.386667, 59.999, 0.5, -0.0, 0.1 + 0.2, 1e15, 1e16,
         1.5e16, 123456789012345680.0, 0.0001, 0.00001, 0.000015, 1e100, 1e-100, 5e-324, 1.7976931348623157e308,
         "Song A", "it's", 'say "hi"', "it's \"both\"", "", "tab\there", "é", [], {}, ["LFE", "TFL"],
         [None, 1, 1.5, [True], {"a": "b"}], {"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"},
         {"bandwidth": None, "sample_rate": 48000, "bit_depth": 24, "nested": {"list": [1.0, "x"]}}]
CLIPS = [("Song A", 48), ("", 5), ("exactly five", 12), ("exactly five", 11), ("two\nlines", 48), ("two\nlines", 5),
         ("Álbum Ñ", 6), ("Álbum Ñ", 7), ("ab", 1), ("abc", 2)]
TABLES = [
    (["key", "name", "artist", "count"], [["l.AbCdEf1", "Album A", "Artist A", "12"]], [24, 48, 32, 6]),
    (["key", "name", "artist", "count"], [], [24, 48, 32, 6]),
    (["#", "name", "artist", "time", "catalog id", "audio"],
     [["1", "Song A", "Artist A", "3:21", "1000000001", "atmos"], ["10", LONG, "", "", "(unavailable)", "?"]],
     [4, 48, 32, 6, 14, 6]),
    (["title", "items", "kinds"], [["Ünïcödé", "3", "album, playlist"], ["x" * 60, "12345678", ""]], [48, 6, 24]),
    (["a", "b"], [["", ""], ["wide cell here", "y"]], None),
]
VALUES = {
    "clip": [{"text": text, "width": width, "clipped": cli_library._clip(text, width)} for text, width in CLIPS],
    "table": [{"headers": headers, "rows": rows, "widths": widths, "printed": printed(cli_library._table, headers, rows, widths)}
              for headers, rows, widths in TABLES],
    "mmss": [{"ms": ms, "text": cli_library._mmss(ms)}
             for ms in (None, 0, 999, 1000, 59999, 60000, 201000, 3599000, 3600000, 86400000, -1, -60001)],
    "hms": [{"seconds": seconds, "text": cli_library._hms(seconds)}
            for seconds in (None, 0.0, 0.0004, 1.0, 5.25, 59.9994, 59.9996, 60.0, 65.25, 599.999, 3599.5, 6000.125, -3.0)],
    "fmt_bytes": [{"size": size, "text": cli_pipeline._fmt_bytes(size)}
                  for size in (0, 1, 1023, 1024, 1536, 4096, 1048575, 1048576, 1500000, 268435456, 1073741823,
                               1073741824, 2147483648, 5497558138880)],
    "song_id": [track_id(value) for value in (
        "1000000001", "0", "", " 1000000001", "1000000001\n", "١٢٣", "12a",
        "https://music.apple.com/zz/song/song-a/1000000001", "https://music.apple.com/zz/song/song-a/1000000001/",
        "https://music.apple.com/zz/album/album-a/1000000010?i=1000000002",
        "https://music.apple.com/zz/album/album-a/1000000010?i=1000000002&l=en",
        "https://music.apple.com/zz/album/album-a/1000000010?i=&i=1000000003",
        "https://music.apple.com/zz/album/album-a/1000000010?i=%31%32", "https://music.apple.com/zz/album/album-a/1000000010",
        "https://music.apple.com/zz/album/album-a/1000000010?i=abc", "https://music.apple.com/zz/album/album-a/1000000010#i=5",
        "https://music.apple.com/zz/playlist/list-p/pl.abc123", "https://music.apple.com/song/1000000001",
        "https://music.apple.com/zz/song/1000000001", "https://music.apple.com/zz/song/song-a/x1",
        "https://beta.music.apple.com/zz/song/song-a/1000000001", "HTTPS://MUSIC.APPLE.COM/zz/song/song-a/1000000001",
        "http://music.apple.com/zz/song/song-a/1000000001", "https://example.com/zz/song/song-a/1000000001",
        "https://music.apple.com.example.com/zz/song/song-a/1000000001", "music.apple.com/zz/song/song-a/1000000001",
        "https://music.apple.com:443/zz/song/song-a/1000000001", "song-a")],
    "report": report_lines(),
    "acquire_progress": acquire_progress(),
    # how an f-string and a dictionary print a value that came out of a JSON file
    "python_text": [{"value": value, "str": str(value), "repr": repr(value)} for value in TEXTS],
}

ABOUT = ("What the Python command line (amui/cli_library.py, amui/cli_pipeline.py; the helpers of both, the probe "
         "module's track_id and the doctor's report lines) printed for these synthetic answers and states when the "
         "Rust port was written. Made by tests/reference/cli_reference.py; the tests of src/cli/reference.rs hold "
         "the port to it.")
json.dump({"about": ABOUT, "answers": ANSWERS, "groups": GROUPS, "values": VALUES}, sys.stdout, indent=1,
          ensure_ascii=False)
sys.stdout.write("\n")
shutil.rmtree(SCRATCH, ignore_errors=True)
