//! Tests for `amui::api`: JSON -> types mapping, pagination, caching, lyrics selection.
//!
//! Everything is synthetic: the client gets its own `Http` (no network), no real library data,
//! and the storefront cache lives in a scratch folder.
//!
//! This is tests/test_api_parsing.py, test for test and under the same names: each Python test
//! class is a module here (`LibraryPagesTest` -> `library_pages_test`). Its six `SessionTest`
//! tests are not here: they exercise `amui.session` (status files, the desktop guard, the sign-in
//! helper's results), not the API client, and belong to the port of that module. `reference`
//! compares the mapping with what the Python functions returned for a wider set of resources.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use amui::api::{self, ApiError, Item, MusicApi};
use amui::config::{self, Config};
use amui::types::{Album, Artist, Artwork, Page, Playlist, Timing, Track};
use amui::util::Event;
use amui::web::{Call, Http, Progress, Source, WebError};
use serde_json::{Map, Value, json};

// --- synthetic resources -------------------------------------------------------

/// `{**base, **extra}`
fn with(mut base: Value, extra: Value) -> Value {
    let (Some(target), Value::Object(extra)) = (base.as_object_mut(), extra) else { panic!("two objects") };
    target.extend(extra);
    base
}

fn art() -> Value {
    json!({"url": "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/{w}x{h}bb.jpg", "width": 1200, "height": 1200,
           "hasP3": false})
}

fn art_catalog() -> Value {
    with(art(), json!({"width": 1500, "height": 1500, "bgColor": "feecda", "textColor1": "1e1a1f"}))
}

fn art_playlist() -> Value {
    json!({"url": "https://is1-ssl.mzstatic.com/image/thumb/Features/x/y/z.png/{w}x{h}cc.jpg", "width": 1200, "height": 1200})
}

fn lib_album() -> Value {
    json!({"id": "l.AbCdEf1", "type": "library-albums", "href": "/v1/me/library/albums/l.AbCdEf1", "attributes": {
        "artistName": "Synth Artist", "artwork": art(), "dateAdded": "2026-01-02T03:04:05Z", "genreNames": ["Electronic"],
        "name": "Synthetic Album", "playParams": {"id": "l.AbCdEf1", "isLibrary": true, "kind": "album"},
        "releaseDate": "2020-05-06", "trackCount": 12}})
}

fn cat_album_stub() -> Value {
    json!({"id": "1000000001", "type": "albums", "attributes": {
        "artistName": "Synth Artist", "artwork": art_catalog(), "audioTraits": ["atmos", "lossless"], "genreNames": ["Electronic"],
        "isCompilation": false, "isSingle": false, "name": "Synthetic Album", "playParams": {"id": "1000000001", "kind": "album"},
        "releaseDate": "2020-05-06", "trackCount": 3, "url": "https://music.apple.com/us/album/synthetic-album/1000000001"}})
}

fn lib_album_with_catalog() -> Value {
    with(lib_album(), json!({"relationships": {"catalog": {"data": [cat_album_stub()]}}}))
}

fn lib_playlist() -> Value {
    json!({"id": "p.AbCdEf2", "type": "library-playlists", "attributes": {
        "artwork": art_playlist(), "canDelete": true, "canEdit": true, "dateAdded": "2026-02-03T00:00:00Z", "hasCatalog": false,
        "hasCollaboration": false, "isPublic": false, "lastModifiedDate": "2026-03-04T00:00:00Z", "name": "Synthetic Playlist",
        "description": {"standard": "A test list"},
        "playParams": {"globalId": "pl.u-AbCdEf3", "id": "p.AbCdEf2", "isLibrary": true, "kind": "playlist"}}})
}

fn lib_song() -> Value {
    json!({"id": "i.AbCdEf4", "type": "library-songs", "attributes": {
        "albumName": "Synthetic Album", "artistName": "Synth Artist", "artwork": art(), "contentRating": "clean", "discNumber": 1,
        "durationInMillis": 201000, "genreNames": ["Electronic"], "hasLyrics": true, "name": "Synthetic Song",
        "playParams": {"catalogId": "1000000002", "id": "i.AbCdEf4", "isLibrary": true, "kind": "song", "reporting": true,
                       "reportingId": "1000000002"}, "releaseDate": "2020-05-06", "trackNumber": 3}})
}

fn lib_song_with_album() -> Value {
    with(lib_song(), json!({"relationships": {"albums": {"data": [{"id": "l.AbCdEf1", "type": "library-albums"}]}}}))
}

fn lib_song_unmatched() -> Value {
    json!({"id": "i.AbCdEf5", "type": "library-songs", "attributes": {
        "albumName": "Home Recording", "artistName": "Me", "discNumber": 1, "durationInMillis": 1000, "genreNames": ["Demo"],
        "hasLyrics": false, "name": "Upload", "trackNumber": 1}})
}

fn lib_artist() -> Value {
    json!({"id": "r.AbCdEf6", "type": "library-artists", "attributes": {"name": "Synth Artist"}})
}

fn lib_artist_with_catalog() -> Value {
    let catalog = json!({"id": "1000000005", "type": "artists", "attributes": {
        "name": "Synth Artist", "genreNames": ["Electronic"], "artwork": art_catalog(),
        "url": "https://music.apple.com/us/artist/synth-artist/1000000005"}});
    let relationships = json!({"catalog": {"href": "/v1/me/library/artists/r.AbCdEf6/catalog", "data": [catalog]}});
    with(lib_artist(), json!({"relationships": relationships}))
}

fn lib_favorites() -> Value {
    json!({"id": "p.Favs1", "type": "library-playlists", "attributes": {
        "artwork": art_playlist(), "canDelete": false, "canEdit": false, "dateAdded": "2026-06-26T18:21:19Z", "hasCatalog": true,
        "hasCollaboration": false, "isPublic": false, "name": "Favorite Songs", "tags": ["favorited"],
        "playParams": {"globalId": "pl.u-Favs2", "id": "p.Favs1", "isLibrary": true, "kind": "playlist"}}})
}

fn cat_song_embedded() -> Value {
    json!({"id": "1000000002", "type": "songs", "attributes": {
        "albumArtistName": "Synth Artist", "albumName": "Synthetic Album", "artistName": "Synth Artist", "artwork": art_catalog(),
        "audioLocale": "en-US", "audioTraits": ["atmos", "lossless", "lossy-stereo", "spatial"], "composerName": "X",
        "discNumber": 1, "durationInMillis": 201000, "genreNames": ["Electronic", "Music"], "hasLyrics": true,
        "hasTimeSyncedLyrics": true, "isrc": "USX000000001", "name": "Synthetic Song",
        "playParams": {"id": "1000000002", "kind": "song"}, "previews": [{"url": "https://audio-ssl.itunes.apple.com/x.m4a"}],
        "releaseDate": "2020-05-06", "trackNumber": 3,
        "url": "https://music.apple.com/us/album/synthetic-album/1000000001?i=1000000002"}})
}

fn cat_song() -> Value {
    let relationships = json!({"albums": {"data": [{"id": "1000000001", "type": "albums"}]},
                               "artists": {"data": [{"id": "2000000001", "type": "artists"}]}});
    with(cat_song_embedded(), json!({"relationships": relationships}))
}

/// Python's default for `traits`.
const PLAIN_TRAITS: [&str; 2] = ["lossless", "lossy-stereo"];

fn embedded_song(song_id: &str, name: &str, traits: &[&str]) -> Value {
    let song = cat_song_embedded();
    let changed = json!({"name": name, "audioTraits": traits, "playParams": {"id": song_id, "kind": "song"}});
    let attributes = with(song["attributes"].clone(), changed);
    with(song, json!({"id": song_id, "attributes": attributes}))
}

fn cat_album() -> Value {
    let relationships = json!({
        "artists": {"data": [{"id": "2000000001", "type": "artists"}]},
        "tracks": {"data": [cat_song_embedded(), embedded_song("1000000003", "Second", &PLAIN_TRAITS)],
                   "next": "/v1/catalog/us/albums/1000000001/tracks?offset=2"}});
    with(cat_album_stub(), json!({"relationships": relationships}))
}

fn cat_single() -> Value {
    let stub = cat_album_stub();
    let changed = json!({"name": "Single - EP", "isSingle": true, "trackCount": 1});
    let attributes = with(stub["attributes"].clone(), changed);
    with(stub, json!({"id": "1000000009", "attributes": attributes}))
}

fn cat_artist() -> Value {
    json!({"id": "2000000001", "type": "artists", "attributes": {
        "artwork": art_catalog(), "genreNames": ["Electronic"], "name": "Synth Artist",
        "url": "https://music.apple.com/us/artist/synth-artist/2000000001"},
        "relationships": {"albums": {"data": [cat_album_stub()], "next": "/v1/catalog/us/artists/2000000001/albums?offset=25"}},
        "views": {"top-songs": {"attributes": {"title": "Top Songs"},
                                "data": [cat_song_embedded(), embedded_song("1000000003", "Second", &PLAIN_TRAITS)]},
                  "full-albums": {"attributes": {"title": "Albums"}, "data": [cat_album_stub()]},
                  "singles": {"attributes": {"title": "Singles & EPs"}, "data": [cat_album_stub(), cat_single()]}}})
}

fn cat_playlist_stub() -> Value {
    json!({"id": "pl.abc123", "type": "playlists", "attributes": {
        "artwork": art_catalog(), "audioTraits": [], "curatorName": "Apple Music Electronic",
        "description": {"short": "Short", "standard": "Standard text"}, "lastModifiedDate": "2026-01-01T00:00:00Z",
        "name": "Synthetic Essentials", "playParams": {"id": "pl.abc123", "kind": "playlist", "versionHash": "x"},
        "playlistType": "editorial", "url": "https://music.apple.com/us/playlist/synthetic-essentials/pl.abc123"}})
}

fn cat_playlist() -> Value {
    let relationships = json!({
        "curator": {"data": [{"id": "3000000001", "type": "apple-curators"}]},
        "tracks": {"data": [cat_song_embedded(), embedded_song("1000000003", "Second", &PLAIN_TRAITS)]}});
    with(cat_playlist_stub(), json!({"relationships": relationships}))
}

fn station() -> Value {
    json!({"id": "ra.123", "type": "stations", "attributes": {"name": "Synthetic Radio"}})
}

fn recommendations() -> Value {
    json!({"data": [
        {"id": "6-rec1", "type": "personal-recommendation", "attributes": {"title": {"stringForDisplay": "Made for You"}},
         "relationships": {"contents": {"data": [cat_album_stub(), cat_playlist_stub(), station()]}}},
        {"id": "6-rec2", "type": "personal-recommendation", "attributes": {"title": {"stringForDisplay": "Stations"}},
         "relationships": {"contents": {"data": [station()]}}},
    ], "meta": {"metrics": {}}})
}

fn charts() -> Value {
    json!({"results": {
        "albums": [{"chart": "most-played", "name": "Top Albums", "data": [cat_album_stub()], "next": "/v1/catalog/us/charts?offset=20"}],
        "songs": [{"chart": "most-played", "name": "Top Songs", "data": [cat_song_embedded()]}],
        "playlists": [{"chart": "most-played", "name": "Top Playlists", "data": [cat_playlist_stub()]}]}})
}

const NS: &str = r##"xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:itunes="http://music.apple.com/lyric-ttml-internal""##;

fn line_ttml() -> String {
    format!(
        r##"<tt {NS} xml:lang="en" itunes:timing="Line"><body><div><p begin="1.0" end="2.0">Line one</p><p begin="2.0" end="3.0">Line two</p></div></body></tt>"##
    )
}

fn word_ttml() -> String {
    format!(
        r##"<tt {NS} xml:lang="en" itunes:timing="Word"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.5">Word</span> <span begin="1.5" end="2.0">one</span></p></div></body></tt>"##
    )
}

/// Python: `lyrics_response(*, syllable=True, line=True, localizations=None)`.
fn lyrics_response(syllable: bool, line: bool, localizations: Option<Value>) -> Value {
    let mut relationships = cat_song()["relationships"].clone();
    if line {
        let entry = json!({"id": "1000000002", "type": "lyrics", "attributes": {"ttml": line_ttml()}});
        relationships["lyrics"] = json!({"data": [entry]});
    }
    if syllable {
        let value = localizations.unwrap_or_else(|| json!(word_ttml()));
        let entry = json!({"id": "1000000002", "type": "syllable-lyrics", "attributes": {"ttmlLocalizations": value}});
        relationships["syllable-lyrics"] = json!({"data": [entry]});
    }
    json!({"data": [with(cat_song(), json!({"relationships": relationships}))]})
}

// --- fake transport ---------------------------------------------------------------

type Query = HashMap<String, String>;
type Handler = Arc<dyn Fn(&Query) -> Result<Value, WebError> + Send + Sync>;

fn query(pairs: &[(&str, &str)]) -> Query {
    pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

/// Routes GET URLs (path + query) to canned bodies; records every call.
struct FakeWeb {
    calls: Mutex<Vec<(String, Query)>>,
    routes: Mutex<HashMap<String, Handler>>,
}

impl FakeWeb {
    fn new() -> FakeWeb {
        let fake = FakeWeb { calls: Mutex::default(), routes: Mutex::default() };
        fake.route(
            "/v1/me/storefront",
            json!({"data": [{"id": "us", "type": "storefronts", "attributes": {"name": "United States"}}]}),
        );
        fake
    }

    fn route(&self, path: &str, body: Value) {
        self.route_with(path, move |_| Ok(body.clone()));
    }

    fn route_with(&self, path: &str, handler: impl Fn(&Query) -> Result<Value, WebError> + Send + Sync + 'static) {
        self.routes.lock().unwrap().insert(path.to_string(), Arc::new(handler));
    }

    fn paths(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(path, _)| path.clone()).collect()
    }

    fn count(&self, path: &str) -> usize {
        self.paths().iter().filter(|called| *called == path).count()
    }

    fn calls(&self) -> Vec<(String, Query)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Http for FakeWeb {
    fn get_json_with(
        &self,
        url: &str,
        params: &[(&str, &str)],
        auth: bool,
        timeout: f64,
        source: Option<Source>,
    ) -> Result<Map<String, Value>, WebError> {
        assert!(params.is_empty(), "MusicApi builds the full URL itself");
        assert!(auth && timeout == 20.0 && source.is_none(), "a plain read with the account's credentials");
        let parts = url::Url::parse(url).unwrap();
        assert!(parts.scheme() == "https" && parts.host_str() == Some(config::API_HOST), "{url}");
        // as `urllib.parse.parse_qs` reads it: the first value of each name, blank values dropped
        let mut query = Query::new();
        for (name, value) in parts.query_pairs().filter(|(_, value)| !value.is_empty()) {
            query.entry(name.into_owned()).or_insert(value.into_owned());
        }
        self.calls.lock().unwrap().push((parts.path().to_string(), query.clone()));
        let handler = self.routes.lock().unwrap().get(parts.path()).cloned();
        let Some(handler) = handler else {
            return Err(WebError::NotFound(format!("{} returned HTTP 404", config::API_HOST)));
        };
        match handler(&query)? {
            Value::Object(body) => Ok(body), // a fresh copy each time, like a real response
            other => panic!("a route answers with an object, not {other}"),
        }
    }

    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
        panic!("the reading half of MusicApi asks for JSON only ({} {})", call.method, call.url);
    }

    fn download(
        &self,
        _: &str,
        _: &Path,
        _: u64,
        _: Option<Progress>,
        _: Option<&Event>,
        _: f64,
    ) -> Result<(u64, String), WebError> {
        panic!("MusicApi downloads nothing");
    }

    fn token_sources(&self) -> Vec<Source> {
        Vec::new()
    }
}

/// Python's `ApiTestCase`: a scratch folder, the fake, a client on both.
struct ApiTestCase {
    _dir: tempfile::TempDir,
    config: Arc<Config>,
    tmp: PathBuf,
    fake: Arc<FakeWeb>,
    /// What the cache tests move; the other tests leave the client its own clock.
    now: Option<Arc<Mutex<f64>>>,
    api: MusicApi,
}

impl ApiTestCase {
    fn new() -> ApiTestCase {
        ApiTestCase::build(None)
    }

    fn build(now: Option<Arc<Mutex<f64>>>) -> ApiTestCase {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new()));
        config.ensure_dirs().unwrap();
        let tmp = config.tmp_dir.join("api-test");
        amui::util::create_private_dirs(&tmp).unwrap();
        let fake = Arc::new(FakeWeb::new());
        let api = client(&config, &fake, &tmp, now.as_ref());
        ApiTestCase { _dir: dir, config, tmp, fake, now, api }
    }

    /// Python: `MusicApi(self.tmp)`.
    fn client(&self) -> MusicApi {
        client(&self.config, &self.fake, &self.tmp, self.now.as_ref())
    }

    /// The query of the first request to `path`.
    fn query(&self, path: &str) -> Query {
        self.fake.calls().into_iter().find(|(called, _)| called == path).map(|(_, query)| query).unwrap()
    }
}

/// A client that keeps its storefront files in `tmp`, on the test's clock when it has one.
fn client(config: &Arc<Config>, fake: &Arc<FakeWeb>, tmp: &Path, now: Option<&Arc<Mutex<f64>>>) -> MusicApi {
    let api = MusicApi::new(config.clone(), fake.clone()).with_cache_dir(tmp);
    match now.cloned() {
        Some(now) => api.with_clock(move || *now.lock().unwrap()),
        None => api,
    }
}

fn opt(value: &Option<String>) -> Option<&str> {
    value.as_deref()
}

fn kinds(items: &[Item]) -> Vec<&'static str> {
    items.iter().map(Item::kind).collect()
}

fn web_error(result: Result<impl std::fmt::Debug, ApiError>) -> WebError {
    match result {
        Err(ApiError::Web(error)) => error,
        other => panic!("expected a WebError, got {other:?}"),
    }
}

mod storefront_test {
    use super::*;

    #[test]
    fn fetched_once_and_persisted() {
        use std::os::unix::fs::PermissionsExt;
        let t = ApiTestCase::new();
        assert_eq!(t.api.storefront().unwrap(), "us");
        assert_eq!(t.api.storefront().unwrap(), "us");
        assert_eq!(t.fake.count("/v1/me/storefront"), 1);
        let file = t.tmp.join("storefront.json");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["id"], "us");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // a second instance sharing the directory needs no request at all
        let other = t.client();
        assert_eq!(other.storefront().unwrap(), "us");
        assert_eq!(t.fake.count("/v1/me/storefront"), 1);
    }

    #[test]
    fn rejects_odd_ids() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/storefront", json!({"data": [{"id": "../x", "type": "storefronts"}]}));
        let error = web_error(t.api.storefront());
        assert_eq!(error.to_string(), "Apple returned an unexpected storefront id");
    }
}

mod library_pages_test {
    use super::*;

    #[test]
    fn albums_page_and_pagination() {
        let t = ApiTestCase::new();
        t.fake.route_with("/v1/me/library/albums", |query| {
            Ok(if query.contains_key("offset") {
                json!({"data": [lib_album()], "meta": {"total": 584}})
            } else {
                json!({"data": [lib_album()], "meta": {"total": 584}, "next": "/v1/me/library/albums?offset=100"})
            })
        });
        let page: Page<Album> = t.api.library_albums(0, 100, None, false).unwrap();
        assert_eq!((page.next_offset, page.total), (Some(100), Some(584)));
        assert_eq!(t.query("/v1/me/library/albums"), query(&[("limit", "100")]));
        let album: &Album = &page.items[0];
        assert_eq!(
            (opt(&album.library_id), opt(&album.catalog_id), album.key().as_str()),
            (Some("l.AbCdEf1"), None, "l.AbCdEf1")
        );
        assert_eq!(
            (album.name.as_str(), album.artist.as_str(), album.track_count),
            ("Synthetic Album", "Synth Artist", Some(12))
        );
        assert_eq!(
            (opt(&album.genre), opt(&album.release_date), opt(&album.date_added)),
            (Some("Electronic"), Some("2020-05-06"), Some("2026-01-02T03:04:05Z"))
        );
        assert!(album.audio_traits.is_empty());
        assert_eq!(
            album.artwork.as_ref().unwrap().url(600),
            "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/600x600bb.jpg"
        );
        let last = t.api.library_albums(100, 500, None, false).unwrap();
        assert_eq!(last.next_offset, None);
        // clamped to Apple's maximum
        assert_eq!(t.fake.calls().last().unwrap().1, query(&[("limit", "100"), ("offset", "100")]));
    }

    #[test]
    fn albums_sort_and_catalog_include() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/library/albums", json!({"data": [lib_album_with_catalog()]}));
        let page = t.api.library_albums(0, 100, Some("-dateAdded"), true).unwrap();
        assert_eq!(
            t.query("/v1/me/library/albums"),
            query(&[("limit", "100"), ("sort", "-dateAdded"), ("include", "catalog")])
        );
        let album = &page.items[0];
        assert_eq!((opt(&album.library_id), opt(&album.catalog_id)), (Some("l.AbCdEf1"), Some("1000000001")));
        assert_eq!(album.audio_traits, ["atmos", "lossless"]);
        assert_eq!(page.next_offset, None);
        assert_eq!(page.total, None);
    }

    #[test]
    fn songs() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/library/songs", json!({"data": [lib_song(), lib_song_unmatched()], "meta": {"total": 2}}));
        let page = t.api.library_songs(0, 50, false).unwrap();
        assert_eq!(t.query("/v1/me/library/songs"), query(&[("limit", "50")]));
        let [song, upload]: &[Track; 2] = page.items.as_slice().try_into().unwrap();
        assert_eq!(
            (opt(&song.library_id), opt(&song.catalog_id), song.key().as_str()),
            (Some("i.AbCdEf4"), Some("1000000002"), "1000000002")
        );
        assert!(song.playable());
        assert_eq!(
            (song.name.as_str(), song.artist.as_str(), song.album.as_str()),
            ("Synthetic Song", "Synth Artist", "Synthetic Album")
        );
        assert_eq!((song.duration_ms, song.track_number, song.disc_number), (Some(201000), Some(3), Some(1)));
        assert_eq!(
            (song.has_lyrics, opt(&song.content_rating), opt(&song.genre)),
            (Some(true), Some("clean"), Some("Electronic"))
        );
        assert!(song.audio_traits.is_empty());
        assert_eq!(song.atmos_known(), None); // library metadata does not say
        assert_eq!(song.artwork.as_ref().unwrap().width, Some(1200));
        assert_eq!(song.album_library_id, None);
        assert_eq!(
            (opt(&upload.library_id), opt(&upload.catalog_id), upload.playable()),
            (Some("i.AbCdEf5"), None, false)
        );
        assert_eq!(upload.artwork, None);
        assert_eq!(upload.key(), "i.AbCdEf5");
    }

    #[test]
    fn songs_include_albums() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/library/songs", json!({"data": [lib_song_with_album()]}));
        let page = t.api.library_songs(0, 100, true).unwrap();
        assert_eq!(t.query("/v1/me/library/songs")["include"], "albums");
        assert_eq!(opt(&page.items[0].album_library_id), Some("l.AbCdEf1"));
    }

    #[test]
    fn playlists() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/library/playlists",
            json!({"data": [lib_playlist(), lib_favorites()], "meta": {"total": 2}}),
        );
        let page = t.api.library_playlists(0, 100).unwrap();
        let [playlist, favorites]: &[Playlist; 2] = page.items.as_slice().try_into().unwrap();
        assert_eq!(t.query("/v1/me/library/playlists")["extend[library-playlists]"], "tags");
        assert_eq!(
            (opt(&playlist.library_id), opt(&playlist.catalog_id), playlist.key().as_str()),
            (Some("p.AbCdEf2"), Some("pl.u-AbCdEf3"), "p.AbCdEf2")
        );
        assert_eq!(
            (playlist.name.as_str(), playlist.description.as_str(), playlist.can_edit, playlist.is_library),
            ("Synthetic Playlist", "A test list", true, true)
        );
        assert_eq!((playlist.can_delete, playlist.is_favorites), (true, false));
        assert_eq!(
            (favorites.name.as_str(), favorites.can_edit, favorites.can_delete, favorites.is_favorites),
            ("Favorite Songs", false, false, true)
        );
        assert_eq!(
            (opt(&playlist.date_added), opt(&playlist.last_modified)),
            (Some("2026-02-03T00:00:00Z"), Some("2026-03-04T00:00:00Z"))
        );
        assert_eq!(playlist.track_count, None);
        assert_eq!(
            playlist.artwork.as_ref().unwrap().url(300),
            "https://is1-ssl.mzstatic.com/image/thumb/Features/x/y/z.png/300x300cc.jpg"
        );
    }

    #[test]
    fn artists() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/library/artists",
            json!({"data": [lib_artist(), lib_artist_with_catalog()], "meta": {"total": 2}}),
        );
        let page = t.api.library_artists(0, 100, true).unwrap();
        let [bare, pictured]: &[Artist; 2] = page.items.as_slice().try_into().unwrap();
        assert_eq!(t.query("/v1/me/library/artists")["include"], "catalog");
        assert_eq!(
            (opt(&bare.library_id), opt(&bare.catalog_id), bare.name.as_str(), bare.key().as_str()),
            (Some("r.AbCdEf6"), None, "Synth Artist", "r.AbCdEf6")
        );
        assert_eq!(bare.artwork, None);
        // the included catalog artist supplies the picture, the genre and the catalog id; the key stays the library id
        assert_eq!(
            (opt(&pictured.library_id), opt(&pictured.catalog_id), pictured.key().as_str(), opt(&pictured.genre)),
            (Some("r.AbCdEf6"), Some("1000000005"), "r.AbCdEf6", Some("Electronic"))
        );
        assert_eq!(pictured.artwork.as_ref().unwrap().url_template, art_catalog()["url"].as_str().unwrap());
        t.fake.calls.lock().unwrap().clear();
        t.api.library_artists(0, 100, false).unwrap();
        assert!(!t.query("/v1/me/library/artists").contains_key("include"));
    }

    #[test]
    fn recently_added_is_mixed_and_capped() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/library/recently-added",
            json!({"data": [lib_album(), lib_playlist(), station()], "next": "/v1/me/library/recently-added?offset=25"}),
        );
        let page = t.api.library_recently_added(0, 100).unwrap();
        assert_eq!(t.query("/v1/me/library/recently-added"), query(&[("limit", "25")]));
        assert_eq!(kinds(&page.items), ["album", "playlist"]); // stations skipped
        assert_eq!((page.next_offset, page.total), (Some(25), None));
    }

    #[test]
    fn album_tracks_follow_next() {
        let t = ApiTestCase::new();
        t.fake.route_with("/v1/me/library/albums/l.AbCdEf1/tracks", |query| {
            Ok(if query.contains_key("offset") {
                json!({"data": [lib_song_unmatched()]})
            } else {
                json!({"data": [lib_song()], "next": "/v1/me/library/albums/l.AbCdEf1/tracks?offset=1"})
            })
        });
        let tracks = t.api.library_album_tracks("l.AbCdEf1").unwrap();
        let ids: Vec<Option<&str>> = tracks.iter().map(|track| opt(&track.library_id)).collect();
        assert_eq!(ids, [Some("i.AbCdEf4"), Some("i.AbCdEf5")]);
        assert_eq!(t.fake.count("/v1/me/library/albums/l.AbCdEf1/tracks"), 2);
    }

    #[test]
    fn playlist_tracks_page_and_empty_playlist() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/library/playlists/p.AbCdEf2/tracks",
            json!({"data": [lib_song()], "meta": {"total": 1084}, "next": "/v1/me/library/playlists/p.AbCdEf2/tracks?offset=100"}),
        );
        let page = t.api.library_playlist_tracks("p.AbCdEf2", 0, 100).unwrap();
        assert_eq!((page.items.len(), page.next_offset, page.total), (1, Some(100), Some(1084)));
        let empty = t.api.library_playlist_tracks("p.Empty", 0, 100).unwrap(); // Apple answers 404 for an empty playlist
        assert_eq!((empty.items, empty.next_offset, empty.total), (vec![], None, Some(0)));
    }

    #[test]
    fn artist_albums() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/library/artists/r.AbCdEf6/albums", json!({"data": [lib_album()], "meta": {"total": 1}}));
        let albums = t.api.library_artist_albums("r.AbCdEf6").unwrap();
        let ids: Vec<Option<&str>> = albums.iter().map(|album| opt(&album.library_id)).collect();
        assert_eq!(ids, [Some("l.AbCdEf1")]);
    }

    #[test]
    fn bad_identifier() {
        let t = ApiTestCase::new();
        let error = web_error(t.api.library_album_tracks("l.x/../../v1/me"));
        assert_eq!(error.to_string(), "Invalid Apple Music identifier");
        assert_eq!(t.fake.calls(), []);
    }
}

mod catalog_test {
    use super::*;

    #[test]
    fn song() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", json!({"data": [cat_song()]}));
        let track = t.api.catalog_song("1000000002").unwrap();
        assert_eq!(
            (opt(&track.catalog_id), opt(&track.library_id), track.key().as_str()),
            (Some("1000000002"), None, "1000000002")
        );
        assert_eq!(track.audio_traits, ["atmos", "lossless", "lossy-stereo", "spatial"]);
        assert_eq!(track.atmos_known(), Some(true));
        assert_eq!(
            (opt(&track.album_catalog_id), opt(&track.artist_catalog_id)),
            (Some("1000000001"), Some("2000000001"))
        );
        assert_eq!(
            (opt(&track.isrc), opt(&track.release_date), opt(&track.genre)),
            (Some("USX000000001"), Some("2020-05-06"), Some("Electronic"))
        );
        let artwork = track.artwork.as_ref().unwrap();
        assert_eq!(opt(&artwork.bg_color), Some("feecda"));
        assert_eq!(artwork.url(1000), "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/1000x1000bb.jpg");
    }

    #[test]
    fn embedded_song_album_id_from_url() {
        let track = api::parse_track(&cat_song_embedded(), None).unwrap();
        assert_eq!(opt(&track.album_catalog_id), Some("1000000001"));
        assert_eq!(track.artist_catalog_id, None);
        assert_eq!(api::parse_track(&embedded_song("1", "x", &PLAIN_TRAITS), None).unwrap().atmos_known(), Some(false));
    }

    #[test]
    fn album_with_tracks_follows_relationship_next() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/albums/1000000001", json!({"data": [cat_album()]}));
        t.fake.route(
            "/v1/catalog/us/albums/1000000001/tracks",
            json!({"data": [embedded_song("1000000004", "Third", &PLAIN_TRAITS)]}),
        );
        let (album, tracks) = t.api.catalog_album("1000000001").unwrap();
        assert_eq!(t.query("/v1/catalog/us/albums/1000000001"), query(&[("include", "tracks")]));
        assert_eq!(
            (opt(&album.catalog_id), opt(&album.library_id), opt(&album.artist_catalog_id)),
            (Some("1000000001"), None, Some("2000000001"))
        );
        assert_eq!((album.is_single, album.is_compilation, album.track_count), (Some(false), Some(false), Some(3)));
        assert_eq!(album.audio_traits, ["atmos", "lossless"]);
        let names: Vec<&str> = tracks.iter().map(|track| track.name.as_str()).collect();
        assert_eq!(names, ["Synthetic Song", "Second", "Third"]);
        assert!(tracks.iter().all(|track| opt(&track.album_catalog_id) == Some("1000000001")));
        let atmos: Vec<Option<bool>> = tracks.iter().map(Track::atmos_known).collect();
        assert_eq!(atmos, [Some(true), Some(false), Some(false)]);
    }

    #[test]
    fn artist_views() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/artists/2000000001", json!({"data": [cat_artist()]}));
        let (artist, top, albums) = t.api.catalog_artist("2000000001").unwrap();
        let query = t.query("/v1/catalog/us/artists/2000000001");
        assert_eq!(query["views"], "top-songs,full-albums,singles");
        assert_eq!(query["limit[top-songs]"], "20");
        assert_eq!(
            (opt(&artist.catalog_id), artist.name.as_str(), opt(&artist.genre)),
            (Some("2000000001"), "Synth Artist", Some("Electronic"))
        );
        let top: Vec<Option<&str>> = top.iter().map(|track| opt(&track.catalog_id)).collect();
        assert_eq!(top, [Some("1000000002"), Some("1000000003")]);
        let ids: Vec<Option<&str>> = albums.iter().map(|album| opt(&album.catalog_id)).collect();
        assert_eq!(ids, [Some("1000000001"), Some("1000000009")]); // de-duplicated, albums first
        let singles: Vec<Option<bool>> = albums.iter().map(|album| album.is_single).collect();
        assert_eq!(singles, [Some(false), Some(true)]);
    }

    #[test]
    fn playlist() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/playlists/pl.abc123", json!({"data": [cat_playlist()]}));
        let (playlist, tracks) = t.api.catalog_playlist("pl.abc123").unwrap();
        assert_eq!(
            (opt(&playlist.catalog_id), opt(&playlist.library_id), playlist.is_library, playlist.can_edit),
            (Some("pl.abc123"), None, false, false)
        );
        assert_eq!(
            (opt(&playlist.curator), playlist.description.as_str(), playlist.track_count),
            (Some("Apple Music Electronic"), "Standard text", Some(2))
        );
        assert_eq!(tracks.len(), 2);
    }

    #[test]
    fn not_found_propagates() {
        let t = ApiTestCase::new();
        assert!(matches!(web_error(t.api.catalog_song("999")), WebError::NotFound(_)));
    }
}

mod search_test {
    use super::*;

    #[test]
    fn catalog_search() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/catalog/us/search",
            json!({"results": {
                "songs": {"data": [cat_song_embedded()], "next": "/v1/catalog/us/search?offset=25&term=x"},
                "albums": {"data": [cat_album_stub()]}, "artists": {"data": [cat_artist()]}, "playlists": {"data": [cat_playlist_stub()]},
                "topResults": {"data": [cat_artist(), cat_song_embedded()]}}}),
        );
        let results = t.api.search_catalog("synth artist", 99).unwrap();
        assert_eq!(
            t.query("/v1/catalog/us/search"),
            query(&[
                ("term", "synth artist"),
                ("types", "songs,albums,artists,playlists"),
                ("limit", "25"),
                ("with", "topResults")
            ])
        );
        assert_eq!(
            [results.songs.len(), results.albums.len(), results.artists.len(), results.playlists.len()],
            [1, 1, 1, 1]
        );
        assert!(matches!(results.top, Some(Item::Artist(_))));
        assert_eq!(results.to_json()["top_kind"], "artist");
    }

    #[test]
    fn library_search_missing_types() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/library/search",
            json!({"results": {"library-songs": {"data": [lib_song()]}, "library-albums": {"data": [lib_album()]}},
                   "meta": {"results": {"order": ["library-albums", "library-songs"]}}}),
        );
        let results = t.api.search_library("synthetic", 10).unwrap();
        let query = t.query("/v1/me/library/search");
        assert_eq!(query["types"], "library-songs,library-albums,library-artists,library-playlists");
        assert_eq!(query["limit"], "10");
        assert_eq!((results.songs.len(), results.albums.len()), (1, 1));
        assert_eq!((results.artists, results.playlists, results.top), (vec![], vec![], None));
    }

    #[test]
    fn blank_term_makes_no_request() {
        let t = ApiTestCase::new();
        assert_eq!(t.api.search_catalog("   ", 10).unwrap().songs, []);
        assert_eq!(t.api.search_library("", 10).unwrap().albums, []);
        assert_eq!(t.fake.calls(), []);
    }
}

mod lyrics_test {
    use super::*;

    #[test]
    fn prefers_syllable_lyrics() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(true, true, None));
        let lyrics = t.api.lyrics("1000000002").unwrap().unwrap();
        assert_eq!(
            t.query("/v1/catalog/us/songs/1000000002"),
            query(&[("include", "lyrics,syllable-lyrics"), ("extend", "ttmlLocalizations")])
        );
        assert_eq!((lyrics.source.as_str(), lyrics.timing, lyrics.lines.len()), ("syllable-lyrics", Timing::Word, 1));
        let words: Vec<&str> = lyrics.lines[0].words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(words, ["Word ", "one"]);
    }

    #[test]
    fn falls_back_to_line_ttml() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(false, true, None));
        let lyrics = t.api.lyrics("1000000002").unwrap().unwrap();
        assert_eq!((lyrics.source.as_str(), lyrics.timing, lyrics.lines.len()), ("lyrics", Timing::Line, 2));
    }

    #[test]
    fn none_when_absent() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(false, false, None));
        assert_eq!(t.api.lyrics("1000000002").unwrap(), None);
        assert_eq!(t.api.lyrics("404404").unwrap(), None); // NotFound -> None
    }

    #[test]
    fn empty_strings_fall_through() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(true, true, Some(json!("  "))));
        assert_eq!(t.api.lyrics("1000000002").unwrap().unwrap().source, "lyrics");
    }

    #[test]
    fn localizations_map() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(true, true, Some(json!({"en": word_ttml()}))));
        assert_eq!(t.api.lyrics("1000000002").unwrap().unwrap().timing, Timing::Word);
    }

    #[test]
    fn lyrics_come_from_the_account_s_storefront_whatever_region_is_chosen() {
        let t = ApiTestCase::new();
        t.api.set_catalog_storefront(Some("jp")).unwrap();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(true, true, None));
        // the chosen region does not offer lyrics of this song to the account
        t.fake.route("/v1/catalog/jp/songs/1000000002", lyrics_response(false, false, None));
        let lyrics = t.api.lyrics("1000000002").unwrap().unwrap();
        assert_eq!((lyrics.source.as_str(), lyrics.timing), ("syllable-lyrics", Timing::Word));
        assert_eq!(t.fake.count("/v1/catalog/jp/songs/1000000002"), 0);
        // a song the account's storefront does not know: the chosen region is asked
        t.fake.route("/v1/catalog/jp/songs/1000000003", lyrics_response(false, true, None));
        assert_eq!(t.api.lyrics("1000000003").unwrap().unwrap().source, "lyrics");
        assert_eq!(t.fake.paths().iter().filter(|path| path.ends_with("/songs/1000000003")).count(), 2);
    }

    #[test]
    fn unparseable_ttml_is_a_web_error() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/songs/1000000002", lyrics_response(true, true, Some(json!("<tt>"))));
        let error = web_error(t.api.lyrics("1000000002"));
        assert_eq!(error.to_string(), "Apple returned lyrics that could not be parsed");
    }
}

mod home_test {
    use super::*;

    #[test]
    fn recently_played() {
        let t = ApiTestCase::new();
        t.fake.route(
            "/v1/me/recent/played",
            json!({"data": [cat_playlist_stub(), cat_album_stub(), station()], "next": "/v1/me/recent/played?offset=10"}),
        );
        let items = t.api.recently_played(50, 0).unwrap();
        assert_eq!(t.query("/v1/me/recent/played"), query(&[("types", "albums,playlists"), ("limit", "10")]));
        assert_eq!(kinds(&items), ["playlist", "album"]);
    }

    #[test]
    fn recently_played_tracks() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/recent/played/tracks", json!({"data": [cat_song_embedded()]}));
        let tracks = t.api.recently_played_tracks(10, 0).unwrap();
        assert_eq!(t.query("/v1/me/recent/played/tracks"), query(&[("types", "songs"), ("limit", "10")]));
        assert_eq!(opt(&tracks[0].catalog_id), Some("1000000002"));
    }

    #[test]
    fn recommendations() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/recommendations", super::recommendations());
        let groups = t.api.recommendations().unwrap();
        assert_eq!(groups.len(), 1); // the stations-only group is dropped
        assert_eq!((opt(&groups[0].id), groups[0].title.as_str()), (Some("6-rec1"), "Made for You"));
        assert_eq!(kinds(&groups[0].items), ["album", "playlist"]);
    }

    #[test]
    fn charts() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/catalog/us/charts", super::charts());
        let charts = t.api.charts().unwrap();
        assert_eq!(t.query("/v1/catalog/us/charts"), query(&[("types", "albums,songs,playlists"), ("limit", "20")]));
        let mut keys: Vec<String> = charts.to_json().as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["albums", "playlists", "songs", "titles"]);
        // the three lists hold an Album, a Track and a Playlist
        let (_, _, _): (&Album, &Track, &Playlist) = (&charts.albums[0], &charts.songs[0], &charts.playlists[0]);
        assert_eq!(
            json!(charts.titles),
            json!({"albums": "Top Albums", "songs": "Top Songs", "playlists": "Top Playlists"})
        );
    }
}

mod cache_test {
    use super::*;

    /// Python's `setUp`: the clock stands at 1000 and the library has one album.
    fn case() -> (ApiTestCase, Arc<Mutex<f64>>) {
        let now = Arc::new(Mutex::new(1000.0));
        let t = ApiTestCase::build(Some(now.clone()));
        t.fake.route("/v1/me/library/albums", json!({"data": [lib_album()]}));
        (t, now)
    }

    fn albums(api: &MusicApi) -> Page<Album> {
        api.library_albums(0, 100, None, false).unwrap()
    }

    #[test]
    fn ttl_and_clear() {
        let (t, now) = case();
        albums(&t.api);
        albums(&t.api);
        assert_eq!(t.fake.count("/v1/me/library/albums"), 1);
        *now.lock().unwrap() += 59.0;
        albums(&t.api);
        assert_eq!(t.fake.count("/v1/me/library/albums"), 1);
        *now.lock().unwrap() += 2.0; // past the 60 s default
        albums(&t.api);
        assert_eq!(t.fake.count("/v1/me/library/albums"), 2);
        t.api.clear_cache();
        albums(&t.api);
        assert_eq!(t.fake.count("/v1/me/library/albums"), 3);
        t.api.library_albums(100, 100, None, false).unwrap(); // a different URL is a different entry
        assert_eq!(t.fake.count("/v1/me/library/albums"), 4);
    }

    #[test]
    fn custom_ttl() {
        let (t, now) = case();
        let short = t.client().with_ttl(5.0);
        albums(&short);
        *now.lock().unwrap() += 6.0;
        albums(&short);
        assert_eq!(t.fake.count("/v1/me/library/albums"), 2);
    }

    #[test]
    fn errors_are_not_cached_and_auth_error_passes_through() {
        let (t, _now) = case();
        let rejection = "amp-api.music.apple.com rejected the session (HTTP 403); reconnect to Apple Music";
        let calls = AtomicUsize::new(0);
        t.fake.route_with("/v1/me/library/albums", move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(WebError::auth(rejection, Some(403)));
            }
            Ok(json!({"data": [lib_album()]}))
        });
        let error = t.api.library_albums(0, 100, None, false).unwrap_err();
        assert!(error.is_auth());
        assert_eq!(error, ApiError::Web(WebError::auth(rejection, Some(403)))); // as the HTTP layer made it
        assert_eq!(error.to_string(), rejection);
        assert_eq!(albums(&t.api).items.len(), 1);
    }

    #[test]
    fn concurrent_access() {
        let (t, _now) = case();
        let errors: Mutex<Vec<ApiError>> = Mutex::default();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let done = t.api.library_albums(0, 100, None, false).and_then(|_| t.api.storefront());
                    if let Err(error) = done {
                        errors.lock().unwrap().push(error);
                    }
                });
            }
        });
        assert_eq!(errors.into_inner().unwrap(), []);
        assert!(t.fake.count("/v1/me/library/albums") <= 8);
    }
}

mod artwork_test {
    use super::*;

    #[test]
    fn templates() {
        let plain = api::parse_artwork(
            &json!({"url": "https://x.mzstatic.com/a/{w}x{h}bb.jpg", "width": 1200, "height": 1200}),
        )
        .unwrap();
        assert_eq!(plain.url(600), "https://x.mzstatic.com/a/600x600bb.jpg");
        let full: Artwork =
            api::parse_artwork(&json!({"url": "https://x.mzstatic.com/a/{w}x{h}{c}.{f}", "bgColor": "112233"}))
                .unwrap();
        assert_eq!((full.width, full.height, opt(&full.bg_color)), (None, None, Some("112233")));
        assert_eq!(full.url_as(300, "png"), "https://x.mzstatic.com/a/300x300bb.png");
        assert_eq!(full.url(10), "https://x.mzstatic.com/a/32x32bb.jpg"); // bounded
        assert_eq!(api::parse_artwork(&Value::Null), None);
        assert_eq!(api::parse_artwork(&json!({"width": 1})), None);
    }

    #[test]
    fn parse_item_dispatch() {
        assert!(matches!(api::parse_item(&lib_song()), Some(Item::Track(_))));
        assert!(matches!(api::parse_item(&cat_album_stub()), Some(Item::Album(_))));
        assert!(matches!(api::parse_item(&lib_playlist()), Some(Item::Playlist(_))));
        assert!(matches!(api::parse_item(&lib_artist()), Some(Item::Artist(_))));
        assert_eq!(api::parse_item(&station()), None);
        assert_eq!(api::parse_item(&json!({"id": "1", "type": "songs"})), None); // relationship stub without attributes
    }
}

/// Beyond the Python tests: the port against the Python functions themselves.
mod reference {
    use super::*;

    /// fixtures/api_reference.json: synthetic resources (the ones above and many odd ones) with
    /// what amui/api.py's parse functions returned for each, as `to_dict()`.
    #[test]
    fn resources_map_as_in_python() {
        let reference: Value = serde_json::from_str(include_str!("fixtures/api_reference.json")).unwrap();
        let cases = reference["cases"].as_array().unwrap();
        assert!(cases.len() > 120);
        for case in cases {
            let (function, name, resource) =
                (case["function"].as_str().unwrap(), case["name"].as_str().unwrap(), &case["resource"]);
            let mapped = match function {
                "track" => {
                    let album = case.get("album").and_then(api::parse_album);
                    api::parse_track(resource, album.as_ref()).map(|track| track.to_json())
                }
                "album" => api::parse_album(resource).map(|album| album.to_json()),
                "playlist" => api::parse_playlist(resource).map(|playlist| playlist.to_json()),
                "artist" => api::parse_artist(resource).map(|artist| artist.to_json()),
                "artwork" => api::parse_artwork(resource).map(|artwork| artwork.to_json()),
                "item" => api::parse_item(resource).map(|item| json!({"kind": item.kind(), "value": item.to_json()})),
                other => panic!("unknown function {other}"),
            };
            assert_eq!(mapped.unwrap_or(Value::Null), case["expected"], "{function}: {name}");
        }
    }

    /// Python hands these two results out as dictionaries; here they are types that print as
    /// the same JSON objects.
    #[test]
    fn groups_and_charts_as_json() {
        let t = ApiTestCase::new();
        t.fake.route("/v1/me/recommendations", recommendations());
        t.fake.route("/v1/catalog/us/charts", charts());
        let group = t.api.recommendations().unwrap().remove(0).to_json();
        assert_eq!((&group["id"], &group["title"]), (&json!("6-rec1"), &json!("Made for You")));
        let keys: Vec<&Value> = group["items"].as_array().unwrap().iter().map(|item| &item["key"]).collect();
        assert_eq!(keys, [&json!("1000000001"), &json!("pl.abc123")]);
        let charts = t.api.charts().unwrap().to_json();
        assert_eq!(
            charts["titles"],
            json!({"albums": "Top Albums", "songs": "Top Songs", "playlists": "Top Playlists"})
        );
        let first = |kind: &str| charts[kind][0]["key"].clone();
        assert_eq!(
            [first("albums"), first("songs"), first("playlists")],
            [json!("1000000001"), json!("1000000002"), json!("pl.abc123")]
        );
        assert_eq!(charts["songs"][0]["playable"], true);
    }
}
