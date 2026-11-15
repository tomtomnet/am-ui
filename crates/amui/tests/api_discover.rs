//! Tests for what `amui::api` has beyond the Python client: stations, the pages made of shelves
//! (Home, New, Radio), the station that continues a queue, and the current version of a song.
//!
//! Everything is synthetic: the client gets its own `Http` (no network) and made-up resources in
//! the shapes Apple answers with.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use amui::api::{self, ApiError, MusicApi, Seed};
use amui::config::{self, Config};
use amui::types::{Item, ShelfLayout, Track};
use amui::util::Event;
use amui::web::{Call, Http, Progress, Source, WebError};
use serde_json::{Value, json};

// --- synthetic resources -------------------------------------------------------

fn art() -> Value {
    json!({"url": "https://is1-ssl.mzstatic.com/image/thumb/Music/x/y/z.jpg/{w}x{h}bb.jpg", "width": 1500, "height": 1500,
           "bgColor": "102030"})
}

fn station(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "stations", "attributes": {
        "artwork": art(), "isLive": false, "kind": "editorial", "mediaKind": "audio", "name": name,
        "plainEditorialNotes": {"name": name, "short": "Songs chosen for the test.", "tagline": "A tagline"},
        "playParams": {"format": "tracks", "hasDrm": false, "id": id, "kind": "radioStation", "mediaType": 0,
                       "stationHash": "hash"},
        "url": "https://music.apple.com/us/station/x/ra.1"}})
}

fn live_station(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "stations", "attributes": {
        "artwork": art(), "isLive": true, "kind": "streaming", "mediaKind": "audio", "name": name,
        "plainEditorialNotes": {"name": name},
        "playParams": {"format": "stream", "hasDrm": true, "id": id, "kind": "radioStation", "mediaType": 0,
                       "stationHash": "hash"}}})
}

fn album(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "albums", "attributes": {
        "artistName": "Synth Artist", "artwork": art(), "name": name, "playParams": {"id": id, "kind": "album"},
        "releaseDate": "2020-05-06", "trackCount": 3, "url": format!("https://music.apple.com/us/album/x/{id}")}})
}

fn playlist(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "playlists", "attributes": {
        "artwork": art(), "curatorName": "Synth Curator", "name": name, "playParams": {"id": id, "kind": "playlist"},
        "url": format!("https://music.apple.com/us/playlist/x/{id}")}})
}

fn song(id: &str, name: &str, artist: &str, album: &str, millis: i64, rating: Option<&str>) -> Value {
    let mut attributes = json!({
        "albumName": album, "artistName": artist, "artwork": art(), "durationInMillis": millis, "name": name,
        "playParams": {"id": id, "kind": "song"},
        "url": format!("https://music.apple.com/us/album/x/1000000001?i={id}")});
    if let Some(rating) = rating {
        attributes["contentRating"] = json!(rating);
    }
    json!({"id": id, "type": "songs", "attributes": attributes})
}

fn video(id: &str) -> Value {
    json!({"id": id, "type": "music-videos", "attributes": {"artistName": "Synth Artist", "name": "A Video"}})
}

fn with_reason(mut item: Value, reason: &str) -> Value {
    item["meta"] = json!({"reason": {"stringForDisplay": reason}});
    item
}

fn recommendation(id: &str, title: &str, display: &str, contents: Vec<Value>) -> Value {
    json!({"id": id, "type": "personal-recommendation", "attributes": {
        "display": {"kind": display}, "hasSeeAll": false, "kind": "music-recommendations",
        "title": {"stringForDisplay": title}}, "relationships": {"contents": {"data": contents}}})
}

fn element(id: &str, kind: &str, attributes: Value, relationships: Value) -> Value {
    let mut attributes = attributes;
    attributes["editorialElementKind"] = json!(kind);
    json!({"id": id, "type": "editorial-elements", "attributes": attributes, "relationships": relationships})
}

fn hero_child(kind: &str, badge: &str, content: Value) -> Value {
    element("c", kind, json!({"designBadge": badge}), json!({"contents": {"data": [content]}}))
}

fn editorial_page(elements: Vec<Value>) -> Value {
    let tab = element("tab", "331", json!({}), json!({"children": {"data": elements}}));
    json!({"data": [{"id": "g", "type": "groupings", "attributes": {"name": "Music"},
                     "relationships": {"tabs": {"data": [tab]}}}]})
}

/// A storefront that has Apple's words in these languages.
fn storefront(languages: &[&str]) -> Value {
    json!({"data": [{"id": "us", "type": "storefronts", "attributes": {
        "defaultLanguageTag": languages[0], "name": "A Storefront", "supportedLanguageTags": languages}}]})
}

// --- fake transport ---------------------------------------------------------------

type Query = HashMap<String, String>;

#[derive(Debug, Clone)]
struct Asked {
    method: String,
    path: String,
    query: Query,
    body: Option<Value>,
    auth: bool,
}

type Handler = Arc<dyn Fn(&Asked) -> Result<Value, WebError> + Send + Sync>;

/// Routes requests ("GET /v1/…") to canned answers; records every request.
struct FakeWeb {
    asked: Mutex<Vec<Asked>>,
    routes: Mutex<HashMap<String, Handler>>,
}

impl FakeWeb {
    fn new() -> FakeWeb {
        let fake = FakeWeb { asked: Mutex::default(), routes: Mutex::default() };
        fake.route("GET /v1/me/storefront", json!({"data": [{"id": "us", "type": "storefronts"}]}));
        fake.route("GET /v1/storefronts/us", storefront(&["en-US", "es-MX"]));
        fake
    }

    fn route(&self, request: &str, answer: Value) {
        self.route_with(request, move |_| Ok(answer.clone()));
    }

    fn route_with(&self, request: &str, handler: impl Fn(&Asked) -> Result<Value, WebError> + Send + Sync + 'static) {
        self.routes.lock().unwrap().insert(request.to_string(), Arc::new(handler));
    }

    fn asked(&self, request: &str) -> Vec<Asked> {
        let asked = self.asked.lock().unwrap();
        asked.iter().filter(|asked| format!("{} {}", asked.method, asked.path) == request).cloned().collect()
    }
}

impl Http for FakeWeb {
    fn request(&self, call: &Call) -> Result<Vec<u8>, WebError> {
        let parts = url::Url::parse(call.url).unwrap();
        assert!(parts.scheme() == "https" && parts.host_str() == Some(config::API_HOST), "{}", call.url);
        let mut query = Query::new();
        for (name, value) in parts.query_pairs() {
            query.entry(name.into_owned()).or_insert(value.into_owned());
        }
        let body = call.body.map(|body| serde_json::from_slice(body).unwrap());
        let asked =
            Asked { method: call.method.to_string(), path: parts.path().to_string(), query, body, auth: call.auth };
        self.asked.lock().unwrap().push(asked.clone());
        let handler = self.routes.lock().unwrap().get(&format!("{} {}", asked.method, asked.path)).cloned();
        let Some(handler) = handler else {
            return Err(WebError::NotFound(format!("{} returned HTTP 404", config::API_HOST)));
        };
        Ok(match handler(&asked)? {
            Value::Null => Vec::new(),
            answer => answer.to_string().into_bytes(),
        })
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

struct Case {
    _dir: tempfile::TempDir,
    fake: Arc<FakeWeb>,
    api: MusicApi,
}

/// A client whose desktop speaks American English.
fn case() -> Case {
    case_in("en_US.UTF-8")
}

/// A client for a desktop with this locale ("": none).
fn case_in(locale: &str) -> Case {
    let dir = tempfile::tempdir().unwrap();
    let env = HashMap::from([("LANG".to_string(), locale.to_string())]);
    let config = Arc::new(Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &env));
    config.ensure_dirs().unwrap();
    let cache = config.tmp_dir.join("api-test");
    amui::util::create_private_dirs(&cache).unwrap();
    let fake = Arc::new(FakeWeb::new());
    let api = MusicApi::new(config, fake.clone()).with_cache_dir(cache);
    Case { _dir: dir, fake, api }
}

fn kinds(items: &[Item]) -> Vec<&'static str> {
    items.iter().map(Item::kind).collect()
}

fn titles(items: &[Item]) -> Vec<&str> {
    items.iter().map(Item::title).collect()
}

// --- resources ----------------------------------------------------------------------

#[test]
fn a_station_of_songs_and_a_live_one() {
    let parsed = api::parse_station(&station("ra.1000000001", "Station A")).unwrap();
    assert_eq!((parsed.id.as_str(), parsed.name.as_str()), ("ra.1000000001", "Station A"));
    assert_eq!(parsed.subtitle, "Songs chosen for the test.");
    assert_eq!((parsed.kind.as_str(), parsed.format.as_str(), parsed.live), ("editorial", "tracks", false));
    assert!(parsed.has_tracks());
    assert_eq!(parsed.artwork.unwrap().bg_color.as_deref(), Some("102030"));

    // the note that only repeats the name is no subtitle
    let live = api::parse_station(&live_station("ra.1000000002", "Radio A")).unwrap();
    assert_eq!((live.subtitle.as_str(), live.live, live.format.as_str()), ("Live Radio", true, "stream"));
    assert!(!live.has_tracks());

    let mut plain = station("ra.1000000003", "Station B");
    plain["attributes"].as_object_mut().unwrap().remove("plainEditorialNotes");
    plain["attributes"]["editorialNotes"] = json!({"tagline": "From the older notes"});
    assert_eq!(api::parse_station(&plain).unwrap().subtitle, "From the older notes");
    plain["attributes"].as_object_mut().unwrap().remove("editorialNotes");
    assert_eq!(api::parse_station(&plain).unwrap().subtitle, "Station");

    assert!(api::parse_station(&json!({"id": "ra.1", "type": "stations"})).is_none());
    assert!(api::parse_station(&json!({"type": "stations", "attributes": {"name": "No id"}})).is_none());
}

#[test]
fn a_shelf_item_is_any_item_or_a_station() {
    assert!(matches!(api::parse_shelf_item(&station("ra.1", "S")), Some(Item::Station(_))));
    assert!(matches!(api::parse_shelf_item(&album("1000000001", "A")), Some(Item::Album(_))));
    assert!(matches!(api::parse_shelf_item(&playlist("pl.1", "P")), Some(Item::Playlist(_))));
    let track = song("1000000002", "Song", "Artist", "Album", 1000, None);
    assert!(matches!(api::parse_shelf_item(&track), Some(Item::Track(_))));
    assert!(api::parse_shelf_item(&video("1000000009")).is_none());
    // the listings of the Python client still leave stations out
    assert!(api::parse_item(&station("ra.1", "S")).is_none());
}

// --- Home -----------------------------------------------------------------------------

fn home_answer() -> Value {
    let picks = vec![
        with_reason(station("ra.1000000001", "Station A"), "Made for You"),
        with_reason(playlist("pl.1000000002", "Playlist A"), "Updated Playlist"),
        json!({"id": "e1", "type": "editorial-items", "attributes": {"name": "An editorial card"}}),
        album("1000000003", "Album A"),
    ];
    let songs = vec![
        song("1000000011", "Song A", "Artist", "Album", 1000, None),
        song("1000000012", "Song B", "Artist", "Album", 1000, None),
    ];
    json!({"data": [
        recommendation("r1", "Top Picks for You", "MusicNotesHeroShelf", picks),
        recommendation("r2", "Recently Played", "MusicCoverShelf",
                       vec![with_reason(album("1000000004", "Album B"), "ignored"), station("ra.1000000005", "Station B")]),
        recommendation("r3", "Concerts", "MusicConcertsEmptyShelf",
                       vec![json!({"id": "e2", "type": "editorial-items", "attributes": {}})]),
        recommendation("r4", "Songs for You", "MusicCoverShelf", songs),
        {"id": "r5", "type": "personal-recommendation", "attributes": {"title": "A plain title"},
         "relationships": {"contents": {"data": [album("1000000006", "Album C")]}}},
    ]})
}

#[test]
fn home_makes_a_shelf_of_each_recommendation() {
    let t = case();
    t.fake.route("GET /v1/me/recommendations", home_answer());
    let shelves = t.api.home().unwrap();
    let shown: Vec<(&str, ShelfLayout)> = shelves.iter().map(|shelf| (shelf.title.as_str(), shelf.layout)).collect();
    assert_eq!(
        shown,
        [
            ("Top Picks for You", ShelfLayout::Hero),
            ("Recently Played", ShelfLayout::Cards),
            ("Songs for You", ShelfLayout::Songs),
            ("A plain title", ShelfLayout::Cards),
        ]
    );
    // the top picks say why each item is there; what cannot be shown takes its note with it
    assert_eq!(kinds(&shelves[0].items), ["station", "playlist", "album"]);
    assert_eq!(shelves[0].notes, ["Made for You", "Updated Playlist", ""]);
    assert_eq!(shelves[0].id.as_deref(), Some("r1"));
    assert_eq!(titles(&shelves[1].items), ["Album B", "Station B"]);
    assert!(shelves[1].notes.is_empty() && shelves[2].notes.is_empty());

    let asked = t.fake.asked("GET /v1/me/recommendations");
    assert_eq!(asked.len(), 1);
    let query = &asked[0].query;
    assert_eq!(query["name"], "listen-now");
    assert_eq!(query["l"], "en-US");
    assert_eq!(query["with"], "library");
    assert!(query["types"].split(',').any(|kind| kind == "stations"));
    assert!(asked[0].auth);

    // kept for a while: Home is not asked for again
    assert_eq!(t.api.home().unwrap(), shelves);
    assert_eq!(t.fake.asked("GET /v1/me/recommendations").len(), 1);
    t.api.clear_cache();
    t.api.home().unwrap();
    assert_eq!(t.fake.asked("GET /v1/me/recommendations").len(), 2);
}

#[test]
fn home_without_recommendations_is_empty_and_a_refused_sign_in_is_told() {
    let t = case();
    t.fake.route("GET /v1/me/recommendations", json!({"data": []}));
    assert!(t.api.home().unwrap().is_empty());

    let t = case();
    t.fake.route_with("GET /v1/me/recommendations", |_| {
        Err(WebError::Auth { message: "rejected".into(), status: Some(403) })
    });
    assert!(t.api.home().unwrap_err().is_auth());

    let t = case();
    t.fake.route("GET /v1/me/recommendations", json!({"results": {}}));
    assert!(matches!(t.api.home(), Err(ApiError::Web(_))));
}

// --- New and Radio --------------------------------------------------------------------

fn new_page() -> Value {
    let heroes = vec![
        hero_child("317", "New Album", album("1000000001", "Album A")),
        hero_child("317", "", playlist("pl.1000000002", "Playlist A")),
        hero_child("317", "A Video", video("1000000009")),
    ];
    let songs = vec![
        song("1000000011", "Song A", "Artist", "Album", 1000, None),
        album("1000000019", "Not a song"),
        song("1000000012", "Song B", "Artist", "Album", 1000, None),
    ];
    editorial_page(vec![
        element("e1", "316", json!({}), json!({"children": {"data": heroes}})),
        element("e2", "327", json!({"name": "Best New Songs"}), json!({"contents": {"data": songs}})),
        element(
            "e3",
            "326",
            json!({"name": "New Music"}),
            json!({
            "contents": {"data": [album("1000000003", "Album B"), video("1000000008"), station("ra.1000000004", "Station A")]},
            "room": {"data": [{"id": "room1", "type": "rooms"}]}}),
        ),
        element("e4", "326", json!({"name": "Videos"}), json!({"contents": {"data": [video("1000000007")]}})),
        element(
            "e5",
            "385",
            json!({"name": "Bricks"}),
            json!({"children": {"data": [
            hero_child("386", "", playlist("pl.1000000005", "Playlist B"))]}}),
        ),
        element(
            "e6",
            "391",
            json!({"name": "More to Explore", "links": [{"label": "x", "url": "https://music.apple.com/x"}]}),
            json!({}),
        ),
        element(
            "e7",
            "404",
            json!({"name": "A paragraph"}),
            json!({"contents": {"data": [album("1000000006", "Hidden")]}}),
        ),
    ])
}

#[test]
fn browse_reads_the_editorial_page() {
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", new_page());
    let shelves = t.api.browse().unwrap();
    let shown: Vec<(&str, ShelfLayout, Vec<&str>)> =
        shelves.iter().map(|shelf| (shelf.title.as_str(), shelf.layout, titles(&shelf.items))).collect();
    assert_eq!(
        shown,
        [
            ("", ShelfLayout::Hero, vec!["Album A", "Playlist A"]),
            ("Best New Songs", ShelfLayout::Songs, vec!["Song A", "Song B"]),
            ("New Music", ShelfLayout::Cards, vec!["Album B", "Station A"]),
            ("Bricks", ShelfLayout::Cards, vec!["Playlist B"]),
        ]
    );
    assert_eq!(shelves[0].notes, ["New Album", ""]);
    assert_eq!(shelves[0].id.as_deref(), Some("e1"));
    assert!(shelves[1].notes.is_empty());

    let asked = t.fake.asked("GET /v1/editorial/us/groupings");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].query["name"], "music");
    assert_eq!(asked[0].query["tabs"], "subscriber");
    assert_eq!(asked[0].query["l"], "en-US");
}

#[test]
fn an_editorial_page_without_a_tab_is_empty() {
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", json!({"data": []}));
    assert!(t.api.browse().unwrap().is_empty());
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", json!({"data": [{"id": "g", "type": "groupings"}]}));
    assert!(t.api.browse().unwrap().is_empty());
}

fn radio_page() -> Value {
    editorial_page(vec![
        element(
            "e1",
            "316",
            json!({}),
            json!({"children": {"data": [
            hero_child("383", "Live", live_station("ra.1000000001", "Radio A"))]}}),
        ),
        element(
            "e2",
            "488",
            json!({"title": "Live Radio Now"}),
            json!({"children": {"data": [
            hero_child("489", "", live_station("ra.1000000002", "Radio B"))]}}),
        ),
        element(
            "e3",
            "326",
            json!({"name": "Stations by Genre"}),
            json!({"contents": {"data": [
            station("ra.1000000003", "Station A")]}}),
        ),
    ])
}

#[test]
fn radio_puts_recently_played_after_the_opening_rows() {
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", radio_page());
    t.fake.route(
        "GET /v1/me/recent/radio-stations",
        json!({"data": [station("ra.1000000008", "Recent A"), live_station("ra.1000000009", "Recent B")]}),
    );
    let shelves = t.api.radio().unwrap();
    let shown: Vec<(&str, ShelfLayout, Vec<&str>)> =
        shelves.iter().map(|shelf| (shelf.title.as_str(), shelf.layout, titles(&shelf.items))).collect();
    assert_eq!(
        shown,
        [
            ("", ShelfLayout::Hero, vec!["Radio A"]),
            ("Live Radio Now", ShelfLayout::Cards, vec!["Radio B"]),
            ("Recently Played", ShelfLayout::Cards, vec!["Recent A", "Recent B"]),
            ("Stations by Genre", ShelfLayout::Cards, vec!["Station A"]),
        ]
    );
    assert_eq!(t.fake.asked("GET /v1/editorial/us/groupings")[0].query["name"], "radio");
    assert_eq!(t.fake.asked("GET /v1/me/recent/radio-stations")[0].query["limit"], "30");
}

#[test]
fn radio_shows_its_page_without_the_recent_stations() {
    // none played yet
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", radio_page());
    t.fake.route("GET /v1/me/recent/radio-stations", json!({"data": []}));
    assert_eq!(t.api.radio().unwrap().len(), 3);

    // Apple does not answer for them
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", radio_page());
    let titles: Vec<String> = t.api.radio().unwrap().into_iter().map(|shelf| shelf.title).collect();
    assert_eq!(titles, ["", "Live Radio Now", "Stations by Genre"]);

    // but a refused sign-in is not hidden
    let t = case();
    t.fake.route("GET /v1/editorial/us/groupings", radio_page());
    t.fake.route_with("GET /v1/me/recent/radio-stations", |_| {
        Err(WebError::Auth { message: "rejected".into(), status: Some(403) })
    });
    assert!(t.api.radio().unwrap_err().is_auth());
}

#[test]
fn one_station_by_its_id() {
    let t = case();
    t.fake.route("GET /v1/catalog/us/stations/ra.1000000001", json!({"data": [station("ra.1000000001", "Station A")]}));
    assert_eq!(t.api.station("ra.1000000001").unwrap().name, "Station A");
    assert!(matches!(t.api.station("ra.1000000002"), Err(ApiError::Web(WebError::NotFound(_)))));
    assert!(matches!(t.api.station("a/b"), Err(ApiError::Web(_))));
    assert!(t.fake.asked("GET /v1/catalog/us/stations/a/b").is_empty());
}

// --- one item of the library ----------------------------------------------------------------

#[test]
fn one_library_item_by_its_id() {
    let t = case();
    let catalog = album("1000000001", "Album A");
    t.fake.route(
        "GET /v1/me/library/albums/l.AbCdEf1",
        json!({"data": [{"id": "l.AbCdEf1", "type": "library-albums", "attributes": {
            "artistName": "Synth Artist", "artwork": art(), "name": "Album A", "trackCount": 3,
            "playParams": {"id": "l.AbCdEf1", "isLibrary": true, "kind": "album"}},
            "relationships": {"catalog": {"data": [catalog]}}}]}),
    );
    let found = t.api.library_album("l.AbCdEf1").unwrap();
    assert_eq!((found.name.as_str(), found.library_id.as_deref()), ("Album A", Some("l.AbCdEf1")));
    assert_eq!(found.catalog_id.as_deref(), Some("1000000001"));
    assert_eq!(t.fake.asked("GET /v1/me/library/albums/l.AbCdEf1")[0].query["include"], "catalog");

    t.fake.route(
        "GET /v1/me/library/playlists/p.AbCdEf2",
        json!({"data": [{"id": "p.AbCdEf2", "type": "library-playlists", "attributes": {
            "canEdit": true, "name": "Playlist A", "tags": ["favorited"],
            "playParams": {"id": "p.AbCdEf2", "isLibrary": true, "kind": "playlist"}}}]}),
    );
    let found = t.api.library_playlist("p.AbCdEf2").unwrap();
    assert_eq!((found.name.as_str(), found.library_id.as_deref()), ("Playlist A", Some("p.AbCdEf2")));
    assert_eq!(t.fake.asked("GET /v1/me/library/playlists/p.AbCdEf2")[0].query["extend[library-playlists]"], "tags");

    t.fake.route(
        "GET /v1/me/library/artists/r.AbCdEf3",
        json!({"data": [{"id": "r.AbCdEf3", "type": "library-artists", "attributes": {"name": "Synth Artist"}}]}),
    );
    let found = t.api.library_artist("r.AbCdEf3").unwrap();
    assert_eq!((found.name.as_str(), found.library_id.as_deref()), ("Synth Artist", Some("r.AbCdEf3")));

    // one that is not there, and an answer without the item
    assert!(matches!(t.api.library_album("l.Missing"), Err(ApiError::Web(WebError::NotFound(_)))));
    t.fake.route("GET /v1/me/library/albums/l.Empty", json!({"data": []}));
    assert!(t.api.library_album("l.Empty").is_err());
    t.fake.route("GET /v1/me/library/albums/l.Stub", json!({"data": [{"id": "l.Stub", "type": "library-albums"}]}));
    assert!(t.api.library_album("l.Stub").is_err());
}

// --- the language of Apple's own words ----------------------------------------------------

/// The `l` parameters of the requests for Home, New and the recent stations.
fn languages_asked(t: &Case) -> Vec<Option<String>> {
    t.fake.route("GET /v1/me/recommendations", json!({"data": []}));
    t.fake.route("GET /v1/editorial/us/groupings", json!({"data": []}));
    t.fake.route("GET /v1/me/recent/radio-stations", json!({"data": []}));
    t.api.home().unwrap();
    t.api.browse().unwrap();
    t.api.recent_stations(10).unwrap();
    ["GET /v1/me/recommendations", "GET /v1/editorial/us/groupings", "GET /v1/me/recent/radio-stations"]
        .into_iter()
        .map(|request| t.fake.asked(request)[0].query.get("l").cloned())
        .collect()
}

#[test]
fn apple_s_words_are_asked_for_in_the_desktop_s_language() {
    // the storefront has it
    let t = case_in("en_US.UTF-8");
    assert_eq!(languages_asked(&t), vec![Some("en-US".to_string()); 3]);
    // what the storefront offers is asked for once
    assert_eq!(t.fake.asked("GET /v1/storefronts/us").len(), 1);

    // the same language of another region is the next best
    let t = case_in("en_GB.UTF-8");
    assert_eq!(languages_asked(&t), vec![Some("en-US".to_string()); 3]);
    let t = case_in("es_ES.UTF-8");
    assert_eq!(languages_asked(&t), vec![Some("es-MX".to_string()); 3]);
    let t = case_in("en_US.UTF-8");
    t.fake.route("GET /v1/storefronts/us", storefront(&["es-MX", "en-GB"]));
    assert_eq!(languages_asked(&t), vec![Some("en-GB".to_string()); 3]);
}

#[test]
fn without_a_fitting_language_apple_answers_in_the_storefront_s_own() {
    // a language the storefront does not have
    let t = case_in("de_DE.UTF-8");
    assert_eq!(languages_asked(&t), vec![None; 3]);
    // a desktop that names no language asks nothing, not even what the storefront offers
    for locale in ["", "C.UTF-8", "POSIX"] {
        let t = case_in(locale);
        assert_eq!(languages_asked(&t), vec![None; 3], "{locale}");
        assert!(t.fake.asked("GET /v1/storefronts/us").is_empty());
    }
    // a storefront that cannot be asked, or does not say
    let t = case();
    t.fake.route_with("GET /v1/storefronts/us", |_| Err(WebError::Other("no answer".into())));
    assert_eq!(languages_asked(&t), vec![None; 3]);
    let t = case();
    t.fake.route("GET /v1/storefronts/us", json!({"data": [{"id": "us", "type": "storefronts", "attributes": {}}]}));
    assert_eq!(languages_asked(&t), vec![None; 3]);
}

#[test]
fn the_album_and_artist_of_a_library_song() {
    let t = case();
    t.fake.route(
        "GET /v1/me/library/songs/i.AbCdEf4",
        json!({"data": [{"id": "i.AbCdEf4", "type": "library-songs", "attributes": {"name": "Song A"},
            "relationships": {
                "albums": {"data": [{"id": "l.AbCdEf1", "type": "library-albums"}]},
                "artists": {"data": [{"id": "1000000005", "type": "artists"}, {"id": "r.AbCdEf6", "type": "library-artists"}]}}}]}),
    );
    let links = t.api.library_song_links("i.AbCdEf4").unwrap();
    assert_eq!(links, (Some("l.AbCdEf1".to_string()), Some("r.AbCdEf6".to_string())));
    assert_eq!(t.fake.asked("GET /v1/me/library/songs/i.AbCdEf4")[0].query["include"], "albums,artists");

    // a song whose album and artist the library does not name
    t.fake.route(
        "GET /v1/me/library/songs/i.AbCdEf5",
        json!({"data": [{"id": "i.AbCdEf5", "type": "library-songs", "attributes": {"name": "Upload"}}]}),
    );
    assert_eq!(t.api.library_song_links("i.AbCdEf5").unwrap(), (None, None));
    assert!(matches!(t.api.library_song_links("i.Missing"), Err(ApiError::Web(WebError::NotFound(_)))));
    assert!(t.api.library_song_links("a/b").is_err());
}

// --- the songs of a station -------------------------------------------------------------

#[test]
fn the_next_songs_of_a_station_are_asked_for_every_time() {
    let t = case();
    let batch = Arc::new(Mutex::new(0));
    let counter = batch.clone();
    t.fake.route_with("POST /v1/me/stations/next-tracks/ra.1000000001", move |_| {
        let mut batch = counter.lock().unwrap();
        *batch += 1;
        let first = song(&format!("10000000{}1", *batch), &format!("Song {}a", *batch), "Artist", "Album", 1000, None);
        let stub = json!({"id": "1000000099", "type": "songs"});
        let second = song(&format!("10000000{}2", *batch), &format!("Song {}b", *batch), "Artist", "Album", 1000, None);
        Ok(json!({"data": [first, stub, second]}))
    });
    let names = |tracks: Vec<Track>| tracks.into_iter().map(|track| track.name).collect::<Vec<_>>();
    assert_eq!(names(t.api.station_next_tracks("ra.1000000001", None).unwrap()), ["Song 1a", "Song 1b"]);
    assert_eq!(names(t.api.station_next_tracks("ra.1000000001", Some(3)).unwrap()), ["Song 2a", "Song 2b"]);
    assert_eq!(names(t.api.station_next_tracks("ra.1000000001", Some(500)).unwrap()), ["Song 3a", "Song 3b"]);

    let asked = t.fake.asked("POST /v1/me/stations/next-tracks/ra.1000000001");
    assert_eq!(asked.len(), 3);
    assert!(asked.iter().all(|asked| asked.auth && asked.body.is_none()));
    assert!(!asked[0].query.contains_key("limit"));
    assert_eq!(asked[1].query["limit"], "3");
    assert_eq!(asked[2].query["limit"], "25");
    assert!(t.api.station_next_tracks("ra.1000000001", None).unwrap().iter().all(Track::playable));
}

#[test]
fn a_station_that_has_run_out_gives_no_songs() {
    let t = case();
    t.fake.route("POST /v1/me/stations/next-tracks/ra.1000000001", Value::Null);
    assert!(t.api.station_next_tracks("ra.1000000001", Some(1)).unwrap().is_empty());
    t.fake.route("POST /v1/me/stations/next-tracks/ra.1000000002", json!({"data": []}));
    assert!(t.api.station_next_tracks("ra.1000000002", Some(1)).unwrap().is_empty());
    // an unknown station is an error, as is an id that is none
    assert!(matches!(t.api.station_next_tracks("ra.1000000003", None), Err(ApiError::Web(WebError::NotFound(_)))));
    assert!(t.api.station_next_tracks(" ", None).is_err());
}

// --- the station that continues a queue ---------------------------------------------------

#[test]
fn a_continuous_station_starts_from_the_last_songs() {
    let t = case();
    let made = json!({"id": "ra.cp-AbCdEf", "type": "stations", "attributes": {
        "isLive": false, "kind": "songSeeded", "name": "Song A Station",
        "playParams": {"format": "tracks", "id": "ra.cp-AbCdEf", "kind": "radioStation"}}});
    t.fake.route("POST /v1/me/stations/continuous", json!({"results": {"station": made}}));

    let seeds: Vec<Seed> = (1..=12)
        .map(|n| {
            let track = Track::new(Some(&format!("10000000{n:02}")), format!("Song {n}"), "Artist");
            Seed::of(&track).unwrap()
        })
        .collect();
    let station = t.api.continuous_station(&seeds).unwrap();
    assert_eq!((station.id.as_str(), station.kind.as_str()), ("ra.cp-AbCdEf", "songSeeded"));
    assert!(station.has_tracks());

    let asked = t.fake.asked("POST /v1/me/stations/continuous");
    assert_eq!(asked.len(), 1);
    let sent = asked[0].body.as_ref().unwrap()["data"].as_array().unwrap().clone();
    let ids: Vec<&str> = sent.iter().map(|entry| entry["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "1000000003",
            "1000000004",
            "1000000005",
            "1000000006",
            "1000000007",
            "1000000008",
            "1000000009",
            "1000000010",
            "1000000011",
            "1000000012"
        ]
    );
    assert!(sent.iter().all(|entry| entry["type"] == "songs" && entry.get("meta").is_none()));
    assert!(asked[0].auth && asked[0].query.is_empty());
}

#[test]
fn seeds_name_the_song_and_what_it_was_played_from() {
    let catalog = Track::new(Some("1000000001"), "Song A", "Artist");
    let library = Track { library_id: Some("i.AbCdEf1".into()), ..Track::new(None, "Song B", "Artist") };
    let both = Track { library_id: Some("i.AbCdEf2".into()), ..Track::new(Some("1000000002"), "Song C", "Artist") };
    assert!(Seed::of(&Track::new(None, "No id", "Artist")).is_none());
    assert_eq!(Seed::of(&both).unwrap().kind, "songs");

    let t = case();
    let made = json!({"id": "ra.cp-AbCdEf", "type": "stations", "attributes": {"name": "S", "playParams": {"format": "tracks"}}});
    // Apple also answers with a list holding the station
    t.fake.route("POST /v1/me/stations/continuous", json!({"results": {"station": [made]}}));
    let seeds = [
        Seed::of(&catalog).unwrap().within("albums", "1000000009"),
        Seed::of(&library).unwrap().within("library-playlists", "p.AbCdEf3"),
    ];
    t.api.continuous_station(&seeds).unwrap();
    let asked = t.fake.asked("POST /v1/me/stations/continuous");
    assert_eq!(
        asked[0].body.as_ref().unwrap()["data"],
        json!([
            {"id": "1000000001", "type": "songs", "meta": {"container": {"id": "1000000009", "type": "albums"}}},
            {"id": "i.AbCdEf1", "type": "library-songs", "meta": {"container": {"id": "p.AbCdEf3", "type": "library-playlists"}}},
        ])
    );
}

#[test]
fn no_station_without_songs_or_without_an_answer() {
    let t = case();
    assert!(matches!(t.api.continuous_station(&[]), Err(ApiError::Value(_))));
    assert!(t.fake.asked("POST /v1/me/stations/continuous").is_empty());

    let seed = Seed::of(&Track::new(Some("1000000001"), "Song A", "Artist")).unwrap();
    t.fake.route("POST /v1/me/stations/continuous", json!({"results": {}}));
    assert!(matches!(t.api.continuous_station(std::slice::from_ref(&seed)), Err(ApiError::Web(_))));
    t.fake.route("POST /v1/me/stations/continuous", Value::Null);
    assert!(matches!(t.api.continuous_station(std::slice::from_ref(&seed)), Err(ApiError::Web(_))));

    // an id that is none is not sent
    let odd = Seed { id: "a b".into(), kind: "songs", container: None };
    assert!(t.api.continuous_station(&[odd]).is_err());
    assert_eq!(t.fake.asked("POST /v1/me/stations/continuous").len(), 2);
}

// --- the current version of a song --------------------------------------------------------

fn withdrawn() -> Track {
    Track {
        album: "Album A".into(),
        library_id: Some("i.AbCdEf4".into()),
        duration_ms: Some(201_000),
        content_rating: Some("explicit".into()),
        ..Track::new(Some("1000000002"), "Song A (feat. Guest B)", "Synth Artist")
    }
}

fn search(songs: Vec<Value>) -> Value {
    json!({"results": {"songs": {"data": songs}}})
}

#[test]
fn the_equivalent_song_is_the_same_recording() {
    let t = case();
    t.fake.route(
        "GET /v1/catalog/us/search",
        search(vec![
            song("1000000101", "Song A (Live)", "Synth Artist", "Album A", 201_000, Some("explicit")),
            song("1000000102", "Song A (feat. Guest B)", "Another Artist", "Album A", 201_000, Some("explicit")),
            song("1000000103", "Song A (feat. Guest B)", "Synth Artist", "Album A", 207_000, Some("explicit")),
            song("1000000104", "Song A (feat. Guest B)", "Synth Artist", "A Compilation", 201_000, Some("explicit")),
            song("1000000105", "Song A (feat. Guest B)", "Synth Artist", "Album A", 201_300, Some("clean")),
            song("1000000106", "song a (feat. guest b)", "SYNTH ARTIST", "Album A", 201_400, Some("explicit")),
            song("1000000107", "Song A (feat. Guest B)", "Synth Artist", "Album A", 201_900, Some("explicit")),
        ]),
    );
    let found = t.api.equivalent_song(&withdrawn()).unwrap().unwrap();
    // same album, same rating, closest in length; spelling and case do not matter
    assert_eq!(found.catalog_id.as_deref(), Some("1000000106"));
    let asked = t.fake.asked("GET /v1/catalog/us/search");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].query["term"], "Song A (feat. Guest B) Synth Artist");
    assert_eq!(asked[0].query["limit"], "25");
}

#[test]
fn the_equivalent_song_prefers_the_album_then_the_rating() {
    let t = case();
    t.fake.route(
        "GET /v1/catalog/us/search",
        search(vec![
            song("1000000104", "Song A (feat. Guest B)", "Synth Artist", "A Compilation", 201_000, Some("explicit")),
            song("1000000105", "Song A (feat. Guest B)", "Synth Artist", "Album A", 201_300, Some("clean")),
        ]),
    );
    assert_eq!(t.api.equivalent_song(&withdrawn()).unwrap().unwrap().catalog_id.as_deref(), Some("1000000105"));

    // a length that is not known does not rule a song out
    let t = case();
    let mut unknown = song("1000000108", "Song A (feat. Guest B)", "Synth Artist", "Album A", 1, None);
    unknown["attributes"].as_object_mut().unwrap().remove("durationInMillis");
    t.fake.route("GET /v1/catalog/us/search", search(vec![unknown]));
    assert_eq!(t.api.equivalent_song(&withdrawn()).unwrap().unwrap().catalog_id.as_deref(), Some("1000000108"));
}

#[test]
fn no_equivalent_song() {
    // the catalog only has the version that was asked about, and other songs
    let t = case();
    t.fake.route(
        "GET /v1/catalog/us/search",
        search(vec![
            song("1000000002", "Song A (feat. Guest B)", "Synth Artist", "Album A", 201_000, Some("explicit")),
            song("1000000109", "Another Song", "Synth Artist", "Album A", 201_000, Some("explicit")),
        ]),
    );
    assert!(t.api.equivalent_song(&withdrawn()).unwrap().is_none());

    // nothing to search for: no request
    let t = case();
    assert!(t.api.equivalent_song(&Track::new(Some("1000000002"), "", "Synth Artist")).unwrap().is_none());
    assert!(t.api.equivalent_song(&Track::new(Some("1000000002"), "Song A", " ")).unwrap().is_none());
    assert!(t.fake.asked("GET /v1/catalog/us/search").is_empty());

    // Apple's refusal comes through
    let t = case();
    t.fake.route_with("GET /v1/catalog/us/search", |_| {
        Err(WebError::RateLimited { message: "slow down".into(), retry_after: None })
    });
    assert!(matches!(t.api.equivalent_song(&withdrawn()), Err(ApiError::Web(WebError::RateLimited { .. }))));
}
