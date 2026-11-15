# amui API layer — notes (2026-11-07/08)

Files: `amui/api.py` (MusicApi), `amui/lyrics.py` (TTML parser), `amui/session.py`
(status/check/connect), `amui/cli_library.py` (`python3 -m amui …` commands),
`tests/test_lyrics.py`, `tests/test_api_parsing.py`. All network goes through
`amui.web` (saved cookie + public developer token, API host only). No token
values, signed URLs, response bodies or lyric text are logged or stored; this
file records shapes and counts only.

## Verified JSON field names (live, read-only, web session)

Resource ids: catalog songs/albums/artists are numeric strings; library songs
`i.…`, library albums `l.…`, library playlists `p.…`, library artists `r.…`,
catalog playlists `pl.…` (a user's own playlists surface in catalog form as
`pl.u-…`), stations `ra.…` (skipped).

| Resource | `type` | attributes used | relationships |
| --- | --- | --- | --- |
| library song | `library-songs` | `name artistName albumName durationInMillis trackNumber discNumber hasLyrics contentRating genreNames[0] releaseDate artwork{url,width,height} playParams{catalogId,id,isLibrary,kind,reportingId}` (no `audioTraits`, no `dateAdded`, no `isrc`) | none by default; `include=albums` → `albums.data[0].id` (`l.…`) |
| library album | `library-albums` | `name artistName artwork dateAdded genreNames releaseDate trackCount playParams{id,isLibrary,kind}` — **no `playParams.catalogId`** | `include=catalog` → `catalog.data[0]` = full catalog album (id + `audioTraits`) |
| library playlist | `library-playlists` | `name artwork canEdit canDelete dateAdded hasCatalog isPublic lastModifiedDate description{standard,short}? playParams{globalId("pl.…"),id,isLibrary,kind}` — no `trackCount` | `include=catalog` → `catalog.data[0]` (`playlists`, `pl.u-…`) |
| library artist | `library-artists` | `name` only (no artwork) | — |
| catalog song | `songs` | `name artistName albumName albumArtistName durationInMillis trackNumber discNumber hasLyrics hasTimeSyncedLyrics audioTraits[] contentRating? genreNames isrc releaseDate artwork{url,width,height,bgColor,textColor1..4} playParams{id,kind} url` | single GET includes `albums.data[0].id` and `artists.data[0].id`; embedded songs (album tracks, search, charts) have **no relationships** → album id parsed from `attributes.url` (`/album/<slug>/<albumId>?i=<songId>`) |
| catalog album | `albums` | `name artistName artwork audioTraits trackCount releaseDate genreNames isSingle isCompilation isComplete contentRating? copyright recordLabel upc url playParams` | `artists.data[0].id`; `tracks.data` (+ `tracks.next` for >300) |
| catalog artist | `artists` | `name artwork genreNames url` | `albums` (25 + next); `views.top-songs / full-albums / singles .data`, each with `attributes.title` (localised) |
| catalog playlist | `playlists` | `name artwork audioTraits curatorName description{short,standard} lastModifiedDate playlistType url playParams{id,kind,versionHash}` (no `trackCount`) | `curator`, `tracks.data` (+ `next`) |
| recommendation | `personal-recommendation` | `title.stringForDisplay`, `kind`, `resourceTypes` | `contents.data` mixed (`albums`, `playlists`, `stations`) |
| lyrics | `lyrics` / `syllable-lyrics` | `ttml` (str) / `ttmlLocalizations` (str) | — |

Pagination: `next` is a relative path with `?offset=N` (plus the original
query, e.g. `&sort=-dateAdded`); `meta.total` is present on `/v1/me/library/*`
and `/tracks` pages, absent on `recently-added`, `recent/played`, views.
Search: `results.songs|albums|artists|playlists.{data,next}` and
`results.topResults.data` (mixed) with `with=topResults`; library search uses
`results["library-songs"]` etc. and omits types with no hits; `meta.results.order`.
Charts: `results.albums|songs|playlists` are **lists** of chart objects
`{name, chart, data, next}`; the first is "most-played".

Artwork templates seen: `…/{w}x{h}bb.jpg` (albums/songs) and `…/{w}x{h}cc.jpg`
(library playlists) — no `{c}`/`{f}` placeholders in practice; `Artwork.url()`
handles both forms (tested).

Limits: library pages 100; `recently-added` 25 (HTTP 400 above); catalog and
library search 25; `recent/played` and `recent/played/tracks` 10 (400 above,
paged with `offset`); artist views accept `limit[top-songs]=20`,
`limit[full-albums]=50`, `limit[singles]=50`. `sort=-dateAdded` works on
`/v1/me/library/albums`. Comma lists stay literal in the query (`safe=",[]"`).
No `platform=web` or `l=` parameter was needed for `audioTraits`/lyrics.

## Mapping rules (api.parse_track / parse_album / parse_playlist / parse_artist / parse_item)

* Library song: `library_id = id`, `catalog_id = playParams.catalogId` (None for
  an unmatched upload → `Track.playable` False), `audio_traits = ()` so
  `atmos_known` is None — resolve with `catalog_song()` when the badge matters.
  `album_library_id` only with `library_songs(include_albums=True)`.
* Library album: `library_id = id`, `catalog_id` None unless
  `library_albums(include_catalog=True)` (then also `audio_traits` from the
  catalog album). `Album.key` is the library id.
* Library playlist: `library_id = id`, `catalog_id = playParams.globalId`
  (`pl.u-…`), `track_count` None (use `Page.total` of
  `library_playlist_tracks`), `can_edit` from `canEdit`, `description.standard`.
* Catalog song: `catalog_id = id`; `album_catalog_id` from relationships or the
  `url`; `artist_catalog_id` only when relationships are present. Tracks from
  `catalog_album()` get `album_catalog_id` = the album's id.
* `parse_item` skips unsupported types (stations, music videos, curators).

## Contract additions / deviations

* `MusicApi(cache_dir, *, ttl=60.0)`; `clear_cache()`; in-memory TTL cache per
  full URL (library 60 s, catalog metadata 600 s, lyrics 3600 s, max 512
  entries); storefront persisted in `cache_dir/storefront.json` (0600, dir
  0700, re-fetched after 7 days). Errors are never cached; `web.AuthError`,
  `web.NotFound`, `web.RateLimited` propagate unchanged.
* `library_albums(offset, limit, *, sort=None, include_catalog=False)`,
  `library_songs(offset, limit, *, include_albums=False)` — optional kwargs;
  `include_albums` doubles the page size and adds ~60 % latency (0.8 → 1.3 s
  per 100 songs), hence off by default.
* `library_playlist_tracks` maps HTTP 404 to an empty page (Apple's answer for
  a playlist without tracks; **not reproducible live** — this library has none).
* `catalog_artist` requests `views=top-songs,full-albums,singles` and returns
  albums = full-albums followed by singles (de-duplicated); `limit[top-songs]=20`.
* `catalog_playlist` sets `track_count` to the number of tracks loaded.
* `recently_played(limit=10, offset=0)`, new `recently_played_tracks(limit=10,
  offset=0) -> list[Track]`; `recommendations()` items carry `"id"` as well as
  `"title"`/`"items"` and groups with no album/playlist content are dropped
  (stations-only groups); `charts()` adds `"titles": {kind: chart name}`.
* `lyrics()` returns None for NotFound / no TTML / zero lines and raises
  `web.WebError` for unparsable TTML. It prefers `syllable-lyrics.ttmlLocalizations`
  (a string; a per-language dict is also accepted) and falls back to `lyrics.ttml`.
* `session`: new `SessionError`, `desktop_available()`, `ConnectProcess.wait()`,
  `ConnectProcess.lines`/`errors` (helper output, no secrets),
  `connect(timeout_s, *, on_output=None)`; `status()` adds `captured_at`,
  `http_status`, `has_token`. `check()` returns `status()` (state
  "no_session" without a token, "reconnect_required" on 401/403 — the
  workspace module writes the status file itself) and raises `SessionError`
  for network/other HTTP failures (message = host + status only).
* `cli_library` adds a `recently-added` command and accepts `pl.…` in `tracks`.

## Lyrics parsing semantics (amui.lyrics)

* `Line.text` is whitespace-normalised, excludes background spans. `Word.text`
  includes the untimed text up to the next word (normally one trailing space,
  nothing for syllable spans, punctuation tails), so
  `"".join(w.text for w in line.words) == line.text` (tested) — the UI can lay
  words out by concatenation without guessing spacing.
* `<span ttm:role="x-bg">` → its own `Line(background=True)` right after the
  main line, agent inherited, begin/end from its own attributes or its words.
  A `<p ttm:role="x-bg">` is a background line itself.
* `Lyrics.timing` = `itunes:timing` when present, downgraded if the document
  lacks that granularity; inferred otherwise. `language` = root `xml:lang`.
  `<p>` times are used as absolute (matching Apple's player). Empty paragraphs
  are dropped. Documents with a DTD/entities are rejected (ValueError).

## Live verification (read-only GETs, counts only)

* `status`/`check`: connected; `check` refreshed the status file (23:18 UTC).
* Library totals come from `meta.total` (several hundred albums, songs and artists in the test library).
  In a 100-song page 99 songs had `playParams.catalogId` + artwork, 1 did not.
* `tracks l.…`: 1 track; `tracks p.…`: total 1084, `--offset 1080 --limit 5`
  → 4 items and no next; `tracks <album G>`: 14 tracks, all `atmos`.
* `search "daft punk" --limit 2`: library 1 song + 1 artist; catalog 2 songs,
  2 albums, 1 artist, 1 playlist, top result = artist.
* `song <song F>`: `audioTraits = atmos, lossless, lossy-stereo, spatial`,
  with its album and artist ids (album G, artist H).
* `lyrics <song F>`: source syllable-lyrics, timing line, 73 lines (73
  timed), 0 words. `lyrics <song J>`: timing word, 48 lines = 44 main + 4
  background, 298 timed words, agent v1 (matches the workspace report: 44
  paragraphs, 298 timed spans; the 4 bg wrappers are untimed spans).
* `recent`: 10 items (user `pl.u-…` playlists + albums); recently played
  tracks: 10. `home`: 15 raw recommendation groups → 9 with album/playlist
  content. `charts`: 20 songs / 20 albums / 20 playlists (titles localised).
* `connect` without DISPLAY/WAYLAND_DISPLAY: refused with a clear
  `SessionError` (CLI exit 1). `--login` was not run.
* `python3 -m unittest discover -s tests -p "test_lyrics.py"` (19 tests) and
  `-p "test_api_parsing.py"` (44 tests) pass; `import amui.api, amui.lyrics,
  amui.session` is clean (no network at import).

## Open issues / observations

* `recently-added` returned only `library-albums` in the first 75 items of
  this library; playlists are mapped when Apple mixes them in (untested live).
* Library artists carry no artwork; the UI needs a catalog lookup for images.
* Library song/album lists never say whether Atmos exists; the badge needs
  `catalog_song()`/`catalog_album()` or `include_catalog=True`.
* The public developer token is re-fetched per process by `amui.web`
  (~0.3 s); a disk cache there would make CLI calls snappier.
* Titles from Apple (charts, artist views, recommendations) come in the
  storefront's language; no `l=` override is sent.
* The `(amui.cli_play unavailable …)` line printed by `python3 -m amui` comes
  from `amui/__main__.py` until the playback module exists.

## Library editing (2026-11-08)

New in `amui/api.py` (tests: `tests/test_api_mutations.py`, mocked
`web._request`/`web.get_json`). Endpoints on Apple's documented Apple Music API
(developer.apple.com/documentation/applemusicapi) plus, since 2026-11-08, the
three undocumented requests music.apple.com itself makes for playlist editing
(read from its public bundle `index~*.js`: `requestUpdatePlaylist`,
`requestDeleteFromLibrary`, `requestRemoveFromPlaylist`; verified live on a
throwaway playlist). The documented contracts were read from the documentation
data behind these pages:
`add-a-resource-to-a-library`, `create-a-new-library-playlist`,
`libraryplaylistcreationrequest` (+ `attributes`/`relationships` dictionaries),
`add-tracks-to-a-library-playlist`, `libraryplaylisttracksrequest`,
`get-a-library-playlist`, `ratings-api` (group), `add-a-personal-content-rating-33dop`
(Add a Personal Song Rating), `delete-a-personal-content-rating-3a3a2`,
`get-a-personal-content-rating-4k9c0`, `get-multiple-personal-content-ratings-6wab5`,
`ratingrequest`, `ratings`, `add-resource-to-favorites`, `playlists-api` (group
listing every documented library-playlist endpoint).

Transport: `MusicApi._mutate(method, path, params, body)` calls
`web._request` (credentials still limited to the API host by `amui.web`),
JSON body with `Content-Type: application/json`, 30 s timeout; an empty 202/204
answer returns None. Every mutation drops the in-memory cache entries whose URL
starts with the affected prefix (`/v1/me/library` for library/playlist changes,
`/v1/me/ratings` for ratings) — also when the request fails, since Apple may
have applied it. Errors propagate unchanged (`web.AuthError`, `web.NotFound`,
`web.WebError`); `ValueError` for bad arguments (blank name, unknown kind or
storefront code), `web.WebError("Invalid Apple Music identifier")` for ids
with characters outside `[A-Za-z0-9._-]`.

| Method | Request | Answer / mapping |
| --- | --- | --- |
| `favorite(track)` | `PUT /v1/me/ratings/songs/{catalog id}` body `{"type":"ratings","attributes":{"value":1}}`; library-only songs (no catalog id) use `/v1/me/ratings/library-songs/{library id}` | 200 `{data:[ratings]}` |
| `unfavorite(track)` | `DELETE` on the same path | 204; a 404 (nothing rated) counts as done |
| `is_favorite(catalog_ids)` | `GET /v1/me/ratings/songs?ids=a,b,…` in chunks of `RATINGS_LIMIT` (100), cached like other reads | `{id: value == 1}`; ids without a rating → False; a 404 chunk → all False |
| `add_to_library(catalog_id, kind="songs")` | `POST /v1/me/library?ids[<kind>]=<id>` (`LIBRARY_KINDS`: songs, albums, playlists, music-videos) | 202, empty body; Apple applies it asynchronously and silently ignores ids it cannot add |
| `create_playlist(name, description="", tracks=())` | `POST /v1/me/library/playlists` body `{"attributes":{"name","description"?},"relationships":{"tracks":{"data":[{id,type}]}}?}` | 201 `{data:[library-playlists]}` → `parse_playlist` (library_id `p.…`, `can_edit`) |
| `add_to_playlist(library_id, tracks)` | `POST /v1/me/library/playlists/{id}/tracks` body `{"data":[{id,type}]}` | 204; nothing is sent for an empty list |
| `rename_playlist(library_id, name, description=None)` | `PATCH /v1/me/library/playlists/{id}` body `{"attributes":{"name"?,"description"?}}` (undocumented; what the web app sends) | 204; nothing sent when neither changes |
| `delete_playlist(library_id)` | `DELETE /v1/me/library/playlists/{id}` (undocumented) | 204; Apple applies it a few seconds later (the listing still shows the playlist for ~2 s; the UI hides it for `DELETED_GRACE_S`); Apple's own playlists (`canDelete` false) → 404 |
| `remove_from_playlist(library_id, tracks)` | `DELETE /v1/me/library/playlists/{id}/tracks?ids[library-songs]={i.…}&mode=all`, one request per song (undocumented) | 204; needs the library song ids the playlist listing carries (`Track.library_id`), else `web.WebError` |
| `invalidate(*prefixes)` | — | drops cached answers under the paths (default `/v1/me/library`) |

`library_playlists()` requests `extend[library-playlists]=tags`: Apple's Favorite Songs playlist
carries `tags: ["favorited"]` (→ `Playlist.is_favorites`; it is also the only one with `canDelete`
false → `Playlist.can_delete`). `library_artists()` requests `include=catalog` like the web app: the
matching catalog artist supplies `Artist.artwork` (nearly every artist of the test library), `catalog_id` and
genres; responses grow from 13 KB to 260 KB per 100 artists (~1 s instead of ~0.3 s per page).
`api.NotSupported` remains defined but nothing raises it any more.

Track references: a `Track` with a catalog id is sent as `{"id": catalog_id,
"type": "songs"}`, otherwise `{"id": library_id, "type": "library-songs"}`
(the documented allowed types are `songs`, `library-songs`, `music-videos`,
`library-music-videos`); a track with neither id raises `web.WebError` before
any request. Plain id strings are accepted too (catalog songs).

Why favorites are ratings: Apple documents `POST /v1/me/favorites?ids=…`
("Add resource to favorites", 202) but no endpoint to remove or list
favorites, so a favorite/unfavorite/is_favorite cycle is only possible with the
personal song ratings (value 1 = like/favorite, -1 = dislike; the only two
values Apple supports). Live, read-only: 49 of the 99 catalog-matched songs on
the first library page carry rating 1 and no other value occurs, i.e. the
songs the user favorited in Apple's apps are visible as rating 1. The web
player uses the same `ratings` resource for its favorite button.

Not supported (documented-API gap, raise `NotSupported`): the "Playlists"
group of the API lists only *Create a New Library Playlist*, *Add Tracks to a
Library Playlist*, *Add a Resource to a Library* and the playlist-folder
endpoints under "Creating and Modifying User Playlists" — nothing to rename or
edit a playlist's attributes, delete a playlist, or remove tracks (Apple's own
`library-playlists` attributes do say `canEdit`/`canDelete: true`, but the
endpoints to do so are private web-player calls, which this app does not
guess). Likewise nothing removes a song from the library. The UI should hide or
grey out these actions and point at the Apple Music app / music.apple.com.

Documentation quirks: the create request's `relationships` is described as
optional on the endpoint page but its sub-dictionary marks `tracks` and
`parent` as required (the optional reading is right: a body without
`relationships` works live); the Add Song Rating example body uses
`"type":"rating"` while the `RatingRequest` schema allows only `"ratings"` —
the schema value is sent (not exercised live, see below); the `ids` query
parameter of `POST /v1/me/library` lists no allowed kinds (example `ids[albums]`),
hence `LIBRARY_KINDS` is the set of library resource kinds.

Catalog storefront override: `set_catalog_storefront(code | None)` validates
the code against `api.STOREFRONT_CODES` (ISO 3166-1 alpha-2, 249 codes, plus
`xk`, the user-assigned code Apple uses for its Kosovo storefront; live
`GET /v1/storefronts` returns 172 ids, all in that set, `xk` included), lower-cases
it and persists `cache_dir/storefront-override.json` (0600; `None`/`""` removes
the file). `catalog_storefront()` returns the override or the account's
storefront and is now what `_catalog()`, `search_catalog()`, `charts()` (and
through `_catalog()` `lyrics()`) use — the only change to existing code, three
call sites, no signature changed; `storefront()` still returns the account's.
`catalog_storefront_override()` tells the UI whether an override is active.
Cache keys contain the storefront, so switching needs no invalidation.

Live verification (one playlist, minimal; names/ids reported to the user only):
`create_playlist` → 201 with the resource (name and `description.standard`
echoed, `canEdit`/`canDelete` true, `playParams` without `globalId`, so
`Playlist.catalog_id` None); `add_to_playlist` with two catalog songs → 204;
`library_playlist_tracks` showed both tracks on the first read (library ids
`i.…` with `playParams.catalogId`); `GET /v1/me/library/playlists/{id}` showed
the new `lastModifiedDate`. `library_playlists()`/`recently-added` did **not**
list the new playlist ~1 s after creation but did a few minutes later (first
in default order and with `sort=-dateAdded`) — after `create_playlist` the UI
should insert the returned `Playlist` itself instead of relying on an
immediate refetch. Adding the tracks to the playlist did not add the song that
was not yet in the library (`include=library` on the catalog song still
empty). `rename_playlist`/`delete_playlist` raised `NotSupported`, so the test
playlist (named `am-ui test <UTC timestamp>`) stays in the library for the user
to delete. `is_favorite` with 99 ids in one request → 200 (hence
`RATINGS_LIMIT = 100`). The rating round trip (`favorite`/`unfavorite`) was
**not** exercised live: the designated song already had rating 1, and the rule
was to rate it only when unrated; the request shapes are unit-tested against
the documentation. If Apple ever answers 400 to the PUT, try the example's
`"type":"rating"`. The `jp` override on a temporary cache dir fetched the
catalog song through `/v1/catalog/jp/…` and reset cleanly.
