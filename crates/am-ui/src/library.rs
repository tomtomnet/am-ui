//! The library as the pages show it: albums, artists, songs, playlists and what was added
//! recently, loaded once and kept in list models.
//!
//! A list is fetched in pages of 100. The first page says how many items there are; the rest
//! are fetched several at a time. A list that is empty fills as they arrive (those that arrive
//! while the window is busy, together); a list that has rows keeps them until the new ones are
//! complete, and then only what differs is changed (`diff`), so that a page that shows it stays
//! where it is. What was changed here and the service does not show yet (`edits`) counts over its
//! answers.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use amui::types::{Page, Playlist, Track};
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::data::{Collection, Item, Music, Problem, Res};
use crate::diff::update_store;
use crate::edits::Edits;
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

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Albums, Kind::Artists, Kind::Playlists, Kind::Recent, Kind::Songs];

    /// The lists that can hold playlists: what was changed in those counts over the service's
    /// answers.
    fn has_playlists(self) -> bool {
        matches!(self, Kind::Playlists | Kind::Recent)
    }
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
    /// The copy asked for last, per list: one that is older is not written after it.
    latest: Arc<Mutex<HashMap<Kind, u64>>>,
}

impl Snapshots {
    pub fn new(dir: std::path::PathBuf) -> Snapshots {
        Snapshots { dir, latest: Arc::default() }
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
        let (dir, latest) = (self.dir.clone(), self.latest.clone());
        let version = {
            let mut latest = latest.lock().unwrap();
            let version = latest.entry(kind).or_insert(0);
            *version += 1;
            *version
        };
        std::thread::spawn(move || {
            let data = match &rows {
                Rows::Songs(tracks) => serde_json::to_vec(tracks),
                Rows::Items(items) => serde_json::to_vec(items),
            };
            if let Ok(data) = data
                && amui::util::create_private_dirs(&dir).is_ok()
            {
                // the threads of several copies run side by side: the last one asked for stays
                let latest = latest.lock().unwrap();
                if latest.get(&kind) == Some(&version) {
                    let _ = amui::util::write_private_atomic(&path, &data);
                }
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
    /// The lists to fetch once more when their present fetch is over.
    again: RefCell<HashSet<Kind>>,
    started: Cell<bool>,
    /// A list finished loading, started loading or failed.
    pub changed: Signal<Kind>,
    /// Why the lists that failed last could not be fetched.
    problems: RefCell<HashMap<Kind, Problem>>,
    /// Where the lists are kept between runs; `None`: nowhere (the demonstration).
    snapshots: Option<Snapshots>,
    /// What was changed here and the service may not show yet.
    edits: RefCell<Edits>,
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
            again: RefCell::default(),
            started: Cell::new(false),
            changed: Signal::new(),
            problems: RefCell::default(),
            snapshots: None,
            edits: RefCell::default(),
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

    /// Why a list could not be fetched, if one could not.
    pub fn problem(&self) -> Option<Problem> {
        let problems = self.problems.borrow();
        Kind::ALL.iter().find_map(|kind| problems.get(kind).cloned())
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
            for kind in Kind::ALL {
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
        self.problems.borrow_mut().clear();
        for kind in Kind::ALL {
            self.reload_kind(music, kind);
        }
    }

    /// Load one list again. If it is being fetched, its fetch is followed by another: what is on
    /// its way may be from before what made this one necessary.
    pub fn reload_kind(self: &Rc<Self>, music: &Arc<dyn Music>, kind: Kind) {
        let m = music.clone();
        match kind {
            Kind::Albums => self.fill(
                music,
                kind,
                Arc::new(move |offset, limit| m.library_albums(offset, limit).map(|page| map(page, Item::Album))),
                boxed,
            ),
            Kind::Artists => self.fill(
                music,
                kind,
                Arc::new(move |offset, limit| m.library_artists(offset, limit).map(|page| map(page, Item::Artist))),
                boxed,
            ),
            Kind::Playlists => self.fill(
                music,
                kind,
                Arc::new(move |offset, limit| {
                    m.library_playlists(offset, limit).map(|page| shown(map(page, Item::Playlist)))
                }),
                boxed,
            ),
            Kind::Recent => self.fill(
                music,
                kind,
                Arc::new(move |offset, limit| m.library_recently_added(offset, limit).map(shown)),
                boxed,
            ),
            Kind::Songs => self.fill(music, kind, Arc::new(move |offset, limit| m.library_songs(offset, limit)), boxed),
        }
    }

    fn fill<T: Send + 'static>(
        self: &Rc<Self>,
        music: &Arc<dyn Music>,
        kind: Kind,
        fetch: Fetch<T>,
        wrap: fn(T) -> glib::BoxedAnyObject,
    ) {
        if !self.loading.borrow_mut().insert(kind) {
            self.again.borrow_mut().insert(kind);
            return;
        }
        self.changed.emit(&kind);
        let arrivals = Rc::new(Arrivals::new(kind));
        let (library, done, arrived, music) = (self.clone(), self.clone(), arrivals.clone(), music.clone());
        fetch_all(
            fetch,
            move |_offset, items| arrived.take(&library, items.into_iter().map(wrap).collect()),
            move |result| {
                arrivals.finish(&done, result.is_ok());
                done.loading.borrow_mut().remove(&kind);
                match result {
                    Ok(()) => {
                        done.loaded.borrow_mut().insert(kind);
                        done.problems.borrow_mut().remove(&kind);
                        done.save(kind);
                    }
                    Err(problem) => {
                        done.problems.borrow_mut().insert(kind, problem);
                    }
                }
                done.changed.emit(&kind);
                if done.again.borrow_mut().remove(&kind) {
                    done.reload_kind(&music, kind);
                }
            },
        );
    }

    /// Keep a list as it is now for the next run.
    fn save(&self, kind: Kind) {
        if let Some(snapshots) = &self.snapshots {
            snapshots.write(kind, Rows::of(kind, self.store(kind)));
        }
    }

    /// Make the list `kind` show `rows` (`None`: what it shows), with what was changed here over
    /// them. Only what differs changes.
    fn put(&self, kind: Kind, rows: Option<Vec<glib::BoxedAnyObject>>) {
        let store = self.store(kind);
        if !kind.has_playlists() {
            if let Some(rows) = rows {
                update_store(store, &rows);
            }
            return;
        }
        let items: Vec<Item> = match rows {
            Some(rows) => rows.iter().filter_map(|row| item_of(row.upcast_ref())).collect(),
            None => (0..store.n_items()).filter_map(|at| store.item(at).as_ref().and_then(item_of)).collect(),
        };
        let items = self.edits.borrow_mut().items(items, Instant::now());
        let rows: Vec<glib::BoxedAnyObject> = items.into_iter().map(boxed).collect();
        update_store(store, &rows);
    }

    /// What was changed here shows in the lists of playlists now.
    fn show_edits(&self) {
        for kind in [Kind::Playlists, Kind::Recent] {
            self.put(kind, None);
            // (a list that is only partly here is saved when it is complete)
            if self.is_loaded(kind) {
                self.save(kind);
            }
            self.changed.emit(&kind);
        }
    }

    /// A playlist was created with `songs` in it: it is in the lists from now on.
    pub fn playlist_created(&self, playlist: &Playlist, songs: &[Track]) {
        self.edits.borrow_mut().created(playlist, songs, Instant::now());
        self.show_edits();
    }

    /// A playlist got a new name and description.
    pub fn playlist_renamed(&self, library_id: &str, name: &str, description: &str) {
        self.edits.borrow_mut().renamed(library_id, name, description, Instant::now());
        self.show_edits();
    }

    /// A playlist was deleted: it is gone from the lists.
    pub fn playlist_deleted(&self, library_id: &str) {
        self.edits.borrow_mut().deleted(library_id, Instant::now());
        self.show_edits();
    }

    /// Songs were added to a playlist that had the songs `known`, as far as that is known.
    pub fn songs_added(&self, library_id: &str, songs: &[Track], known: &[Track]) {
        self.edits.borrow_mut().added(library_id, songs, known, Instant::now());
    }

    /// A song was removed from a playlist.
    pub fn song_removed(&self, library_id: &str, song: &Track) {
        self.edits.borrow_mut().removed(library_id, song, Instant::now());
    }

    /// An album or a playlist as the service gave it, with what was changed here over it.
    pub fn with_edits(&self, answer: &Collection) -> Collection {
        self.edits.borrow_mut().collection(answer, Instant::now())
    }
}

/// The rows of a list that has rows are kept until the new ones are complete, and those that have
/// arrived for an empty list go into its store as they come. The pages that arrive while the
/// window is busy go in together, `BATCH` rows at most with frames in between: a list that is
/// sorted or shown changes once for them, not once a page.
struct Arrivals {
    kind: Kind,
    rows: RefCell<Vec<glib::BoxedAnyObject>>,
    /// Whether the rows go into the store as they come: the list was empty when the first page
    /// arrived. `None`: none has yet.
    filling: Cell<Option<bool>>,
    /// The first rows are in the store: what follows is added to them.
    begun: Cell<bool>,
    /// Something is going to store what is waiting when the window is idle.
    scheduled: Cell<bool>,
}

impl Arrivals {
    fn new(kind: Kind) -> Arrivals {
        Arrivals {
            kind,
            rows: RefCell::default(),
            filling: Cell::new(None),
            begun: Cell::new(false),
            scheduled: Cell::new(false),
        }
    }

    /// The rows of the next page.
    fn take(self: &Rc<Self>, library: &Rc<Library>, rows: Vec<glib::BoxedAnyObject>) {
        let filling = self.filling.get().unwrap_or_else(|| library.store(self.kind).n_items() == 0);
        self.filling.set(Some(filling));
        self.rows.borrow_mut().extend(rows);
        if !filling || self.scheduled.replace(true) {
            return;
        }
        let (arrivals, library) = (self.clone(), library.clone());
        glib::spawn_future_local(async move {
            loop {
                // when the window has nothing more urgent to do
                glib::timeout_future_with_priority(glib::Priority::DEFAULT_IDLE, Duration::ZERO).await;
                arrivals.store(&library, BATCH);
                if arrivals.rows.borrow().is_empty() {
                    break;
                }
            }
            arrivals.scheduled.set(false);
        });
    }

    /// Put up to `most` of the rows that are waiting into the store. The first ones take the place
    /// of what is there (a copy of the last run that came in after the first page was fetched).
    fn store(&self, library: &Library, most: usize) {
        let rows: Vec<glib::BoxedAnyObject> = {
            let mut waiting = self.rows.borrow_mut();
            let count = waiting.len().min(most);
            waiting.drain(..count).collect()
        };
        let store = library.store(self.kind);
        if !self.begun.replace(true) {
            store.splice(0, store.n_items(), &rows);
        } else if !rows.is_empty() {
            store.extend_from_slice(&rows);
        }
    }

    /// The fetch is over. A list that filled gets what is still waiting; one that had rows gets
    /// the new ones, if they are complete (else it stays as it was).
    fn finish(&self, library: &Library, complete: bool) {
        match self.filling.get() {
            Some(true) => {
                self.store(library, usize::MAX);
                if self.kind.has_playlists() {
                    library.put(self.kind, None);
                }
            }
            Some(false) if complete => library.put(self.kind, Some(self.rows.take())),
            _ => {}
        }
    }
}

fn map<T, U>(page: Page<T>, wrap: fn(T) -> U) -> Page<U> {
    Page::new(page.items.into_iter().map(wrap).collect(), page.next_offset, page.total)
}

/// A page of a list without the playlists that have no name: that is how the service lists a
/// playlist that is being removed.
fn shown(page: Page<Item>) -> Page<Item> {
    let named = |item: &Item| !matches!(item, Item::Playlist(playlist) if playlist.name.trim().is_empty());
    Page::new(page.items.into_iter().filter(named).collect(), page.next_offset, page.total)
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use crate::demo::DemoMusic;

    /// Longer than any test takes: what is changed in the demonstration's library stays unseen.
    const FOREVER: Duration = Duration::from_secs(3600);

    /// Run `test` with a main loop for this thread; `settle(done)` runs it until `done` holds.
    fn with_loop(test: impl FnOnce(&dyn Fn(&dyn Fn() -> bool))) {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                test(&|done: &dyn Fn() -> bool| {
                    let start = Instant::now();
                    while !done() {
                        context.iteration(false);
                        std::thread::sleep(Duration::from_millis(1));
                        assert!(start.elapsed() < Duration::from_secs(20), "the library did not finish loading");
                    }
                    // what was scheduled along the way
                    while context.iteration(false) {}
                });
            })
            .unwrap();
    }

    fn names(library: &Library, kind: Kind) -> Vec<String> {
        let store = library.store(kind);
        (0..store.n_items())
            .filter_map(|at| store.item(at).as_ref().and_then(item_of))
            .map(|item| item.title().to_string())
            .collect()
    }

    fn idle(library: &Library) -> bool {
        Kind::ALL.iter().all(|kind| !library.is_loading(*kind))
    }

    #[test]
    fn answers_from_before_a_change_do_not_undo_it() {
        with_loop(|settle| {
            let music: Arc<dyn Music> = Arc::new(DemoMusic::quick(FOREVER, ""));
            let library = Rc::new(Library::default());
            library.load(&music);
            settle(&|| idle(&library));
            let before = names(&library, Kind::Playlists);
            assert_eq!(before[0], "Favorite Songs");
            assert!(before.len() > 5 && names(&library, Kind::Recent).contains(&before[1]));

            // a new playlist shows at once, after Favorite Songs and at the top of what is recent
            let created = music.create_playlist("Brand New", "Words", &[]).unwrap();
            library.playlist_created(&created, &[]);
            assert_eq!(names(&library, Kind::Playlists)[..2], ["Favorite Songs", "Brand New"]);
            assert_eq!(names(&library, Kind::Recent)[0], "Brand New");
            // the service's list, still without it, leaves it there
            library.reload_kind(&music, Kind::Playlists);
            library.reload_kind(&music, Kind::Recent);
            settle(&|| idle(&library));
            assert_eq!(names(&library, Kind::Playlists)[..2], ["Favorite Songs", "Brand New"]);
            assert_eq!(names(&library, Kind::Recent)[0], "Brand New");

            // a deleted playlist goes at once and stays away, though it is still listed (without
            // a name)
            let gone = before[1].clone();
            music.delete_playlist("p.1").unwrap();
            library.playlist_deleted("p.1");
            assert!(!names(&library, Kind::Playlists).contains(&gone));
            assert!(!names(&library, Kind::Recent).contains(&gone));
            library.reload_kind(&music, Kind::Playlists);
            library.reload_kind(&music, Kind::Recent);
            settle(&|| idle(&library));
            assert!(!names(&library, Kind::Playlists).contains(&gone));
            assert!(!names(&library, Kind::Playlists).iter().any(String::is_empty));
            assert!(!names(&library, Kind::Recent).iter().any(String::is_empty));

            // a new name counts over the old one
            music.rename_playlist("p.2", "New Name", "").unwrap();
            library.playlist_renamed("p.2", "New Name", "");
            assert!(names(&library, Kind::Playlists).contains(&"New Name".to_string()));
            library.reload(&music);
            settle(&|| idle(&library));
            assert!(names(&library, Kind::Playlists).contains(&"New Name".to_string()));
            assert!(names(&library, Kind::Recent).contains(&"New Name".to_string()));
        });
    }

    #[test]
    fn the_saved_lists_follow_what_was_changed() {
        with_loop(|settle| {
            let dir = tempfile::tempdir().unwrap();
            let snapshots = Snapshots::new(dir.path().join("library"));
            let saved = |kind: Kind| -> Vec<String> {
                match snapshots.read(kind) {
                    Some(Rows::Items(items)) => items.iter().map(|item| item.title().to_string()).collect(),
                    _ => Vec::new(),
                }
            };
            let music: Arc<dyn Music> = Arc::new(DemoMusic::quick(FOREVER, ""));
            let library = Rc::new(Library::with_snapshots(snapshots.clone()));
            library.load(&music);
            settle(&|| idle(&library) && !saved(Kind::Playlists).is_empty());
            settle(&|| saved(Kind::Playlists) == names(&library, Kind::Playlists));
            let deleted = names(&library, Kind::Playlists)[1].clone();

            let created = music.create_playlist("Brand New", "", &[]).unwrap();
            library.playlist_created(&created, &[]);
            music.delete_playlist("p.1").unwrap();
            library.playlist_deleted("p.1");
            for kind in [Kind::Playlists, Kind::Recent] {
                settle(&|| saved(kind) == names(&library, kind));
                assert!(saved(kind).contains(&"Brand New".to_string()), "{kind:?}");
                assert!(!saved(kind).contains(&deleted), "{kind:?}");
            }
            // an answer from before the changes leaves the copy as it is
            library.reload(&music);
            settle(&|| idle(&library));
            settle(&|| saved(Kind::Playlists) == names(&library, Kind::Playlists));
            assert_eq!(saved(Kind::Playlists)[1], "Brand New");
            assert!(!saved(Kind::Playlists).contains(&deleted) && !saved(Kind::Playlists).iter().any(String::is_empty));
        });
    }

    #[test]
    fn a_list_that_is_fetched_again_changes_only_where_it_differs() {
        with_loop(|settle| {
            let music: Arc<dyn Music> = Arc::new(DemoMusic::quick(Duration::ZERO, "grow"));
            let library = Rc::new(Library::default());
            library.load(&music);
            settle(&|| idle(&library));
            let (albums, songs) = (library.albums.n_items(), library.songs.n_items());
            assert!(albums > 0 && songs > 0);
            let first = library.albums.item(0).unwrap();
            let changes = Rc::new(RefCell::new(Vec::new()));
            let seen = changes.clone();
            library.albums.connect_items_changed(move |_, at, removed, added| {
                seen.borrow_mut().push((at, removed, added));
            });
            let unchanged = Rc::new(Cell::new(true));
            let flag = unchanged.clone();
            library.songs.connect_items_changed(move |_, _, _, _| flag.set(false));

            // the service has one album more now: one row is added and the others stay as they are
            music.refresh();
            library.reload(&music);
            settle(&|| idle(&library));
            assert_eq!(library.albums.n_items(), albums + 1);
            assert_eq!(*changes.borrow(), [(albums, 0, 1)]);
            assert_eq!(library.albums.item(0).unwrap(), first);
            // the same answer as before: the list is not touched
            assert!(unchanged.get());
            assert_eq!(library.songs.n_items(), songs);
        });
    }

    #[test]
    fn a_failed_fetch_leaves_a_list_as_it_was() {
        with_loop(|settle| {
            let music: Arc<dyn Music> = Arc::new(DemoMusic::quick(Duration::ZERO, "fail-once"));
            let library = Rc::new(Library::default());
            library.load(&music);
            settle(&|| idle(&library));
            let before = names(&library, Kind::Albums);
            assert!(library.problem().is_none() && !before.is_empty());

            // every list fails: the lists stay
            music.refresh();
            library.reload(&music);
            settle(&|| idle(&library));
            assert_eq!(
                library.problem().map(|problem| problem.message),
                Some("The demonstration was told to fail once.".to_string())
            );
            assert_eq!(names(&library, Kind::Albums), before);

            // the next time it works
            music.refresh();
            library.reload(&music);
            settle(&|| idle(&library));
            assert!(library.problem().is_none());
            assert_eq!(names(&library, Kind::Albums), before);
        });
    }

    #[test]
    fn asking_for_a_list_that_is_being_fetched_fetches_it_again_afterwards() {
        with_loop(|settle| {
            let demo = Arc::new(DemoMusic::quick(Duration::ZERO, ""));
            let music: Arc<dyn Music> = demo.clone();
            let library = Rc::new(Library::default());
            library.load(&music);
            settle(&|| idle(&library));
            let before = demo.playlist_list_requests();
            library.reload_kind(&music, Kind::Playlists);
            library.reload_kind(&music, Kind::Playlists);
            library.reload_kind(&music, Kind::Playlists);
            settle(&|| idle(&library));
            // the one under way and one more for what came while it ran
            assert_eq!(demo.playlist_list_requests() - before, 2);
        });
    }
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
