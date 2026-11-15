//! Tests for `amui::api` library editing: favorites (personal ratings), add to library, playlists
//! (create / add / rename / delete / remove, the last three being the requests music.apple.com
//! makes) and the catalog storefront override.
//!
//! Everything is synthetic: the client gets its own `Http` (no network), no real library data;
//! the storefront files live in a scratch folder.
//!
//! This is tests/test_api_mutations.py, test for test and under the same names: each Python test
//! class is a module here (`FavoritesTest` -> `favorites_test`). `beyond_python` adds what the
//! Python tests leave unsaid.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use amui::api::{self, ApiError, MusicApi, NotSupported, TrackRef};
use amui::config::{self, Config};
use amui::types::{Playlist, Track};
use amui::util::Event;
use amui::web::{Call, Http, Progress, Source, WebError};
use serde_json::{Map, Value, json};

// --- synthetic data ----------------------------------------------------------------

fn catalog() -> Track {
    Track { library_id: Some("i.AbCdEf4".into()), ..Track::new(Some("1000000002"), "Synthetic Song", "Synth Artist") }
}

fn second() -> Track {
    Track::new(Some("1000000003"), "Second", "Synth Artist")
}

fn library_only() -> Track {
    Track { library_id: Some("i.AbCdEf5".into()), ..Track::new(None, "Upload", "Me") }
}

fn no_ids() -> Track {
    Track::new(None, "Ghost", "")
}

fn created() -> Value {
    json!({"id": "p.NewList1", "type": "library-playlists", "href": "/v1/me/library/playlists/p.NewList1", "attributes": {
        "canEdit": true, "canDelete": true, "dateAdded": "2026-11-08T00:00:00Z", "hasCatalog": false, "isPublic": false,
        "name": "Synthetic List", "description": {"standard": "A test list"},
        "playParams": {"id": "p.NewList1", "isLibrary": true, "kind": "playlist"}}})
}

fn ratings() -> Value {
    json!({"data": [
        {"id": "1000000002", "type": "ratings", "href": "/v1/me/ratings/songs/1000000002", "attributes": {"value": 1}},
        {"id": "1000000003", "type": "ratings", "href": "/v1/me/ratings/songs/1000000003", "attributes": {"value": -1}}]})
}

fn lib_song() -> Value {
    json!({"id": "i.AbCdEf4", "type": "library-songs", "attributes": {
        "albumName": "Synthetic Album", "artistName": "Synth Artist", "durationInMillis": 201000, "name": "Synthetic Song",
        "playParams": {"catalogId": "1000000002", "id": "i.AbCdEf4", "isLibrary": true, "kind": "song"}, "trackNumber": 3}})
}

fn cat_song() -> Value {
    json!({"id": "1000000002", "type": "songs", "attributes": {
        "artistName": "Synth Artist", "albumName": "Synthetic Album", "name": "Synthetic Song", "durationInMillis": 201000,
        "playParams": {"id": "1000000002", "kind": "song"}}})
}

fn storefront() -> Value {
    json!({"data": [{"id": "us", "type": "storefronts", "attributes": {"name": "United States"}}]})
}

// --- fake transport ---------------------------------------------------------------

type Query = HashMap<String, String>;
/// (method, path, query, JSON body)
type Seen = (String, String, Query, Option<Value>);
type Handler = Arc<dyn Fn(&Query, Option<&Value>) -> Result<Vec<u8>, WebError> + Send + Sync>;

fn seen(method: &str, path: &str, query: &[(&str, &str)], body: Option<Value>) -> Seen {
    let query = query.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
    (method.to_string(), path.to_string(), query, body)
}

/// Stands in for the HTTP layer, for the edits (`request`) and the reads (`get_json`); records
/// every call.
struct FakeTransport {
    calls: Mutex<Vec<Seen>>,
    /// The URLs as they were asked for.
    urls: Mutex<Vec<String>>,
    routes: Mutex<HashMap<(String, String), Handler>>,
}

impl FakeTransport {
    fn new() -> FakeTransport {
        let fake = FakeTransport { calls: Mutex::default(), urls: Mutex::default(), routes: Mutex::default() };
        fake.route("GET", "/v1/me/storefront", storefront());
        fake
    }

    /// Answer with this JSON.
    fn route(&self, method: &str, path: &str, body: Value) {
        self.route_with(method, path, move |_, _| Ok(body.clone()));
    }

    /// Answer with these bytes (`b""`: Apple's empty 202/204).
    fn route_bytes(&self, method: &str, path: &str, body: &'static [u8]) {
        let handler: Handler = Arc::new(move |_, _| Ok(body.to_vec()));
        self.routes.lock().unwrap().insert((method.to_string(), path.to_string()), handler);
    }

    fn route_with(
        &self,
        method: &str,
        path: &str,
        handler: impl Fn(&Query, Option<&Value>) -> Result<Value, WebError> + Send + Sync + 'static,
    ) {
        let handler: Handler = Arc::new(move |query, payload| Ok(handler(query, payload)?.to_string().into_bytes()));
        self.routes.lock().unwrap().insert((method.to_string(), path.to_string()), handler);
    }

    fn answer(&self, method: &str, url: &str, payload: Option<Value>) -> Result<Vec<u8>, WebError> {
        let parts = url::Url::parse(url).unwrap();
        assert!(parts.scheme() == "https" && parts.host_str() == Some(config::API_HOST), "{url}");
        // as `urllib.parse.parse_qs` reads it: the first value of each name, blank values dropped
        let mut query = Query::new();
        for (name, value) in parts.query_pairs().filter(|(_, value)| !value.is_empty()) {
            query.entry(name.into_owned()).or_insert(value.into_owned());
        }
        self.urls.lock().unwrap().push(url.to_string());
        self.calls.lock().unwrap().push((method.to_string(), parts.path().to_string(), query.clone(), payload.clone()));
        let handler = self.routes.lock().unwrap().get(&(method.to_string(), parts.path().to_string())).cloned();
        match handler {
            Some(handler) => handler(&query, payload.as_ref()),
            None => Err(WebError::NotFound(format!("{} returned HTTP 404", config::API_HOST))),
        }
    }

    fn calls(&self) -> Vec<Seen> {
        self.calls.lock().unwrap().clone()
    }

    fn mutations(&self) -> Vec<Seen> {
        self.calls().into_iter().filter(|(method, ..)| method != "GET").collect()
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.calls().iter().filter(|(called, at, ..)| called == method && at == path).count()
    }
}

impl Http for FakeTransport {
    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
        assert!(call.auth, "mutations must carry the account credentials");
        assert!(["POST", "PUT", "PATCH", "DELETE"].contains(&call.method), "{}", call.method);
        assert!(call.timeout == 30.0 && call.source.is_none() && call.accept == "application/json");
        let payload = match call.body {
            None => {
                assert_eq!(call.content_type, None);
                None
            }
            Some(body) => {
                assert_eq!(call.content_type, Some("application/json"));
                Some(serde_json::from_slice(body).unwrap())
            }
        };
        self.answer(call.method, call.url, payload)
    }

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
        match serde_json::from_slice(&self.answer("GET", url, None)?) {
            Ok(Value::Object(body)) => Ok(body),
            _ => panic!("a read is answered with a JSON object"),
        }
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

/// Python's `MutationTestCase`: a scratch folder, the fake, a client on both.
struct MutationTestCase {
    _dir: tempfile::TempDir,
    config: Arc<Config>,
    tmp: PathBuf,
    fake: Arc<FakeTransport>,
    api: MusicApi,
}

impl MutationTestCase {
    fn new() -> MutationTestCase {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new()));
        config.ensure_dirs().unwrap();
        let tmp = config.tmp_dir.join("api-mutations-test");
        amui::util::create_private_dirs(&tmp).unwrap();
        let fake = Arc::new(FakeTransport::new());
        let api = MusicApi::new(config.clone(), fake.clone()).with_cache_dir(&tmp);
        MutationTestCase { _dir: dir, config, tmp, fake, api }
    }

    /// Python: `MusicApi(self.tmp)`.
    fn client(&self) -> MusicApi {
        MusicApi::new(self.config.clone(), self.fake.clone()).with_cache_dir(&self.tmp)
    }
}

/// Python: `assertRaises(web.WebError)`.
fn web_error(result: Result<impl std::fmt::Debug, ApiError>) -> WebError {
    match result {
        Err(ApiError::Web(error)) => error,
        other => panic!("expected a WebError, got {other:?}"),
    }
}

/// Python: `assertRaises(ValueError)`.
fn value_error(result: Result<impl std::fmt::Debug, ApiError>) -> String {
    match result {
        Err(ApiError::Value(message)) => message,
        other => panic!("expected a value error, got {other:?}"),
    }
}

fn favorites(pairs: &[(&str, bool)]) -> HashMap<String, bool> {
    pairs.iter().map(|(id, favorite)| (id.to_string(), *favorite)).collect()
}

const NO_TRACKS: [&Track; 0] = [];

// --- favorites = personal song ratings --------------------------------------------------
mod favorites_test {
    use super::*;

    #[test]
    fn favorite_puts_rating_1_on_the_catalog_song() {
        let t = MutationTestCase::new();
        t.fake.route("PUT", "/v1/me/ratings/songs/1000000002", json!({"data": [ratings()["data"][0]]}));
        assert_eq!(t.api.favorite(&catalog()), Ok(()));
        let rating = json!({"type": "ratings", "attributes": {"value": 1}});
        assert_eq!(t.fake.mutations(), [seen("PUT", "/v1/me/ratings/songs/1000000002", &[], Some(rating))]);
    }

    #[test]
    fn favorite_library_only_song_uses_library_songs() {
        let t = MutationTestCase::new();
        t.fake.route("PUT", "/v1/me/ratings/library-songs/i.AbCdEf5", json!({"data": []}));
        t.api.favorite(&library_only()).unwrap();
        let (method, path, ..) = &t.fake.mutations()[0];
        assert_eq!((method.as_str(), path.as_str()), ("PUT", "/v1/me/ratings/library-songs/i.AbCdEf5"));
    }

    #[test]
    fn favorite_without_identifier_is_rejected_before_any_request() {
        let t = MutationTestCase::new();
        web_error(t.api.favorite(&no_ids()));
        web_error(t.api.favorite(&Track::new(Some("1/../2"), "x", "")));
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn unfavorite_deletes_and_tolerates_a_missing_rating() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("DELETE", "/v1/me/ratings/songs/1000000002", b""); // 204 No Content
        assert_eq!(t.api.unfavorite(&catalog()), Ok(()));
        assert_eq!(t.fake.mutations(), [seen("DELETE", "/v1/me/ratings/songs/1000000002", &[], None)]);
        assert_eq!(t.api.unfavorite(&library_only()), Ok(())); // no route -> HTTP 404 -> already not a favorite
        let (method, path, ..) = t.fake.mutations().pop().unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("DELETE", "/v1/me/ratings/library-songs/i.AbCdEf5"));
    }

    #[test]
    fn is_favorite_maps_rating_1_only() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/ratings/songs", ratings());
        let result = t.api.is_favorite(&["1000000002", "1000000003", "1000000004", "1000000002"]).unwrap();
        assert_eq!(result, favorites(&[("1000000002", true), ("1000000003", false), ("1000000004", false)]));
        let last = t.fake.calls().pop().unwrap();
        // de-duplicated, comma list kept literal
        assert_eq!(last, seen("GET", "/v1/me/ratings/songs", &[("ids", "1000000002,1000000003,1000000004")], None));
        assert!(t.fake.urls.lock().unwrap()[0].ends_with("/v1/me/ratings/songs?ids=1000000002,1000000003,1000000004"));
    }

    /// The Python test also passes `None` among the ids; here an id is always text.
    #[test]
    fn is_favorite_empty_and_unrated() {
        let t = MutationTestCase::new();
        assert_eq!(t.api.is_favorite(&[] as &[&str]).unwrap(), favorites(&[]));
        assert_eq!(t.api.is_favorite(&["", "  "]).unwrap(), favorites(&[]));
        assert_eq!(t.fake.calls(), []);
        // no route -> 404 -> unrated
        assert_eq!(t.api.is_favorite(&["1000000009"]).unwrap(), favorites(&[("1000000009", false)]));
    }

    #[test]
    fn is_favorite_chunks_requests() {
        let t = MutationTestCase::new();
        let ids: Vec<String> = (0..api::RATINGS_LIMIT * 2 + 5).map(|i| (2_000_000_000 + i).to_string()).collect();
        t.fake.route_with("GET", "/v1/me/ratings/songs", |query, _| {
            let first = query["ids"].split(',').next().unwrap().to_string();
            Ok(json!({"data": [{"id": first, "type": "ratings", "attributes": {"value": 1}}]}))
        });
        let result = t.api.is_favorite(&ids).unwrap();
        let calls = t.fake.calls();
        let requests = calls.iter().filter(|(_, path, ..)| path == "/v1/me/ratings/songs");
        let sizes: Vec<usize> = requests.map(|(_, _, query, _)| query["ids"].split(',').count()).collect();
        assert_eq!(sizes, [api::RATINGS_LIMIT, api::RATINGS_LIMIT, 5]);
        assert_eq!(result.values().filter(|favorite| **favorite).count(), 3);
        assert!(result[&ids[0]] && result[&ids[api::RATINGS_LIMIT]] && result[&ids[2 * api::RATINGS_LIMIT]]);
    }

    #[test]
    fn is_favorite_is_cached_until_a_rating_changes() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/ratings/songs", ratings());
        t.fake.route("PUT", "/v1/me/ratings/songs/1000000003", json!({"data": []}));
        t.fake.route_bytes("DELETE", "/v1/me/ratings/songs/1000000003", b"");
        t.api.is_favorite(&["1000000002"]).unwrap();
        t.api.is_favorite(&["1000000002"]).unwrap();
        assert_eq!(t.fake.count("GET", "/v1/me/ratings/songs"), 1);
        t.api.favorite(&second()).unwrap();
        t.api.is_favorite(&["1000000002"]).unwrap();
        assert_eq!(t.fake.count("GET", "/v1/me/ratings/songs"), 2);
        t.api.unfavorite(&second()).unwrap();
        t.api.is_favorite(&["1000000002"]).unwrap();
        assert_eq!(t.fake.count("GET", "/v1/me/ratings/songs"), 3);
    }

    #[test]
    fn bad_identifier_rejected() {
        let t = MutationTestCase::new();
        web_error(t.api.is_favorite(&["1,2"]));
        web_error(t.api.is_favorite(&["1 2"]));
        assert_eq!(t.fake.calls(), []);
    }
}

// --- add to library -----------------------------------------------------------------------
mod add_to_library_test {
    use super::*;

    #[test]
    fn posts_ids_by_kind() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("POST", "/v1/me/library", b""); // 202 Accepted, no body
        assert_eq!(t.api.add_to_library("1000000002", "songs"), Ok(()));
        t.api.add_to_library("1000000001", "albums").unwrap();
        assert_eq!(
            t.fake.mutations(),
            [
                seen("POST", "/v1/me/library", &[("ids[songs]", "1000000002")], None),
                seen("POST", "/v1/me/library", &[("ids[albums]", "1000000001")], None),
            ]
        );
    }

    #[test]
    fn rejects_unknown_kind_and_bad_id() {
        let t = MutationTestCase::new();
        value_error(t.api.add_to_library("1000000002", "stations"));
        web_error(t.api.add_to_library("1&x=2", "songs"));
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn invalidates_library_pages_but_not_catalog() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/library/songs", json!({"data": [lib_song()], "meta": {"total": 1}}));
        t.fake.route("GET", "/v1/catalog/us/songs/1000000002", json!({"data": [cat_song()]}));
        t.fake.route_bytes("POST", "/v1/me/library", b"");
        let counts =
            || (t.fake.count("GET", "/v1/me/library/songs"), t.fake.count("GET", "/v1/catalog/us/songs/1000000002"));
        t.api.library_songs(0, 100, false).unwrap();
        t.api.catalog_song("1000000002").unwrap();
        t.api.library_songs(0, 100, false).unwrap();
        t.api.catalog_song("1000000002").unwrap();
        assert_eq!(counts(), (1, 1));
        t.api.add_to_library("1000000002", "songs").unwrap();
        t.api.library_songs(0, 100, false).unwrap();
        t.api.catalog_song("1000000002").unwrap();
        assert_eq!(counts(), (2, 1));
    }

    #[test]
    fn server_error_propagates_and_still_invalidates() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/library/songs", json!({"data": [lib_song()]}));
        t.fake.route_with("POST", "/v1/me/library", |_, _| {
            Err(WebError::other(format!("{} returned HTTP 500", config::API_HOST)))
        });
        t.api.library_songs(0, 100, false).unwrap();
        let error = web_error(t.api.add_to_library("1000000002", "songs"));
        assert_eq!(error.to_string(), "amp-api.music.apple.com returned HTTP 500");
        t.api.library_songs(0, 100, false).unwrap();
        assert_eq!(t.fake.count("GET", "/v1/me/library/songs"), 2);
    }

    #[test]
    fn non_json_answer_is_an_error() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("POST", "/v1/me/library", b"<html>nope</html>");
        let error = web_error(t.api.add_to_library("1000000002", "songs"));
        assert_eq!(error.to_string(), "Apple returned a response that is not JSON");
    }
}

// --- playlists ----------------------------------------------------------------------------
mod playlist_test {
    use super::*;

    #[test]
    fn create_with_tracks() {
        let t = MutationTestCase::new();
        t.fake.route("POST", "/v1/me/library/playlists", json!({"data": [created()]}));
        let playlist: Playlist =
            t.api.create_playlist("  Synthetic List ", "A test list", [&catalog(), &library_only()]).unwrap();
        let body = json!({
            "attributes": {"name": "Synthetic List", "description": "A test list"},
            "relationships": {"tracks": {"data": [{"id": "1000000002", "type": "songs"},
                                                  {"id": "i.AbCdEf5", "type": "library-songs"}]}}});
        assert_eq!(t.fake.mutations(), [seen("POST", "/v1/me/library/playlists", &[], Some(body))]);
        assert_eq!(
            (playlist.library_id.as_deref(), playlist.name.as_str(), playlist.description.as_str()),
            (Some("p.NewList1"), "Synthetic List", "A test list")
        );
        assert_eq!((playlist.can_edit, playlist.is_library, playlist.key().as_str()), (true, true, "p.NewList1"));
    }

    #[test]
    fn create_minimal_body() {
        let t = MutationTestCase::new();
        t.fake.route("POST", "/v1/me/library/playlists", json!({"data": [created()]}));
        t.api.create_playlist("Just a name", "", NO_TRACKS).unwrap();
        assert_eq!(t.fake.mutations()[0].3, Some(json!({"attributes": {"name": "Just a name"}})));
    }

    #[test]
    fn create_rejects_blank_name_and_id_less_tracks() {
        let t = MutationTestCase::new();
        value_error(t.api.create_playlist("   ", "", NO_TRACKS));
        web_error(t.api.create_playlist("List", "", [&catalog(), &no_ids()]));
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn create_without_returned_resource_is_an_error() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("POST", "/v1/me/library/playlists", b"");
        web_error(t.api.create_playlist("List", "", NO_TRACKS));
        t.fake.route("POST", "/v1/me/library/playlists", json!({"data": []}));
        web_error(t.api.create_playlist("List", "", NO_TRACKS));
    }

    #[test]
    fn add_tracks() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("POST", "/v1/me/library/playlists/p.NewList1/tracks", b""); // 204
        assert_eq!(t.api.add_to_playlist("p.NewList1", [&catalog(), &second(), &library_only()]), Ok(()));
        let body = json!({"data": [{"id": "1000000002", "type": "songs"}, {"id": "1000000003", "type": "songs"},
                                   {"id": "i.AbCdEf5", "type": "library-songs"}]});
        assert_eq!(t.fake.mutations(), [seen("POST", "/v1/me/library/playlists/p.NewList1/tracks", &[], Some(body))]);
    }

    #[test]
    fn add_tracks_nothing_to_send_and_bad_ids() {
        let t = MutationTestCase::new();
        assert_eq!(t.api.add_to_playlist("p.NewList1", NO_TRACKS), Ok(()));
        web_error(t.api.add_to_playlist("p.x/../../v1/me", [&catalog()]));
        web_error(t.api.add_to_playlist("p.NewList1", [&no_ids()]));
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn mutations_refresh_playlist_pages() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/library/playlists", json!({"data": [created()], "meta": {"total": 1}}));
        t.fake.route(
            "GET",
            "/v1/me/library/playlists/p.NewList1/tracks",
            json!({"data": [lib_song()], "meta": {"total": 1}}),
        );
        t.fake.route("GET", "/v1/me/library/recently-added", json!({"data": [created()]}));
        t.fake.route("POST", "/v1/me/library/playlists", json!({"data": [created()]}));
        t.fake.route_bytes("POST", "/v1/me/library/playlists/p.NewList1/tracks", b"");
        for _ in 0..2 {
            t.api.library_playlists(0, 100).unwrap();
            t.api.library_playlist_tracks("p.NewList1", 0, 100).unwrap();
            t.api.library_recently_added(0, 25).unwrap();
        }
        let counts = || {
            (
                t.fake.count("GET", "/v1/me/library/playlists"),
                t.fake.count("GET", "/v1/me/library/playlists/p.NewList1/tracks"),
                t.fake.count("GET", "/v1/me/library/recently-added"),
            )
        };
        assert_eq!(counts(), (1, 1, 1));
        t.api.create_playlist("Another", "", NO_TRACKS).unwrap();
        t.api.library_playlists(0, 100).unwrap();
        t.api.library_recently_added(0, 25).unwrap();
        assert_eq!(counts(), (2, 1, 2));
        t.api.add_to_playlist("p.NewList1", [&catalog()]).unwrap();
        t.api.library_playlist_tracks("p.NewList1", 0, 100).unwrap();
        t.api.library_playlists(0, 100).unwrap();
        assert_eq!(counts(), (3, 2, 2));
    }

    #[test]
    fn rename_patches_name_and_description() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("PATCH", "/v1/me/library/playlists/p.NewList1", b""); // 204
        assert_eq!(t.api.rename_playlist("p.NewList1", Some("  New name "), Some("desc")), Ok(()));
        t.api.rename_playlist("p.NewList1", Some(""), Some("")).unwrap(); // keep the name, clear the description
        t.api.rename_playlist("p.NewList1", Some("Only the name"), None).unwrap();
        t.api.rename_playlist("p.NewList1", None, None).unwrap(); // nothing to change: no request
        let patch = |attributes: Value| {
            seen("PATCH", "/v1/me/library/playlists/p.NewList1", &[], Some(json!({"attributes": attributes})))
        };
        assert_eq!(
            t.fake.mutations(),
            [
                patch(json!({"name": "New name", "description": "desc"})),
                patch(json!({"description": ""})),
                patch(json!({"name": "Only the name"})),
            ]
        );
    }

    #[test]
    fn delete_playlist() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("DELETE", "/v1/me/library/playlists/p.NewList1", b""); // 204
        assert_eq!(t.api.delete_playlist("p.NewList1"), Ok(()));
        assert_eq!(t.fake.mutations(), [seen("DELETE", "/v1/me/library/playlists/p.NewList1", &[], None)]);
        web_error(t.api.delete_playlist("p.x/../../v1/me"));
        // Apple's own playlists (canDelete false) or an unknown id
        assert!(matches!(web_error(t.api.delete_playlist("p.Missing")), WebError::NotFound(_)));
    }

    #[test]
    fn remove_tracks_one_request_per_library_song() {
        let t = MutationTestCase::new();
        t.fake.route_bytes("DELETE", "/v1/me/library/playlists/p.NewList1/tracks", b""); // 204
        let (in_playlist, upload) = (catalog(), library_only());
        let tracks = [TrackRef::from(&in_playlist), TrackRef::from(&upload), TrackRef::from("i.AbCdEf4")];
        assert_eq!(t.api.remove_from_playlist("p.NewList1", tracks), Ok(()));
        let removal = |id: &str| {
            seen(
                "DELETE",
                "/v1/me/library/playlists/p.NewList1/tracks",
                &[("ids[library-songs]", id), ("mode", "all")],
                None,
            )
        };
        assert_eq!(t.fake.mutations(), [removal("i.AbCdEf4"), removal("i.AbCdEf5")]);
        // The Python test ends by comparing the percent-encoded query with itself. What goes out,
        // from either engine, has the brackets as they are:
        assert!(t.fake.urls.lock().unwrap()[0].ends_with("/tracks?ids[library-songs]=i.AbCdEf4&mode=all"));
    }

    #[test]
    fn remove_tracks_needs_library_ids() {
        let t = MutationTestCase::new();
        // catalog ids only: not in the playlist listing
        web_error(t.api.remove_from_playlist(
            "p.NewList1",
            [&Track::new(Some("1000000002"), "Synthetic Song", "Synth Artist"), &second()],
        ));
        web_error(t.api.remove_from_playlist("p.NewList1", [&no_ids()]));
        assert_eq!(t.api.remove_from_playlist("p.NewList1", NO_TRACKS), Ok(()));
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn playlist_edits_drop_the_library_cache() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/library/playlists", json!({"data": [created()], "meta": {"total": 1}}));
        t.fake.route(
            "GET",
            "/v1/me/library/playlists/p.NewList1/tracks",
            json!({"data": [lib_song()], "meta": {"total": 1}}),
        );
        t.fake.route_bytes("PATCH", "/v1/me/library/playlists/p.NewList1", b"");
        t.fake.route_bytes("DELETE", "/v1/me/library/playlists/p.NewList1", b"");
        t.fake.route_bytes("DELETE", "/v1/me/library/playlists/p.NewList1/tracks", b"");
        t.api.library_playlists(0, 100).unwrap();
        t.api.library_playlist_tracks("p.NewList1", 0, 100).unwrap();
        t.api.rename_playlist("p.NewList1", Some("x"), None).unwrap();
        t.api.library_playlists(0, 100).unwrap();
        t.api.remove_from_playlist("p.NewList1", ["i.AbCdEf4"]).unwrap();
        t.api.library_playlist_tracks("p.NewList1", 0, 100).unwrap();
        t.api.delete_playlist("p.NewList1").unwrap();
        t.api.library_playlists(0, 100).unwrap();
        assert_eq!(t.fake.count("GET", "/v1/me/library/playlists"), 3);
        assert_eq!(t.fake.count("GET", "/v1/me/library/playlists/p.NewList1/tracks"), 2);
    }

    #[test]
    fn not_supported_class_remains_for_callers() {
        let error = ApiError::from(NotSupported::new("why"));
        assert!(error.web().is_none() && !matches!(error, ApiError::Web(_))); // the UI tells the two apart
        assert_eq!(NotSupported::new("why").reason, "why");
    }
}

// --- catalog storefront override ----------------------------------------------------------
mod storefront_override_test {
    use super::*;

    #[test]
    fn default_follows_the_account() {
        let t = MutationTestCase::new();
        assert_eq!(t.api.catalog_storefront().unwrap(), "us");
        assert_eq!(t.api.catalog_storefront_override(), None);
        assert!(!t.tmp.join("storefront-override.json").exists());
    }

    #[test]
    fn override_applies_to_catalog_search_charts_and_lyrics() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/catalog/jp/songs/1000000002", json!({"data": [cat_song()]}));
        t.fake.route("GET", "/v1/catalog/jp/search", json!({"results": {"songs": {"data": [cat_song()]}}}));
        t.fake.route(
            "GET",
            "/v1/catalog/jp/charts",
            json!({"results": {"songs": [{"name": "Top", "data": [cat_song()]}]}}),
        );
        t.api.set_catalog_storefront(Some(" JP ")).unwrap();
        assert_eq!(
            (t.api.catalog_storefront().unwrap().as_str(), t.api.catalog_storefront_override().as_deref()),
            ("jp", Some("jp"))
        );
        assert_eq!(t.api.storefront().unwrap(), "us"); // the account's storefront is untouched
        assert_eq!(t.api.catalog_song("1000000002").unwrap().catalog_id.as_deref(), Some("1000000002"));
        assert_eq!(t.api.search_catalog("synthetic", 10).unwrap().songs.len(), 1);
        assert_eq!(t.api.charts().unwrap().titles["songs"].as_deref(), Some("Top"));
        assert_eq!(t.api.lyrics("1000000002").unwrap(), None); // the fake has no lyrics -> None, but the path was /jp/
        let calls = t.fake.calls();
        let paths: Vec<&str> =
            calls.iter().map(|(_, path, ..)| path.as_str()).filter(|path| path.starts_with("/v1/catalog/")).collect();
        assert_eq!(
            paths,
            [
                "/v1/catalog/jp/songs/1000000002",
                "/v1/catalog/jp/search",
                "/v1/catalog/jp/charts",
                "/v1/catalog/jp/songs/1000000002"
            ]
        );
        assert_eq!(t.fake.mutations(), []); // the override never talks to Apple
    }

    #[test]
    fn persisted_privately_and_shared_by_a_new_instance() {
        use std::os::unix::fs::PermissionsExt;
        let t = MutationTestCase::new();
        t.api.set_catalog_storefront(Some("jp")).unwrap();
        let path = t.tmp.join("storefront-override.json");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["id"], "jp");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let other = t.client();
        assert_eq!(other.catalog_storefront().unwrap(), "jp");
        assert_eq!(t.fake.count("GET", "/v1/me/storefront"), 0); // no network needed for the override
        other.set_catalog_storefront(None).unwrap();
        assert!(!path.exists());
        assert_eq!(other.catalog_storefront().unwrap(), "us");
        assert_eq!(t.api.catalog_storefront_override().as_deref(), Some("jp")); // the first instance keeps its own view
        t.api.set_catalog_storefront(Some("")).unwrap();
        assert_eq!(t.api.catalog_storefront().unwrap(), "us");
    }

    #[test]
    fn invalid_codes() {
        let t = MutationTestCase::new();
        for code in ["zz", "usa", "u", "x k", "1a"] {
            value_error(t.api.set_catalog_storefront(Some(code)));
        }
        assert_eq!(t.api.catalog_storefront_override(), None);
        assert!(!t.tmp.join("storefront-override.json").exists());
    }

    #[test]
    fn corrupt_or_unknown_file_is_ignored() {
        let t = MutationTestCase::new();
        let path = t.tmp.join("storefront-override.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(t.client().catalog_storefront().unwrap(), "us");
        std::fs::write(&path, json!({"id": "../x"}).to_string()).unwrap();
        assert_eq!(t.client().catalog_storefront().unwrap(), "us");
        std::fs::write(&path, json!({"id": "GB"}).to_string()).unwrap();
        assert_eq!(t.client().catalog_storefront().unwrap(), "gb");
    }

    #[test]
    fn code_list() {
        assert_eq!(api::STOREFRONT_CODES.len(), 250); // ISO 3166-1 alpha-2 (249) + Apple's "xk"
        let codes: HashSet<&str> = api::STOREFRONT_CODES.into_iter().collect();
        assert_eq!(codes.len(), 250); // a set in Python: no code twice
        assert!(["us", "gb", "jp", "fr", "de", "kr", "xk"].iter().all(|code| codes.contains(code)));
        assert!(codes.iter().all(|code| code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_lowercase())));
    }
}

/// What the Python tests leave unsaid.
mod beyond_python {
    use super::*;

    #[test]
    fn value_errors_keep_their_texts() {
        let t = MutationTestCase::new();
        assert_eq!(
            value_error(t.api.add_to_library("1000000002", "stations")),
            "kind must be one of songs, albums, playlists, music-videos"
        );
        assert_eq!(value_error(t.api.create_playlist(" \t", "", NO_TRACKS)), "A playlist needs a name");
        assert_eq!(value_error(t.api.set_catalog_storefront(Some(" ZZ "))), "'ZZ' is not an ISO 3166-1 country code");
        assert_eq!(value_error(t.api.set_catalog_storefront(Some("x k"))), "'x k' is not an ISO 3166-1 country code");
        let error = t.api.set_catalog_storefront(Some("it's")).unwrap_err();
        assert_eq!(error.to_string(), "\"it's\" is not an ISO 3166-1 country code");
        assert!(!error.is_auth() && error.web().is_none());
        assert_eq!(t.fake.calls(), []);
    }

    #[test]
    fn web_errors_keep_their_texts() {
        let t = MutationTestCase::new();
        let message = |result: Result<(), ApiError>| web_error(result).to_string();
        assert_eq!(message(t.api.favorite(&no_ids())), "This track has no Apple Music identifier to rate");
        assert_eq!(message(t.api.favorite("1/2")), "Invalid Apple Music identifier");
        assert_eq!(
            message(t.api.add_to_playlist("p.NewList1", [&no_ids()])),
            "A track without an Apple Music identifier cannot be added to a playlist"
        );
        assert_eq!(message(t.api.add_to_playlist("p.NewList1", ["1 2"])), "Invalid Apple Music identifier");
        assert_eq!(
            message(t.api.remove_from_playlist("p.NewList1", [&second()])),
            "Only songs listed by the playlist itself (library ids) can be removed from it"
        );
        assert_eq!(
            message(t.api.remove_from_playlist("p.NewList1", [""])),
            "Only songs listed by the playlist itself (library ids) can be removed from it"
        );
        // the playlist's id is looked at first, the songs' after
        assert_eq!(message(t.api.remove_from_playlist(" ", NO_TRACKS)), "Invalid Apple Music identifier");
        assert_eq!(message(t.api.remove_from_playlist("p.NewList1", ["i.x", "i,y"])), "Invalid Apple Music identifier");
        assert_eq!(t.fake.calls(), []);
        t.fake.route_bytes("POST", "/v1/me/library/playlists", b"[1, 2]");
        assert_eq!(
            web_error(t.api.create_playlist("List", "", NO_TRACKS)).to_string(),
            "Apple created the playlist but did not return it; refresh the library"
        );
        t.fake.route("POST", "/v1/me/library/playlists", json!({"errors": []}));
        assert_eq!(
            web_error(t.api.create_playlist("List", "", NO_TRACKS)).to_string(),
            "Apple returned a response without a data list"
        );
    }

    /// "Plain id strings are accepted too (catalog songs)" (research/engine-api.md).
    #[test]
    fn bare_ids_stand_for_catalog_songs() {
        let t = MutationTestCase::new();
        t.fake.route("PUT", "/v1/me/ratings/songs/1000000002", json!({"data": []}));
        t.fake.route_bytes("DELETE", "/v1/me/ratings/songs/1000000002", b" \n");
        t.fake.route_bytes("POST", "/v1/me/library/playlists/p.NewList1/tracks", b"");
        t.api.favorite("1000000002").unwrap();
        t.api.unfavorite(&String::from(" 1000000002 ")).unwrap();
        let ids = vec!["1000000002", "1000000003"];
        t.api.add_to_playlist("p.NewList1", &ids).unwrap();
        let tracks = vec![second(), library_only()];
        t.api.add_to_playlist("p.NewList1", &tracks).unwrap();
        let by_reference: Vec<&Track> = tracks.iter().collect();
        t.api.add_to_playlist("p.NewList1", &by_reference).unwrap();
        let bodies: Vec<Option<Value>> = t.fake.mutations().into_iter().map(|(.., body)| body).collect();
        let songs = json!({"data": [{"id": "1000000002", "type": "songs"}, {"id": "1000000003", "type": "songs"}]});
        let mixed =
            json!({"data": [{"id": "1000000003", "type": "songs"}, {"id": "i.AbCdEf5", "type": "library-songs"}]});
        let rating = json!({"type": "ratings", "attributes": {"value": 1}});
        assert_eq!(bodies, [Some(rating), None, Some(songs), Some(mixed.clone()), Some(mixed)]);
    }

    #[test]
    fn unfavorite_reports_what_is_not_a_missing_rating() {
        let t = MutationTestCase::new();
        t.fake.route_with("DELETE", "/v1/me/ratings/songs/1000000002", |_, _| {
            Err(WebError::auth(
                "amp-api.music.apple.com rejected the session (HTTP 401); reconnect to Apple Music",
                Some(401),
            ))
        });
        let error = t.api.unfavorite(&catalog()).unwrap_err();
        assert!(error.is_auth());
        assert_eq!(error.web().and_then(WebError::status), Some(401));
    }

    #[test]
    fn invalidate_forgets_the_library_or_the_named_paths() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/library/songs", json!({"data": [lib_song()]}));
        t.fake.route("GET", "/v1/me/library/playlists", json!({"data": [created()]}));
        t.fake.route("GET", "/v1/me/ratings/songs", ratings());
        t.fake.route("GET", "/v1/catalog/us/songs/1000000002", json!({"data": [cat_song()]}));
        let read_all = || {
            t.api.library_songs(0, 100, false).unwrap();
            t.api.library_playlists(0, 100).unwrap();
            t.api.is_favorite(&["1000000002"]).unwrap();
            t.api.catalog_song("1000000002").unwrap();
        };
        let counts = || {
            [
                "/v1/me/library/songs",
                "/v1/me/library/playlists",
                "/v1/me/ratings/songs",
                "/v1/catalog/us/songs/1000000002",
            ]
            .map(|path| t.fake.count("GET", path))
        };
        read_all();
        read_all();
        assert_eq!(counts(), [1, 1, 1, 1]);
        t.api.invalidate(&[]); // the whole library
        read_all();
        assert_eq!(counts(), [2, 2, 1, 1]);
        t.api.invalidate(&["/v1/me/library/playlists", "/v1/me/ratings"]);
        read_all();
        assert_eq!(counts(), [2, 3, 2, 1]);
        t.api.clear_cache();
        read_all();
        assert_eq!(counts(), [3, 4, 3, 2]);
    }

    #[test]
    fn a_failed_rating_still_drops_the_cached_ratings() {
        let t = MutationTestCase::new();
        t.fake.route("GET", "/v1/me/ratings/songs", ratings());
        t.fake.route("GET", "/v1/me/library/songs", json!({"data": [lib_song()]}));
        t.api.is_favorite(&["1000000002"]).unwrap();
        t.api.library_songs(0, 100, false).unwrap();
        // no route for the PUT: Apple's 404
        assert!(matches!(web_error(t.api.favorite(&second())), WebError::NotFound(_)));
        t.api.is_favorite(&["1000000002"]).unwrap();
        t.api.library_songs(0, 100, false).unwrap();
        assert_eq!((t.fake.count("GET", "/v1/me/ratings/songs"), t.fake.count("GET", "/v1/me/library/songs")), (2, 1));
    }
}
