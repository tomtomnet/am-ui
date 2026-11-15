//! The library as the pages show it: albums, artists, songs, playlists and what was added
//! recently, loaded once and kept in list models.
//!
//! A list is fetched in pages of 100. The first page says how many items there are; the rest
//! are fetched several at a time and added in order as they arrive (those that arrive while the
//! window is busy, together).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use amui::types::{Page, Track};
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::data::{Item, Music, Problem, Res};
use crate::signal::{Signal, background};

pub const PAGE: i64 = 100;
/// Pages of one list fetched at the same time.
const PARALLEL: usize = 6;
/// Rows that have arrived put into a list's store at once, at most (ten pages).
const BATCH: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Albums,
    Artists,
    Songs,
    Playlists,
    Recent,
}

/// The value a row of a list model carries.
pub fn boxed<T: 'static>(value: T) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(value)
}

/// The item of a card row.
pub fn item_of(object: &glib::Object) -> Option<Item> {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .and_then(|boxed| boxed.try_borrow::<Item>().ok().map(|item| item.clone()))
}

/// The song of a song row.
pub fn track_of(object: &glib::Object) -> Option<Track> {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .and_then(|boxed| boxed.try_borrow::<Track>().ok().map(|track| track.clone()))
}

/// How a song of the library's list is the song that another list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Same {
    /// The same library id: the library's own entry of it.
    Entry,
    /// The same catalog id: a song of the catalog (an album's page, a search, a shelf) that the
    /// library has.
    Catalog,
}

/// Whether `listed`, a song of the library's list, is `wanted`, a song of any list.
pub fn same_song(listed: &Track, wanted: &Track) -> Option<Same> {
    let equal = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
    if equal(&listed.library_id, &wanted.library_id) {
        Some(Same::Entry)
    } else if equal(&listed.catalog_id, &wanted.catalog_id) {
        Some(Same::Catalog)
    } else {
        None
    }
}

/// Where the library's copy of `wanted` is in `model` (rows carrying `Track`, the library's
/// songs in some order): the song with its library id, or else the first with its catalog id.
pub fn find_song(model: &impl IsA<gio::ListModel>, wanted: &Track) -> Option<u32> {
    let mut by_catalog = None;
    for position in 0..model.n_items() {
        let Some(row) = model.item(position) else { continue };
        let Some(listed) = row.downcast_ref::<glib::BoxedAnyObject>() else { continue };
        let Ok(listed) = listed.try_borrow::<Track>() else { continue };
        match same_song(&listed, wanted) {
            Some(Same::Entry) => return Some(position),
            Some(Same::Catalog) if by_catalog.is_none() => by_catalog = Some(position),
            _ => {}
        }
    }
    by_catalog
}

type Fetch<T> = Arc<dyn Fn(i64, i64) -> Res<Page<T>> + Send + Sync>;

/// Fetch every page of a list. `on_items(offset, items)` is called on the user interface
/// thread with consecutive pages, in order; `on_done` once at the end with the first problem.
pub fn fetch_all<T: Send + 'static>(
    fetch: Fetch<T>,
    on_items: impl Fn(i64, Vec<T>) + 'static,
    on_done: impl FnOnce(Result<(), Problem>) + 'static,
) {
    let first = fetch.clone();
    background(
        move || first(0, PAGE),
        move |result| {
            let page = match result {
                Ok(page) => page,
                Err(problem) => return on_done(Err(problem)),
            };
            let total = page.total;
            let next = page.next_offset;
            on_items(0, page.items);
            let Some(next) = next else { return on_done(Ok(())) };
            match total {
                Some(total) if total > next => fetch_rest(fetch, next, total, on_items, on_done),
                _ => fetch_chain(fetch, next, Rc::new(on_items), on_done),
            }
        },
    );
}

/// The service did not say how many items there are: one page after the other.
fn fetch_chain<T: Send + 'static>(
    fetch: Fetch<T>,
    offset: i64,
    on_items: Rc<dyn Fn(i64, Vec<T>)>,
    on_done: impl FnOnce(Result<(), Problem>) + 'static,
) {
    let job = fetch.clone();
    background(
        move || job(offset, PAGE),
        move |result| match result {
            Ok(page) => {
                let next = page.next_offset;
                on_items(offset, page.items);
                match next {
                    Some(next) if next > offset => fetch_chain(fetch, next, on_items, on_done),
                    _ => on_done(Ok(())),
                }
            }
            Err(problem) => on_done(Err(problem)),
        },
    );
}

/// The pages from `start` up to `total`, several at a time, delivered in order.
fn fetch_rest<T: Send + 'static>(
    fetch: Fetch<T>,
    start: i64,
    total: i64,
    on_items: impl Fn(i64, Vec<T>) + 'static,
    on_done: impl FnOnce(Result<(), Problem>) + 'static,
) {
    let offsets: Vec<i64> = (start..total).step_by(PAGE as usize).collect();
    let (sender, receiver) = async_channel::unbounded::<(i64, Res<Page<T>>)>();
    let queue = Arc::new(std::sync::Mutex::new(offsets.clone().into_iter().rev().collect::<Vec<_>>()));
    for _ in 0..PARALLEL.min(offsets.len()) {
        let (fetch, queue, sender) = (fetch.clone(), queue.clone(), sender.clone());
        let _ = std::thread::Builder::new().name("am-ui-pages".into()).spawn(move || {
            loop {
                let Some(offset) = queue.lock().unwrap().pop() else { break };
                if sender.send_blocking((offset, fetch(offset, PAGE))).is_err() {
                    break;
                }
            }
        });
    }
    drop(sender);
    glib::spawn_future_local(async move {
        let mut waiting: BTreeMap<i64, Vec<T>> = BTreeMap::new();
        let mut expected = start;
        let mut problem = None;
        while let Ok((offset, result)) = receiver.recv().await {
            match result {
                Ok(page) => {
                    waiting.insert(offset, page.items);
                }
                Err(error) => {
                    problem.get_or_insert(error);
                }
            }
            while let Some(items) = waiting.remove(&expected) {
                on_items(expected, items);
                expected += PAGE;
            }
        }
        on_done(problem.map_or(Ok(()), Err));
    });
}

/// Where the lists of the library are kept between runs (`var/state/library`, 0600 files): what
/// was loaded last shows at once when the application starts, until the fresh lists are there.
#[derive(Debug, Clone)]
pub struct Snapshots {
    dir: std::path::PathBuf,
}

impl Snapshots {
    pub fn new(dir: std::path::PathBuf) -> Snapshots {
        Snapshots { dir }
    }

    fn file(&self, kind: Kind) -> std::path::PathBuf {
        let name = match kind {
            Kind::Albums => "albums",
            Kind::Artists => "artists",
            Kind::Songs => "songs",
            Kind::Playlists => "playlists",
            Kind::Recent => "recently-added",
        };
        self.dir.join(format!("{name}.json"))
    }

    /// The rows of a list as they were saved; `None` when there is no copy or it cannot be read.
    fn read(&self, kind: Kind) -> Option<Rows> {
        let data = std::fs::read(self.file(kind)).ok()?;
        match kind {
            Kind::Songs => serde_json::from_slice::<Vec<Track>>(&data).ok().map(Rows::Songs),
            _ => serde_json::from_slice::<Vec<Item>>(&data).ok().map(Rows::Items),
        }
    }

    /// Save the rows of a list (on a worker thread). A copy that cannot be written is no loss.
    fn write(&self, kind: Kind, rows: Rows) {
        let path = self.file(kind);
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            let data = match &rows {
                Rows::Songs(tracks) => serde_json::to_vec(tracks),
                Rows::Items(items) => serde_json::to_vec(items),
            };
            if let Ok(data) = data
                && amui::util::create_private_dirs(&dir).is_ok()
            {
                let _ = amui::util::write_private_atomic(&path, &data);
            }
        });
    }
}

/// The rows of a list, as they are saved.
enum Rows {
    Songs(Vec<Track>),
    Items(Vec<Item>),
}

impl Rows {
    fn boxed(self) -> Vec<glib::BoxedAnyObject> {
        match self {
            Rows::Songs(tracks) => tracks.into_iter().map(boxed).collect(),
            Rows::Items(items) => items.into_iter().map(boxed).collect(),
        }
    }

    /// The rows of a store.
    fn of(kind: Kind, store: &gio::ListStore) -> Rows {
        let objects = (0..store.n_items()).filter_map(|at| store.item(at));
        match kind {
            Kind::Songs => Rows::Songs(objects.filter_map(|object| track_of(&object)).collect()),
            _ => Rows::Items(objects.filter_map(|object| item_of(&object)).collect()),
        }
    }
}

pub struct Library {
    pub albums: gio::ListStore,
    pub artists: gio::ListStore,
    pub songs: gio::ListStore,
    pub playlists: gio::ListStore,
    pub recent: gio::ListStore,
    loading: RefCell<HashSet<Kind>>,
    loaded: RefCell<HashSet<Kind>>,
    started: Cell<bool>,
    /// A list finished loading, started loading or failed.
    pub changed: Signal<Kind>,
    pub problem: RefCell<Option<Problem>>,
    /// Where the lists are kept between runs; `None`: nowhere (the demonstration).
    snapshots: Option<Snapshots>,
    /// The lists whose store shows the copy of the last run.
    from_snapshot: RefCell<HashSet<Kind>>,
}

impl Default for Library {
    fn default() -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>;
        Library {
            albums: store(),
            artists: store(),
            songs: store(),
            playlists: store(),
            recent: store(),
            loading: RefCell::default(),
            loaded: RefCell::default(),
            started: Cell::new(false),
            changed: Signal::new(),
            problem: RefCell::default(),
            snapshots: None,
            from_snapshot: RefCell::default(),
        }
    }
}

impl Library {
    pub fn store(&self, kind: Kind) -> &gio::ListStore {
        match kind {
            Kind::Albums => &self.albums,
            Kind::Artists => &self.artists,
            Kind::Songs => &self.songs,
            Kind::Playlists => &self.playlists,
            Kind::Recent => &self.recent,
        }
    }

    pub fn is_loading(&self, kind: Kind) -> bool {
        self.loading.borrow().contains(&kind)
    }

    pub fn is_loaded(&self, kind: Kind) -> bool {
        self.loaded.borrow().contains(&kind)
    }

    /// Whether the library's songs have `track` (by its library id or its catalog id).
    pub fn has_song(&self, track: &Track) -> bool {
        find_song(&self.songs, track).is_some()
    }

    /// A library that keeps its lists between runs.
    pub fn with_snapshots(snapshots: Snapshots) -> Library {
        Library { snapshots: Some(snapshots), ..Library::default() }
    }

    /// Load every list (once; `reload` starts over). The copies of the last run show first.
    pub fn load(self: &Rc<Self>, music: &Arc<dyn Music>) {
        if self.started.replace(true) {
            return;
        }
        if let Some(snapshots) = self.snapshots.clone() {
            let kinds = [Kind::Albums, Kind::Artists, Kind::Playlists, Kind::Recent, Kind::Songs];
            for kind in kinds {
                let (library, snapshots) = (self.clone(), snapshots.clone());
                background(
                    move || snapshots.read(kind),
                    move |rows| {
                        // only into a list that nothing else has filled yet
                        let store = library.store(kind);
                        if let Some(rows) = rows
                            && store.n_items() == 0
                            && !library.is_loaded(kind)
                        {
                            library.from_snapshot.borrow_mut().insert(kind);
                            store.splice(0, 0, &rows.boxed());
                            library.changed.emit(&kind);
                        }
                    },
                );
            }
        }
        self.reload(music);
    }

    pub fn reload(self: &Rc<Self>, music: &Arc<dyn Music>) {
        self.problem.replace(None);
        for kind in [Kind::Albums, Kind::Artists, Kind::Playlists, Kind::Recent, Kind::Songs] {
            self.reload_kind(music, kind);
        }
    }

    /// Load one list again (after a change to it).
    pub fn reload_kind(self: &Rc<Self>, music: &Arc<dyn Music>, kind: Kind) {
        let m = music.clone();
        match kind {
            Kind::Albums => self.fill(
                kind,
                Arc::new(move |offset, limit| m.library_albums(offset, limit).map(|page| map(page, Item::Album))),
                boxed,
            ),
            Kind::Artists => self.fill(
                kind,
                Arc::new(move |offset, limit| m.library_artists(offset, limit).map(|page| map(page, Item::Artist))),
                boxed,
            ),
            Kind::Playlists => self.fill(
                kind,
                Arc::new(move |offset, limit| m.library_playlists(offset, limit).map(|page| map(page, Item::Playlist))),
                boxed,
            ),
            Kind::Recent => {
                self.fill(kind, Arc::new(move |offset, limit| m.library_recently_added(offset, limit)), boxed)
            }
            Kind::Songs => self.fill(kind, Arc::new(move |offset, limit| m.library_songs(offset, limit)), boxed),
        }
    }

    fn fill<T: Send + 'static>(self: &Rc<Self>, kind: Kind, fetch: Fetch<T>, wrap: fn(T) -> glib::BoxedAnyObject) {
        if !self.loading.borrow_mut().insert(kind) {
            return;
        }
        self.changed.emit(&kind);
        let arrivals = Rc::new(Arrivals::new(kind));
        let (library, done, arrived) = (self.clone(), self.clone(), arrivals.clone());
        fetch_all(
            fetch,
            move |_offset, items| {
                let rows = items.into_iter().map(wrap).collect();
                // a list that shows the last run's copy keeps it until the fresh one is complete
                if library.from_snapshot.borrow().contains(&kind) {
                    arrived.hold(rows);
                } else {
                    arrived.add(&library, rows);
                }
            },
            move |result| {
                let held = done.from_snapshot.borrow().contains(&kind);
                // what has arrived is in the store before the list counts as loaded; a copy of
                // the last run stays when the fresh list could not be had
                if !held || result.is_ok() {
                    arrivals.store(&done, usize::MAX);
                    done.from_snapshot.borrow_mut().remove(&kind);
                }
                done.loading.borrow_mut().remove(&kind);
                match result {
                    Ok(()) => {
                        done.loaded.borrow_mut().insert(kind);
                        if let Some(snapshots) = &done.snapshots {
                            snapshots.write(kind, Rows::of(kind, done.store(kind)));
                        }
                    }
                    Err(problem) => {
                        done.problem.replace(Some(problem));
                    }
                }
                done.changed.emit(&kind);
            },
        );
    }
}

/// The rows of a list that have arrived and are not in its store yet. The pages that arrive
/// while the window is busy go into the store together, `BATCH` rows at most with frames in
/// between: a list that is sorted or shown changes once for them, not once a page.
struct Arrivals {
    kind: Kind,
    rows: RefCell<Vec<glib::BoxedAnyObject>>,
    /// The first page is there. A reload replaces the rows of the store only then: no empty
    /// flash before it.
    started: Cell<bool>,
    /// The rows of the store were replaced; what follows is added to them.
    replaced: Cell<bool>,
    /// An idle callback is going to store what is waiting.
    scheduled: Cell<bool>,
}

impl Arrivals {
    fn new(kind: Kind) -> Arrivals {
        Arrivals {
            kind,
            rows: RefCell::default(),
            started: Cell::new(false),
            replaced: Cell::new(false),
            scheduled: Cell::new(false),
        }
    }

    /// Keep rows until the list is complete (`store` at the end puts them in at once).
    fn hold(&self, rows: Vec<glib::BoxedAnyObject>) {
        self.started.set(true);
        self.rows.borrow_mut().extend(rows);
    }

    fn add(self: &Rc<Self>, library: &Rc<Library>, rows: Vec<glib::BoxedAnyObject>) {
        self.started.set(true);
        self.rows.borrow_mut().extend(rows);
        if self.scheduled.replace(true) {
            return;
        }
        let (arrivals, library) = (self.clone(), library.clone());
        glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
            arrivals.store(&library, BATCH);
            if !arrivals.rows.borrow().is_empty() {
                return glib::ControlFlow::Continue;
            }
            arrivals.scheduled.set(false);
            glib::ControlFlow::Break
        });
    }

    /// Put up to `most` of the rows that are waiting into the store.
    fn store(&self, library: &Library, most: usize) {
        if !self.started.get() {
            return;
        }
        let rows: Vec<glib::BoxedAnyObject> = {
            let mut waiting = self.rows.borrow_mut();
            let count = waiting.len().min(most);
            waiting.drain(..count).collect()
        };
        let store = library.store(self.kind);
        if !self.replaced.replace(true) {
            store.splice(0, store.n_items(), &rows);
        } else if !rows.is_empty() {
            store.extend_from_slice(&rows);
        }
    }
}

fn map<T, U>(page: Page<T>, wrap: fn(T) -> U) -> Page<U> {
    Page::new(page.items.into_iter().map(wrap).collect(), page.next_offset, page.total)
}

#[cfg(test)]
mod song_tests {
    use super::*;

    fn song(catalog_id: Option<&str>, library_id: Option<&str>) -> Track {
        Track { library_id: library_id.map(str::to_string), ..Track::new(catalog_id, "Song A", "Artist A") }
    }

    #[test]
    fn the_library_copy_of_a_song_is_found_by_its_ids() {
        let songs = gio::ListStore::new::<glib::BoxedAnyObject>();
        for listed in [
            song(Some("10"), Some("i.1")),
            song(Some("20"), Some("i.2")),
            song(Some("30"), Some("i.3")),
            song(Some("20"), Some("i.4")),
        ] {
            songs.append(&boxed(listed));
        }
        // a song of the library (a playlist's, the queue's): its own entry, also when another
        // entry has the same catalog id before it
        assert_eq!(find_song(&songs, &song(Some("20"), Some("i.4"))), Some(3));
        assert_eq!(same_song(&song(Some("20"), Some("i.4")), &song(Some("20"), Some("i.4"))), Some(Same::Entry));
        // a song of the catalog (an album's page, a search, a shelf): the entry with its catalog id
        assert_eq!(find_song(&songs, &song(Some("30"), None)), Some(2));
        assert_eq!(find_song(&songs, &song(Some("20"), None)), Some(1));
        assert_eq!(same_song(&song(Some("20"), Some("i.2")), &song(Some("20"), None)), Some(Same::Catalog));
        // a library id the list does not have: the catalog id still finds it
        assert_eq!(find_song(&songs, &song(Some("10"), Some("i.9"))), Some(0));
        // not in the library, or without ids
        assert_eq!(find_song(&songs, &song(Some("40"), None)), None);
        assert_eq!(find_song(&songs, &song(None, None)), None);
        assert_eq!(same_song(&song(None, None), &song(None, None)), None);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use amui::types::{Album, Artist, Playlist, Station};

    #[test]
    fn a_saved_list_reads_back_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = Snapshots::new(dir.path().join("library"));
        // nothing saved, or something that is no list: no copy
        assert!(snapshots.read(Kind::Albums).is_none());
        std::fs::create_dir_all(dir.path().join("library")).unwrap();
        std::fs::write(snapshots.file(Kind::Albums), "not json").unwrap();
        assert!(snapshots.read(Kind::Albums).is_none());

        let items = vec![
            Item::Album(Album::new(Some("1000000001"), "Album A", "Artist A")),
            Item::Playlist(Playlist {
                library_id: Some("p.AbCdEf".into()),
                name: "Playlist A".into(),
                ..Playlist::default()
            }),
            Item::Artist(Artist::new(Some("1000000002"), "Artist B")),
            Item::Station(Station { id: "ra.1000000003".into(), name: "Station A".into(), ..Station::default() }),
        ];
        std::fs::write(snapshots.file(Kind::Recent), serde_json::to_vec(&items).unwrap()).unwrap();
        match snapshots.read(Kind::Recent) {
            Some(Rows::Items(read)) => assert_eq!(read, items),
            _ => panic!("the items read back"),
        }
        let songs = vec![Track::new(Some("1000000004"), "Song A", "Artist A"), Track::new(None, "Song B", "Artist B")];
        std::fs::write(snapshots.file(Kind::Songs), serde_json::to_vec(&songs).unwrap()).unwrap();
        match snapshots.read(Kind::Songs) {
            Some(Rows::Songs(read)) => assert_eq!(read, songs),
            _ => panic!("the songs read back"),
        }
        // a list of songs is not read as items, nor the other way round
        std::fs::copy(snapshots.file(Kind::Songs), snapshots.file(Kind::Albums)).unwrap();
        assert!(snapshots.read(Kind::Albums).is_none());
    }

    #[test]
    fn a_list_is_saved_in_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = Snapshots::new(dir.path().join("library"));
        let songs = vec![Track::new(Some("1000000004"), "Song A", "Artist A")];
        snapshots.write(Kind::Songs, Rows::Songs(songs.clone()));
        // written on a thread of its own
        let file = snapshots.file(Kind::Songs);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !file.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path().join("library")).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(matches!(snapshots.read(Kind::Songs), Some(Rows::Songs(read)) if read == songs));
    }
}
