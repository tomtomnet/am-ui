//! Checks against Apple's service with the sign-in of this machine. They are ignored by default
//! and run by hand:
//!
//!     cargo test -p amui --test live -- --ignored --nocapture
//!
//! They only read, and they print the shape of what comes back (kinds and counts), never titles,
//! names or ids.

use std::collections::BTreeMap;
use std::sync::Arc;

use amui::api::MusicApi;
use amui::config::Config;
use amui::downmix::Downmix;
use amui::types::{Item, Shelf};
use amui::web::Web;

fn client() -> MusicApi {
    let config = Arc::new(Config::from_process());
    let web = Arc::new(Web::new(config.clone()));
    MusicApi::new(config, web)
}

fn describe(page: &str, shelves: &[Shelf]) {
    println!("{page}: {} shelves", shelves.len());
    for shelf in shelves {
        let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
        for item in &shelf.items {
            *kinds.entry(item.kind()).or_default() += 1;
        }
        let stations = shelf.items.iter().filter_map(|item| match item {
            Item::Station(station) => Some(station),
            _ => None,
        });
        let (mut songs, mut live) = (0, 0);
        for station in stations {
            songs += usize::from(station.has_tracks());
            live += usize::from(station.live);
        }
        let artwork = shelf.items.iter().filter(|item| item.artwork().is_some()).count();
        let playable = shelf.items.iter().filter(|item| matches!(item, Item::Track(track) if track.playable())).count();
        println!(
            "  {:?} title: {} chars, id: {}, items: {kinds:?}, with artwork: {artwork}, notes: {} ({} filled), \
             stations of songs: {songs}, live: {live}, playable songs: {playable}",
            shelf.layout,
            shelf.title.chars().count(),
            shelf.id.is_some(),
            shelf.notes.len(),
            shelf.notes.iter().filter(|note| !note.is_empty()).count(),
        );
    }
}

#[test]
#[ignore = "asks Apple's service with this machine's sign-in"]
fn shelves_of_home_new_and_radio() {
    let api = client();
    let home = api.home().expect("Home");
    describe("Home", &home);
    assert!(!home.is_empty(), "Home has shelves");
    let new = api.browse().expect("New");
    describe("New", &new);
    assert!(!new.is_empty(), "New has shelves");
    let radio = api.radio().expect("Radio");
    describe("Radio", &radio);
    assert!(!radio.is_empty(), "Radio has shelves");
}

/// Reads the downmix sink of this desktop session (`pw-cli ls Node`, `pw-cli enum-params`):
/// nothing is set. Prints how many controls the sink has and whether they are the expected ones.
#[test]
#[ignore = "looks at the PipeWire sink of this desktop session"]
fn the_sink_s_controls_can_be_read() {
    let config = Arc::new(Config::from_process());
    let downmix = Downmix::new(config);
    println!("sink running: {}", downmix.sink_running());
    let state = downmix.current();
    println!(
        "saved state: a preset is named: {}, source: {:?}, match: {}",
        !state.preset.is_empty(),
        state.source,
        state.match_on
    );
    let live = downmix.live_controls().expect("the sink's controls");
    let names: Vec<&str> = live.iter().map(|(name, _)| name).collect();
    let matrix = names.iter().filter(|name| !name.starts_with("eq")).count();
    let eq = names.len() - matrix;
    println!("live controls: {} ({matrix} of the matrix, {eq} of the equalizer)", names.len());
    assert!(names.len() > 16, "a 9.1.6 sink has more controls than channels");
    let expected = amui::downmix::expected_controls(&state.preset, state.preamp);
    match expected {
        Ok(expected) => {
            let known = expected.iter().filter(|(name, _)| live.get(name).is_some()).count();
            let equal = expected
                .iter()
                .filter(|(name, value)| live.get(name).is_some_and(|live| (live - value).abs() < 1e-3))
                .count();
            println!(
                "expected for the saved state: {} controls, {known} of them in the sink, {equal} with the expected value",
                expected.len()
            );
        }
        Err(error) => println!("no expectation for the saved state: {error}"),
    }
    // the equalizer stage: what the sink has, and whether it is what this application's state says
    let equalizer = amui::eq::Equalizer::new(Arc::new(downmix));
    let stage = equalizer.live_state().expect("the sink's equalizer stage");
    println!(
        "equalizer stage: gain {:+.2} dB, {} bands; as this application's state wants it: {}",
        stage.gain_db.unwrap_or(0.0),
        stage.bands,
        equalizer.sink_as_wanted().expect("a comparison"),
    );
}

/// How long the account's Favorite Songs playlist takes: its first hundred songs, then the rest
/// six pages at a time, as the application loads a long playlist. Prints counts and seconds.
#[test]
#[ignore = "asks Apple's service with this machine's sign-in"]
fn a_long_playlist_arrives_in_parallel_pages() {
    use std::time::Instant;
    let api = client();
    let playlists = api.library_playlists(0, 100).expect("the playlists");
    let Some(favorites) = playlists.items.iter().find(|playlist| playlist.is_favorites) else {
        println!("this library has no Favorite Songs playlist");
        return;
    };
    let id = favorites.library_id.clone().expect("a library id");
    let started = Instant::now();
    let first = api.library_playlist_tracks(&id, 0, 100).expect("the first page");
    let first_s = started.elapsed().as_secs_f64();
    let total = first.total.unwrap_or(first.items.len() as i64);
    println!("first page: {} songs of {total} in {first_s:.2} s", first.items.len());
    let offsets: Vec<i64> = (1..).map(|page| page * 100).take_while(|offset| *offset < total).collect();
    let started = Instant::now();
    let mut loaded = first.items.len();
    for batch in offsets.chunks(6) {
        let pages: Vec<usize> = std::thread::scope(|scope| {
            let requests: Vec<_> = batch
                .iter()
                .map(|offset| {
                    scope.spawn(|| api.library_playlist_tracks(&id, *offset, 100).map(|page| page.items.len()))
                })
                .collect();
            requests.into_iter().map(|request| request.join().unwrap().expect("a page")).collect()
        });
        loaded += pages.iter().sum::<usize>();
    }
    println!(
        "the other {} pages, six at a time: {:.2} s; {loaded} songs in all",
        offsets.len(),
        started.elapsed().as_secs_f64()
    );
    assert_eq!(loaded as i64, total, "every song arrived");
}

/// One album, playlist and artist of the library by their ids, as the pages of the application
/// ask for them. Prints which details came back, not what they say.
#[test]
#[ignore = "asks Apple's service with this machine's sign-in"]
fn single_library_items_come_with_what_their_pages_show() {
    let api = client();
    let albums = api.library_albums(0, 5, None, false).expect("albums");
    if let Some(id) = albums.items.first().and_then(|album| album.library_id.clone()) {
        let album = api.library_album(&id).expect("the album");
        let tracks = api.library_album_tracks(&id).expect("its songs");
        println!(
            "album: artwork {}, artist {}, genre {}, release date {}, catalog id {}, audio traits {}; songs: {}, \
             with track numbers {}, playable {}",
            album.artwork.is_some(),
            !album.artist.is_empty(),
            album.genre.is_some(),
            album.release_date.is_some(),
            album.catalog_id.is_some(),
            album.audio_traits.len(),
            tracks.len(),
            tracks.iter().filter(|track| track.track_number.is_some()).count(),
            tracks.iter().filter(|track| track.playable()).count(),
        );
        assert_eq!(album.library_id.as_deref(), Some(id.as_str()));
        assert!(!tracks.is_empty(), "an album of the library has songs");
    }
    let songs = api.library_songs(0, 5, false).expect("songs");
    if let Some(id) = songs.items.first().and_then(|song| song.library_id.clone()) {
        let listed = &songs.items[0];
        let (album, artist) = api.library_song_links(&id).expect("the song's album and artist");
        println!(
            "library song: listed with album ids {} and artist id {}; looked up: album {}, artist {}",
            listed.album_catalog_id.is_some() || listed.album_library_id.is_some(),
            listed.artist_catalog_id.is_some(),
            album.is_some(),
            artist.is_some(),
        );
        if let Some(catalog_id) = &listed.catalog_id {
            let song = api.catalog_song(catalog_id).expect("its catalog version");
            println!(
                "its catalog version: album id {}, artist id {}",
                song.album_catalog_id.is_some(),
                song.artist_catalog_id.is_some()
            );
        }
    }
    let playlists = api.library_playlists(0, 5).expect("playlists");
    if let Some(id) = playlists.items.first().and_then(|playlist| playlist.library_id.clone()) {
        let playlist = api.library_playlist(&id).expect("the playlist");
        let page = api.library_playlist_tracks(&id, 0, 100).expect("its first songs");
        println!(
            "playlist: artwork {}, can be edited {}, favourites {}; first page: {} songs, total known {}, more {}",
            playlist.artwork.is_some(),
            playlist.can_edit,
            playlist.is_favorites,
            page.items.len(),
            page.total.is_some(),
            page.next_offset.is_some(),
        );
        assert_eq!(playlist.library_id.as_deref(), Some(id.as_str()));
    }
    let artists = api.library_artists(0, 5, true).expect("artists");
    if let Some(id) = artists.items.first().and_then(|artist| artist.library_id.clone()) {
        let artist = api.library_artist(&id).expect("the artist");
        let albums = api.library_artist_albums(&id).expect("their albums");
        println!(
            "artist: artwork {}, catalog id {}, genre {}; albums in the library: {}",
            artist.artwork.is_some(),
            artist.catalog_id.is_some(),
            artist.genre.is_some(),
            albums.len(),
        );
        assert_eq!(artist.library_id.as_deref(), Some(id.as_str()));
    }
}
