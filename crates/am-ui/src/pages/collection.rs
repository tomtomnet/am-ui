//! The pages of an album and of a playlist: a header (artwork, what it is, what can be done with
//! it), the songs, and a footer with their number and length. Header, songs and footer scroll as
//! one column; the list of songs is embedded in it (`widgets::embedded`).
//!
//! A long playlist arrives in pages of 100 songs. The first answer brings the header, the first
//! songs and the number of all of them; the other pages are fetched several at a time
//! (`library::fetch_all`) and their rows are added in order, so the page is complete after about
//! the time of one more request.
//!
//! The page is built once. What changes later, a song removed, a new answer of the service, is
//! changed in it (only the rows that differ, `diff::update_store`): the list keeps its place and
//! the page where it is scrolled to. What the service shows is what it answered, with what was
//! changed here and it does not show yet over it (`edits`).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use adw::prelude::*;
use amui::types::{Artwork, Page, Playlist, Track};
use gtk::{gio, glib};

use crate::actions;
use crate::app::ctx;
use crate::data::{Collection, Item, Music, Problem, Ref, Res};
use crate::diff::update_store;
use crate::library::{self, boxed, item_of, track_of};
use crate::nav::Dest;
use crate::pages::detail::{self, Cache, Frame};
use crate::pages::shell;
use crate::pages::songs::{Line, Place, track_list_in};
use crate::refresh::{Refreshing, Task};
use crate::signal::background;
use crate::widgets::cover::{Blank, Cover, mosaic};
use crate::widgets::embedded::Embedded;
use crate::widgets::excerpt::Excerpt;
use crate::widgets::hero::Hero;
use crate::widgets::total_time;
use crate::widgets::track_row::RowStyle;

/// The side of the artwork, from a narrow window to a wide one.
const PICTURE: (i32, i32) = (180, 264);
/// The lines of a description that are shown before "more".
const DESCRIPTION_LINES: i32 = 3;

thread_local! {
    static CACHE: RefCell<Cache<Collection>> = const { RefCell::new(Cache::new()) };
    static HOOKED: Cell<bool> = const { Cell::new(false) };
    /// What was added to the library from its page, until the service says so itself.
    static ADDED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Album,
    Playlist,
}

/// Fetch an album or a playlist with all its songs.
///
/// One answer holds an album or a short playlist: `done` gets it. A long playlist arrives in
/// pages: `first` is called with the first answer (the playlist, its first songs and how many
/// there are), `more` with the songs of every further page, in order, and `done` at the end with
/// everything, or with the first problem. All of them run on the user interface thread.
pub fn fetch(
    music: Arc<dyn Music>,
    kind: Kind,
    target: Ref,
    first: impl FnOnce(&Collection) + 'static,
    more: impl Fn(&[Track]) + 'static,
    done: impl FnOnce(Result<Collection, Problem>) + 'static,
) {
    let opening = music.clone();
    fetch_from(
        move || match kind {
            Kind::Album => opening.album(&target),
            Kind::Playlist => opening.playlist(&target),
        },
        move |library_id, offset, limit| music.playlist_tracks(library_id, offset, limit),
        first,
        more,
        done,
    );
}

/// `fetch` with the two requests it makes: `open` for the first answer, `pages(library id,
/// offset, limit)` for the further pages of a long playlist.
fn fetch_from(
    open: impl FnOnce() -> Res<Collection> + Send + 'static,
    pages: impl Fn(&str, i64, i64) -> Res<Page<Track>> + Send + Sync + 'static,
    first: impl FnOnce(&Collection) + 'static,
    more: impl Fn(&[Track]) + 'static,
    done: impl FnOnce(Result<Collection, Problem>) + 'static,
) {
    background(open, move |answer| {
        let collection = match answer {
            Ok(collection) => collection,
            Err(problem) => return done(Err(problem)),
        };
        let library_id = match &collection.item {
            Item::Playlist(playlist) => playlist.library_id.clone(),
            _ => None,
        };
        let (Some(next), Some(library_id)) = (collection.next_offset, library_id) else {
            return done(Ok(Collection { next_offset: None, ..collection }));
        };
        first(&collection);
        // every page through `fetch_all`, which asks for several at a time and hands them over
        // in order; its first request is answered from what is here already
        let head = Page::new(collection.tracks.clone(), Some(next), collection.total);
        let all: Arc<dyn Fn(i64, i64) -> Res<Page<Track>> + Send + Sync> =
            Arc::new(
                move |offset, limit| {
                    if offset == 0 { Ok(head.clone()) } else { pages(&library_id, offset, limit) }
                },
            );
        let whole = Rc::new(RefCell::new(Some(collection)));
        let growing = whole.clone();
        library::fetch_all(
            all,
            move |offset, tracks: Vec<Track>| {
                if offset > 0 {
                    more(&tracks);
                    if let Some(collection) = growing.borrow_mut().as_mut() {
                        collection.tracks.extend(tracks);
                    }
                }
            },
            move |result| {
                let Some(collection) = whole.take() else { return };
                done(result.map(|()| Collection { next_offset: None, ..collection }));
            },
        );
    });
}

/// The header of a page and the parts of it that change on their own.
struct Header {
    hero: Hero,
    /// The widget that names the page.
    title: gtk::Label,
    cover: Cover,
    /// What the cover shows.
    artworks: Vec<Artwork>,
    /// The small line ("500 SONGS") and the row it is in.
    words: gtk::Label,
    meta: gtk::Box,
    play: gtk::Button,
    shuffle: gtk::Button,
}

/// What the content of a page keeps at hand after it was built.
struct Parts {
    column: gtk::Box,
    header: Header,
    /// The album or playlist the header was built for.
    item: Item,
    /// The rows of the list of songs.
    store: gio::ListStore,
    /// What stands in place of the list while there are no songs.
    note: gtk::Label,
    list: Embedded,
    /// What the rows know about the page (they were made for it).
    place: Place,
    /// The list has headings for its discs.
    discs: bool,
    footer: gtk::Label,
}

/// The page of one album or playlist.
struct View {
    kind: Kind,
    dest: Dest,
    target: Ref,
    page: glib::WeakRef<adw::NavigationPage>,
    frame: Frame,
    /// What the service answered last.
    answer: RefCell<Option<Rc<Collection>>>,
    /// What is shown: that answer with what was changed here over it; while a long playlist
    /// loads, the playlist and its first songs.
    shown: RefCell<Option<Rc<Collection>>>,
    /// Every song of what is shown is there.
    complete: Cell<bool>,
    parts: RefCell<Option<Parts>>,
    /// Counts the fetches: an answer to an earlier one is dropped.
    generation: Cell<u64>,
    busy: Cell<bool>,
    /// When what is shown was fetched (`None`: to be fetched again once the page is seen).
    fetched: Cell<Option<Instant>>,
    /// Play (true: shuffled) as soon as every song is there.
    pending: Cell<Option<bool>>,
    /// The playlist was deleted.
    gone: Cell<bool>,
}

/// The copies the cache holds follow what happens to their playlists and the wish for fresh
/// data (once for the program).
fn hook() {
    if HOOKED.replace(true) {
        return;
    }
    let is = |dest: &Dest, id: &str| match dest {
        Dest::Playlist(target) => target.library_id.as_deref() == Some(id),
        _ => false,
    };
    let ctx = ctx();
    ctx.refreshed.connect_forever(|_| CACHE.with(|cache| cache.borrow_mut().expire(|_| true)));
    ctx.playlist_changed.connect_forever(move |id| CACHE.with(|cache| cache.borrow_mut().expire(|dest| is(dest, id))));
    ctx.playlist_deleted.connect_forever(move |id| CACHE.with(|cache| cache.borrow_mut().forget(|dest| is(dest, id))));
}

/// A playlist that was just created: its page has something to show at once, whatever the
/// service knows of it yet, and asks for the rest when it opens.
pub fn remember_created(playlist: &Playlist) {
    let dest = Dest::Playlist(actions::reference(&playlist.catalog_id, &playlist.library_id));
    let collection =
        Collection { item: Item::Playlist(playlist.clone()), tracks: Vec::new(), next_offset: None, total: Some(0) };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.put(&dest, Rc::new(collection), Instant::now());
        cache.expire(|shown| *shown == dest);
    });
}

/// The songs the service last gave for the library playlist `id`, as far as it was asked.
pub fn known_songs(id: &str) -> Vec<Track> {
    let wanted = |dest: &Dest| matches!(dest, Dest::Playlist(target) if target.library_id.as_deref() == Some(id));
    CACHE.with(|cache| cache.borrow().find(wanted)).map_or_else(Vec::new, |collection| collection.tracks.clone())
}

/// The page of the album or playlist `target` (the destination `dest`).
pub fn page(dest: &Dest, kind: Kind, target: &Ref) -> adw::NavigationPage {
    hook();
    let holder = adw::Bin::new();
    let shell = shell(dest.title(), &holder);
    let frame = Frame::new(&shell.header);
    holder.set_child(Some(&frame.stack));
    let view = Rc::new(View {
        kind,
        dest: dest.clone(),
        target: target.clone(),
        page: shell.page.downgrade(),
        frame,
        answer: RefCell::default(),
        shown: RefCell::default(),
        complete: Cell::new(false),
        parts: RefCell::default(),
        generation: Cell::new(0),
        busy: Cell::new(false),
        fetched: Cell::new(None),
        pending: Cell::new(None),
        gone: Cell::new(false),
    });
    let weak = Rc::downgrade(&view);
    view.frame.connect_retry(move || {
        if let Some(view) = weak.upgrade() {
            view.fetch(None);
        }
    });
    let ctx = ctx();
    let weak = Rc::downgrade(&view);
    ctx.refreshed.connect(&view.frame.stack, move |_, refreshing| {
        if let Some(view) = weak.upgrade() {
            view.outdated(Some(refreshing));
        }
    });
    let weak = Rc::downgrade(&view);
    ctx.playlist_changed.connect(&view.frame.stack, move |_, id| {
        if let Some(view) = weak.upgrade()
            && view.is_playlist(id)
        {
            view.edited();
            view.outdated(None);
        }
    });
    let weak = Rc::downgrade(&view);
    ctx.playlist_deleted.connect(&view.frame.stack, move |_, id| {
        if let Some(view) = weak.upgrade()
            && view.is_playlist(id)
        {
            view.deleted();
        }
    });
    // The page owns its view. Each time it is shown again (the way back leads to it) it looks
    // whether what it shows is still fresh.
    let seen = view.clone();
    shell.page.connect_showing(move |_| seen.seen());
    view.open();
    shell.page
}

impl View {
    /// Whether this is the page of the library playlist `id`.
    fn is_playlist(&self, id: &str) -> bool {
        // the page may have been opened by the playlist's catalog id: what it shows knows more
        let shown = match self.shown.borrow().as_deref() {
            Some(Collection { item: Item::Playlist(playlist), .. }) => playlist.library_id.clone(),
            _ => None,
        };
        self.kind == Kind::Playlist && (self.target.library_id.as_deref() == Some(id) || shown.as_deref() == Some(id))
    }

    /// What the cache has at once; a request when that is nothing or old.
    fn open(self: &Rc<Self>) {
        match CACHE.with(|cache| cache.borrow_mut().get(&self.dest)) {
            Some((collection, fetched)) => {
                self.fetched.set(fetched);
                self.show(collection, true);
                if !detail::is_fresh(fetched, Instant::now()) {
                    self.fetch(None);
                }
            }
            None => self.fetch(None),
        }
    }

    /// The page is on screen (again).
    fn seen(self: &Rc<Self>) {
        if self.gone.get() || self.busy.get() {
            return;
        }
        if !self.frame.has_content() || !detail::is_fresh(self.fetched.get(), Instant::now()) {
            self.fetch(None);
        }
    }

    /// What was changed here (a song removed, a new name) shows at once, on screen or not.
    fn edited(self: &Rc<Self>) {
        let answer = self.answer.borrow().clone();
        if let Some(answer) = answer.filter(|_| self.complete.get()) {
            self.show(answer, true);
        }
    }

    /// What is shown may have changed: ask again now if the page is on screen, else when it is
    /// seen next. `refreshing`: the user asked for fresh data and waits for the answer.
    fn outdated(self: &Rc<Self>, refreshing: Option<&Refreshing>) {
        self.fetched.set(None);
        if self.frame.stack.is_mapped() && !self.gone.get() {
            self.fetch(refreshing.map(Refreshing::task));
        }
    }

    fn deleted(self: &Rc<Self>) {
        self.gone.set(true);
        self.busy.set(false);
        self.generation.set(self.generation.get() + 1);
        self.shown.replace(None);
        self.parts.replace(None);
        self.frame.gone("am-view-list-symbolic", "Playlist Deleted", "This playlist is no longer in your library.");
        let ctx = ctx();
        if ctx.nav.current().as_ref() == Some(&self.dest) {
            ctx.nav.back();
        }
    }

    /// Ask the service. While the page shows something that happens quietly: what differs in the
    /// answer changes on the page, and a failure is only told to `task`, the refresh that waits.
    fn fetch(self: &Rc<Self>, task: Option<Task>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.busy.set(true);
        let quiet = self.frame.has_content();
        if !quiet {
            self.frame.wait();
        }
        let current =
            move |view: &std::rc::Weak<View>| view.upgrade().filter(|view| view.generation.get() == generation);
        let (first, more, done) = (Rc::downgrade(self), Rc::downgrade(self), Rc::downgrade(self));
        fetch(
            ctx().music.clone(),
            self.kind,
            self.target.clone(),
            move |collection| {
                // a long playlist: its header and first songs now, the rest as it arrives
                if let Some(view) = current(&first)
                    && !quiet
                {
                    view.show(Rc::new(collection.clone()), false);
                }
            },
            move |tracks| {
                if let Some(view) = current(&more)
                    && !quiet
                    && let Some(parts) = view.parts.borrow().as_ref()
                {
                    let rows: Vec<glib::BoxedAnyObject> = tracks.iter().cloned().map(boxed).collect();
                    parts.store.extend_from_slice(&rows);
                }
            },
            move |result| {
                let Some(view) = current(&done) else { return };
                view.busy.set(false);
                match result {
                    Ok(collection) => view.arrived(Rc::new(collection)),
                    Err(problem) => view.failed(&problem, task.as_ref()),
                }
            },
        );
    }

    /// Every song is here.
    fn arrived(self: &Rc<Self>, collection: Rc<Collection>) {
        let now = Instant::now();
        self.fetched.set(Some(now));
        CACHE.with(|cache| cache.borrow_mut().put(&self.dest, collection.clone(), now));
        self.show(collection, true);
    }

    fn failed(&self, problem: &Problem, task: Option<&Task>) {
        let ctx = ctx();
        if let Some(task) = task {
            task.failed(problem);
        }
        if !self.frame.has_content() {
            let title = match self.kind {
                Kind::Album => "Couldn't Load This Album",
                Kind::Playlist => "Couldn't Load This Playlist",
            };
            self.frame.fail(title, problem);
        } else if !self.complete.get() {
            // part of a long playlist is shown: the rest is asked for when the page is seen next
            self.fetched.set(None);
            ctx.toast.emit(&format!("Not all songs could be loaded: {}", problem.message));
        }
    }

    /// The album or playlist as it is to be shown: `answer` with what was changed here over it.
    fn with_edits(&self, answer: &Collection) -> Collection {
        match self.kind {
            Kind::Playlist => ctx().library.with_edits(answer),
            Kind::Album => answer.clone(),
        }
    }

    /// Show what the service answered: the page is built the first time and changed after that,
    /// as far as it was made for what is shown now.
    fn show(self: &Rc<Self>, answer: Rc<Collection>, complete: bool) {
        let collection = Rc::new(self.with_edits(&answer));
        self.answer.replace(Some(answer));
        let (place, discs) = (self.place(&collection), self.has_discs(&collection));
        let fits = self.parts.borrow().as_ref().is_some_and(|parts| parts.place == place && parts.discs == discs);
        if fits {
            self.update(&collection, complete);
        } else {
            let (column, title, parts) = self.build(&collection, complete, place, discs);
            self.frame.show(&column, &title);
            self.parts.replace(Some(parts));
        }
        if let Some(page) = self.page.upgrade() {
            page.set_title(collection.item.title());
        }
        self.shown.replace(Some(collection));
        self.complete.set(complete);
        if complete {
            self.completed();
        }
    }

    /// Bring what the page shows up to `collection`: only what differs changes, and the list and
    /// the page stay where they are.
    fn update(self: &Rc<Self>, collection: &Collection, complete: bool) {
        let mut parts = self.parts.borrow_mut();
        let Some(parts) = parts.as_mut() else { return };
        let rows: Vec<glib::BoxedAnyObject> = collection.tracks.iter().cloned().map(boxed).collect();
        update_store(&parts.store, &rows);
        parts.note.set_visible(collection.tracks.is_empty() && complete);
        parts.list.set_visible(!collection.tracks.is_empty());
        if parts.item != collection.item {
            // another name, another description: a new header, in the place of the old one
            let header = self.header(collection);
            parts.column.remove(&parts.header.hero);
            parts.column.prepend(&header.hero);
            self.frame.set_title(&header.title);
            parts.header = header;
            parts.item = collection.item.clone();
        } else {
            parts.header.follow(collection);
        }
        parts.footer.set_text(&footer_text(collection, complete));
    }

    /// What is shown has all its songs: the footer can say how long they are, and what waited
    /// for them can go on.
    fn completed(self: &Rc<Self>) {
        if let (Some(collection), Some(parts)) = (self.shown.borrow().as_ref(), self.parts.borrow().as_ref()) {
            parts.footer.set_text(&footer_text(collection, true));
        }
        if let Some(shuffle) = self.pending.take() {
            self.play(shuffle);
        }
        if let Some(offset) = detail::initial_scroll() {
            self.frame.scroll_soon(offset);
        }
    }

    /// Play every song, from the first one that can be played.
    fn play(self: &Rc<Self>, shuffle: bool) {
        if self.busy.get() && !self.complete.get() {
            // the rest of a long playlist is on its way
            self.pending.set(Some(shuffle));
            return;
        }
        let tracks = self.shown.borrow().as_ref().map(|collection| collection.tracks.clone()).unwrap_or_default();
        let ctx = ctx();
        let Some(start) = tracks.iter().position(Track::playable) else {
            ctx.toast.emit(&"There are no songs to play".to_string());
            return;
        };
        if shuffle {
            ctx.player.playback.set_shuffle(true);
        }
        actions::play_tracks(tracks, start);
    }

    /// Add what the page shows to the library; `button` gives way to `mark` once that is done.
    fn add(self: &Rc<Self>, button: &gtk::Button, mark: &gtk::Image) {
        let Some(collection) = self.shown.borrow().clone() else { return };
        let (item, title, key) = (collection.item.clone(), collection.item.title().to_string(), collection.item.key());
        button.set_sensitive(false);
        let music = ctx().music.clone();
        let (button, mark, view) = (button.downgrade(), mark.downgrade(), Rc::downgrade(self));
        background(
            move || music.add_to_library(&item),
            move |result| {
                let ctx = ctx();
                match result {
                    Ok(()) => {
                        ADDED.with(|added| added.borrow_mut().insert(key));
                        ctx.toast.emit(&format!("{title} added to your library"));
                        if let (Some(button), Some(mark)) = (button.upgrade(), mark.upgrade()) {
                            button.set_visible(false);
                            mark.set_visible(true);
                        }
                        // Apple applies an addition a moment later: the library's lists then,
                        // this page when it is seen next
                        if let Some(view) = view.upgrade() {
                            view.fetched.set(None);
                            CACHE.with(|cache| cache.borrow_mut().expire(|dest| *dest == view.dest));
                        }
                        actions::reload_library_soon();
                    }
                    Err(problem) => {
                        ctx.toast.emit(&problem.message);
                        if let Some(button) = button.upgrade() {
                            button.set_sensitive(true);
                        }
                    }
                }
            },
        );
    }

    /// What the rows of the page know about it.
    fn place(&self, collection: &Collection) -> Place {
        match &collection.item {
            Item::Playlist(playlist) => Place {
                playlist: playlist.library_id.clone().filter(|_| playlist.can_edit),
                line: Line::Style,
                songs_page: false,
            },
            Item::Album(album) => {
                Place { playlist: None, line: Line::ArtistUnless(album.artist.clone()), songs_page: false }
            }
            _ => Place::default(),
        }
    }

    /// Whether the list of the page needs a heading for every disc.
    fn has_discs(&self, collection: &Collection) -> bool {
        self.kind == Kind::Album && several_discs(&collection.tracks)
    }

    /// The column of the page: header, songs, footer. Also the widget that names the page.
    fn build(
        self: &Rc<Self>,
        collection: &Collection,
        complete: bool,
        place: Place,
        discs: bool,
    ) -> (gtk::Box, gtk::Widget, Parts) {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = self.header(collection);
        column.append(&header.hero);

        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let rows: Vec<glib::BoxedAnyObject> = collection.tracks.iter().cloned().map(boxed).collect();
        store.extend_from_slice(&rows);
        let note = gtk::Label::new(Some(match self.kind {
            Kind::Album => "This album has no songs.",
            Kind::Playlist => "This playlist has no songs yet.",
        }));
        note.set_xalign(0.0);
        note.add_css_class("page-note");
        note.set_visible(collection.tracks.is_empty() && complete);
        column.append(&note);
        let list = Embedded::new(&self.songs(collection, &store, place.clone(), discs));
        list.set_visible(!collection.tracks.is_empty());
        column.append(&list);

        let footer = gtk::Label::new(Some(&footer_text(collection, complete)));
        footer.set_xalign(0.0);
        footer.set_wrap(true);
        footer.add_css_class("page-footer");
        column.append(&footer);
        let title = header.title.clone().upcast();
        let parts = Parts {
            column: column.clone(),
            header,
            item: collection.item.clone(),
            store,
            note,
            list,
            place,
            discs,
            footer,
        };
        (column, title, parts)
    }

    /// The list of the songs in `store`, with a heading for every disc when there are several.
    fn songs(&self, collection: &Collection, store: &gio::ListStore, place: Place, discs: bool) -> gtk::ListView {
        let style = match &collection.item {
            Item::Album(_) => RowStyle::ALBUM,
            _ => RowStyle::LIST,
        };
        let list = if discs {
            // a section for every disc: the songs come in that order already
            let sections = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
            sections.set_section_sorter(Some(&gtk::CustomSorter::new(|a, b| disc_of(a).cmp(&disc_of(b)).into())));
            let list = track_list_in(&sections, style, place);
            list.set_header_factory(Some(&disc_headings()));
            list
        } else {
            track_list_in(store, style, place)
        };
        // Tab leaves the list (the arrow keys move in it): the footer and the player are not
        // hundreds of rows away
        list.set_tab_behavior(gtk::ListTabBehavior::Item);
        list.add_css_class("embedded");
        list
    }

    /// The header: artwork, name, who it is by, what it is, and the buttons.
    fn header(self: &Rc<Self>, collection: &Collection) -> Header {
        let item = &collection.item;
        let cover = Cover::new(PICTURE.0);
        cover.set_blank(Blank::Icon(match item {
            Item::Playlist(_) => "am-audio-x-generic-symbolic",
            _ => "am-album-symbolic",
        }));
        let artworks = artworks_of(collection);
        cover.set_artworks(&artworks);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let title = gtk::Label::new(Some(item.title()));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        title.set_lines(3);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("hero-title");
        text.append(&title);

        match item {
            Item::Album(album) if album.artist_catalog_id.is_some() => {
                // the artist's name leads to the artist
                let artist = gtk::Button::with_label(&album.artist);
                artist.add_css_class("flat");
                artist.add_css_class("hero-link");
                artist.set_halign(gtk::Align::Start);
                artist.set_tooltip_text(Some("Go to Artist"));
                if let Some(label) = artist.child().and_downcast::<gtk::Label>() {
                    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                }
                let target = actions::reference(&album.artist_catalog_id, &None);
                artist.connect_clicked(move |_| ctx().nav.go(Dest::Artist(target.clone())));
                text.append(&artist);
            }
            _ => {
                let by = match item {
                    Item::Album(album) => album.artist.clone(),
                    Item::Playlist(playlist) => playlist.curator.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                if !by.is_empty() {
                    let byline = gtk::Label::new(Some(&by));
                    byline.set_xalign(0.0);
                    byline.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    byline.add_css_class("hero-byline");
                    text.append(&byline);
                }
            }
        }

        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        meta.add_css_class("hero-meta");
        let (line, formats) = meta_of(collection);
        let words = gtk::Label::new(Some(&line));
        words.set_xalign(0.0);
        words.set_ellipsize(gtk::pango::EllipsizeMode::End);
        words.set_visible(!line.is_empty());
        meta.append(&words);
        let has_formats = !formats.is_empty();
        for format in formats {
            let badge = gtk::Label::new(Some(format));
            badge.add_css_class("format-badge");
            badge.set_valign(gtk::Align::Center);
            meta.append(&badge);
        }
        meta.set_visible(!line.is_empty() || has_formats);
        text.append(&meta);

        if let Item::Playlist(playlist) = item
            && !playlist.description.trim().is_empty()
        {
            let description = Excerpt::new(playlist.description.trim(), DESCRIPTION_LINES);
            description.add_css_class("hero-description");
            text.append(&description);
        }

        // what is to be done sits at the foot of the artwork
        let filler = gtk::Box::new(gtk::Orientation::Vertical, 0);
        filler.set_vexpand(true);
        text.append(&filler);
        let (buttons, play, shuffle) = self.buttons(collection);
        text.append(&buttons);
        let hero = Hero::new(&cover, &text, PICTURE.0, PICTURE.1);
        Header { hero, title, cover, artworks, words, meta, play, shuffle }
    }

    /// Play and Shuffle; at the end of the row "Add to Library" (or the mark of what is in the
    /// library) and "More". With the buttons Play and Shuffle.
    fn buttons(self: &Rc<Self>, collection: &Collection) -> (gtk::Box, gtk::Button, gtk::Button) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("hero-actions");
        let play = detail::pill("am-media-playback-start-symbolic", "Play");
        let shuffle = detail::pill("am-media-playlist-shuffle-symbolic", "Shuffle");
        for (button, shuffled) in [(&play, false), (&shuffle, true)] {
            button.set_sensitive(has_songs(collection));
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.play(shuffled);
                }
            });
            row.append(button);
        }
        let space = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        space.set_hexpand(true);
        row.append(&space);

        let item = &collection.item;
        let (in_library, catalog_id, list) = match item {
            Item::Album(album) => (album.library_id.is_some(), album.catalog_id.clone(), library::Kind::Albums),
            Item::Playlist(playlist) => {
                (playlist.library_id.is_some(), playlist.catalog_id.clone(), library::Kind::Playlists)
            }
            _ => (true, None, library::Kind::Albums),
        };
        let added = ADDED.with(|added| added.borrow().contains(&item.key()))
            || (!in_library && catalog_id.as_deref().is_some_and(|id| library_has(list, id)));
        let add = detail::round("am-list-add-symbolic", "Add to Library");
        let mark = gtk::Image::from_icon_name("am-object-select-symbolic");
        mark.add_css_class("hero-mark");
        mark.set_tooltip_text(Some("In Your Library"));
        mark.update_property(&[gtk::accessible::Property::Label("In Your Library")]);
        mark.set_valign(gtk::Align::Center);
        add.set_visible(!in_library && !added && catalog_id.is_some());
        // an album of the library has the mark; a playlist of the library needs none
        mark.set_visible(added || (in_library && matches!(item, Item::Album(_))));
        let (weak, mark_shown) = (Rc::downgrade(self), mark.clone());
        add.connect_clicked(move |button| {
            if let Some(view) = weak.upgrade() {
                view.add(button, &mark_shown);
            }
        });
        if let Some(catalog_id) = catalog_id.filter(|_| add.is_visible()) {
            // added from somewhere else (a menu): marked once the library's list has it
            let mark = mark.downgrade();
            ctx().library.changed.connect(&add, move |add, changed| {
                if *changed == list && add.is_visible() && library_has(list, &catalog_id) {
                    add.set_visible(false);
                    if let Some(mark) = mark.upgrade() {
                        mark.set_visible(true);
                    }
                }
            });
        }
        row.append(&add);
        row.append(&mark);

        let more = detail::round("am-view-more-horizontal-symbolic", "More");
        let item = item.clone();
        more.connect_clicked(move |button| crate::menus::item_menu(&item, button, 0.0, button.height() as f64 + 4.0));
        row.append(&more);
        (row, play, shuffle)
    }
}

impl Header {
    /// Show what changes with the songs: their number, the mosaic of a playlist without artwork
    /// of its own, whether there is anything to play.
    fn follow(&mut self, collection: &Collection) {
        let (line, formats) = meta_of(collection);
        self.words.set_text(&line);
        self.words.set_visible(!line.is_empty());
        self.meta.set_visible(!line.is_empty() || !formats.is_empty());
        let artworks = artworks_of(collection);
        if artworks != self.artworks {
            self.cover.set_artworks(&artworks);
            self.artworks = artworks;
        }
        self.play.set_sensitive(has_songs(collection));
        self.shuffle.set_sensitive(has_songs(collection));
    }
}

/// What the cover of `collection` shows: its artwork, or for a playlist without one those of its
/// songs.
fn artworks_of(collection: &Collection) -> Vec<Artwork> {
    match &collection.item {
        Item::Playlist(Playlist { artwork: None, .. }) => {
            mosaic(collection.tracks.iter().map(|track| track.artwork.as_ref()))
        }
        item => item.artwork().cloned().into_iter().collect(),
    }
}

/// Whether there is anything to play (the songs of a long playlist may be on their way).
fn has_songs(collection: &Collection) -> bool {
    !collection.tracks.is_empty() || collection.next_offset.is_some()
}

/// Whether the library's list of albums or playlists has the one with this catalog id.
fn library_has(list: library::Kind, catalog_id: &str) -> bool {
    let store = ctx().library.store(list).clone();
    (0..store.n_items()).filter_map(|i| store.item(i).as_ref().and_then(item_of)).any(|item| match item {
        Item::Album(album) => album.catalog_id.as_deref() == Some(catalog_id),
        Item::Playlist(playlist) => playlist.catalog_id.as_deref() == Some(catalog_id),
        _ => false,
    })
}

/// The disc a row's song is on.
fn disc_of(object: &glib::Object) -> i64 {
    track_of(object).and_then(|track| track.disc_number).unwrap_or(1)
}

/// Whether the songs are on more than one disc.
fn several_discs(tracks: &[Track]) -> bool {
    let disc = |track: &Track| track.disc_number.unwrap_or(1);
    tracks.first().is_some_and(|first| tracks.iter().any(|track| disc(track) != disc(first)))
}

/// "Disc 2" above the songs of every disc.
fn disc_headings() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        let Some(header) = object.downcast_ref::<gtk::ListHeader>() else { return };
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.add_css_class("disc-heading");
        header.set_child(Some(&label));
    });
    factory.connect_bind(|_, object| {
        let Some(header) = object.downcast_ref::<gtk::ListHeader>() else { return };
        if let (Some(label), Some(first)) = (header.child().and_downcast::<gtk::Label>(), header.item()) {
            label.set_text(&format!("Disc {}", disc_of(&first)));
        }
    });
    factory
}

/// How many songs there are (a long playlist says so before they have all arrived).
fn song_count(collection: &Collection) -> usize {
    let listed = collection.tracks.len();
    if collection.next_offset.is_none() {
        return listed;
    }
    let stated = collection.total.or(match &collection.item {
        Item::Playlist(playlist) => playlist.track_count,
        _ => None,
    });
    stated.map(|count| count.max(0) as usize).unwrap_or(listed).max(listed)
}

/// The small line of the header and the format badges after it: "ALTERNATIVE · 2013" for an
/// album, "500 SONGS" for a playlist.
fn meta_of(collection: &Collection) -> (String, Vec<&'static str>) {
    match &collection.item {
        Item::Album(album) => (
            detail::meta_line(&[album.genre.clone(), album.release_date.as_deref().and_then(detail::year)]),
            detail::badges(&album.audio_traits),
        ),
        Item::Playlist(playlist) => {
            let count = song_count(collection);
            let updated = playlist
                .last_modified
                .as_deref()
                .filter(|_| !playlist.can_edit)
                .and_then(detail::long_date)
                .map(|date| format!("Updated {date}"));
            (detail::meta_line(&[(count > 0).then(|| detail::songs(count)), updated]), Vec::new())
        }
        _ => (String::new(), Vec::new()),
    }
}

/// The footer: when an album was released, how many songs there are and, once they are all
/// here, how long they play.
fn footer_text(collection: &Collection, complete: bool) -> String {
    let mut lines = Vec::new();
    if let Item::Album(album) = &collection.item
        && let Some(date) = album.release_date.as_deref().and_then(detail::long_date)
    {
        lines.push(date);
    }
    let count = song_count(collection);
    let seconds: f64 = collection.tracks.iter().filter_map(|track| track.duration_ms).sum::<i64>() as f64 / 1000.0;
    if count > 0 {
        lines.push(if complete && seconds >= 30.0 { total_time(count, seconds) } else { detail::songs(count) });
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::DemoMusic;
    use amui::types::Album;
    use std::time::Duration;

    fn demo() -> Arc<dyn Music> {
        Arc::new(DemoMusic::new(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")))
    }

    fn library(id: &str) -> Ref {
        Ref { catalog_id: None, library_id: Some(id.to_string()) }
    }

    /// What `fetch` reported: the songs of the first answer and the total it states (a long
    /// playlist), the sizes of the further pages, the result.
    type Reported = (Option<(usize, Option<i64>)>, Vec<usize>, Result<Collection, Problem>);

    type First = Box<dyn FnOnce(&Collection)>;
    type More = Box<dyn Fn(&[Track])>;
    type Done = Box<dyn FnOnce(Result<Collection, Problem>)>;

    /// Run a fetch to its end on a main loop of this thread; `start` begins it with the three
    /// callbacks of `fetch`.
    fn run(start: impl FnOnce(First, More, Done)) -> Reported {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let first = Rc::new(RefCell::new(None));
                let pages = Rc::new(RefCell::new(Vec::new()));
                let result = Rc::new(RefCell::new(None));
                let (first_seen, page_seen, done) = (first.clone(), pages.clone(), result.clone());
                start(
                    Box::new(move |collection| {
                        first_seen.replace(Some((collection.tracks.len(), collection.total)));
                    }),
                    Box::new(move |tracks| page_seen.borrow_mut().push(tracks.len())),
                    Box::new(move |outcome| {
                        done.replace(Some(outcome));
                    }),
                );
                while result.borrow().is_none() {
                    context.iteration(true);
                }
                (first.take(), pages.take(), result.take().unwrap())
            })
            .unwrap()
    }

    /// What the demonstration's music gives for `target`.
    fn fetched(kind: Kind, target: Ref) -> Reported {
        run(|first, more, done| fetch(demo(), kind, target, first, more, done))
    }

    /// The first answer of a playlist of `total` songs that goes on in pages.
    fn long_playlist(total: i64) -> Collection {
        Collection {
            item: Item::Playlist(Playlist::new(Some("p.long"), "Mix")),
            tracks: (0..100).map(numbered).collect(),
            next_offset: Some(100),
            total: Some(total),
        }
    }

    fn numbered(n: i64) -> Track {
        Track::new(Some(&n.to_string()), "Song", "Artist A")
    }

    #[test]
    fn the_pages_of_a_long_playlist_are_fetched_together() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // 730 songs: a first answer of 100 and seven more pages, each of which takes a while
        let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (now, peak) = (running.clone(), most.clone());
        let (first, pages, result) = run(move |first, more, done| {
            fetch_from(
                || Ok(long_playlist(730)),
                move |library_id, offset, limit| {
                    assert_eq!((library_id, limit), ("p.long", 100));
                    peak.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    // later pages answer sooner: they wait for their turn all the same
                    std::thread::sleep(Duration::from_millis(40 + (800 - offset as u64) / 4));
                    now.fetch_sub(1, Ordering::SeqCst);
                    let end = (offset + limit).min(730);
                    Ok(Page::new((offset..end).map(numbered).collect(), (end < 730).then_some(end), Some(730)))
                },
                first,
                more,
                done,
            )
        });
        assert_eq!(first, Some((100, Some(730))));
        assert_eq!(pages, [100, 100, 100, 100, 100, 100, 30]);
        let ids: Vec<i64> =
            result.unwrap().tracks.iter().filter_map(|track| track.catalog_id.as_ref()?.parse().ok()).collect();
        assert_eq!(ids, (0..730).collect::<Vec<_>>());
        let most = most.load(Ordering::SeqCst);
        assert!(most >= 3, "at most {most} requests ran at a time");
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_page_that_fails_is_a_problem() {
        let (first, pages, result) = run(|first, more, done| {
            fetch_from(
                || Ok(long_playlist(300)),
                |_, offset, _| match offset {
                    100 => Ok(Page::new((100..200).map(numbered).collect(), Some(200), Some(300))),
                    _ => Err(Problem::new("The service is not available")),
                },
                first,
                more,
                done,
            )
        });
        // what came before the page that failed was handed over
        assert_eq!((first, pages), (Some((100, Some(300))), vec![100]));
        assert_eq!(result.unwrap_err().message, "The service is not available");
    }

    #[test]
    fn a_long_playlist_arrives_whole_and_in_order() {
        let expected: Vec<String> =
            demo().playlist_tracks("p.0", 0, 10_000).unwrap().items.iter().map(Track::key).collect();
        assert_eq!(expected.len(), 500);
        let (first, pages, result) = fetched(Kind::Playlist, library("p.0"));
        // the first answer has 100 songs and says how many there are; four more pages follow
        assert_eq!(first, Some((100, Some(500))));
        assert_eq!(pages, [100, 100, 100, 100]);
        let collection = result.unwrap();
        assert_eq!(collection.next_offset, None);
        assert_eq!(collection.total, Some(500));
        let keys: Vec<String> = collection.tracks.iter().map(Track::key).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn an_album_and_a_short_playlist_are_one_answer() {
        let (first, pages, result) = fetched(Kind::Album, library("l.3"));
        assert_eq!((first, pages.len()), (None, 0));
        let album = result.unwrap();
        assert!(matches!(album.item, Item::Album(_)));
        assert!(!album.tracks.is_empty());
        let (first, pages, result) = fetched(Kind::Playlist, library("p.1"));
        assert_eq!((first, pages.len()), (None, 0));
        assert_eq!(result.unwrap().tracks.len(), 21);
        let (_, _, missing) = fetched(Kind::Album, Ref { catalog_id: Some("al999999".into()), library_id: None });
        assert!(missing.is_err());
    }

    fn song(disc: Option<i64>, seconds: i64) -> Track {
        Track { disc_number: disc, duration_ms: Some(seconds * 1000), ..Track::new(Some("1"), "Song A", "Artist A") }
    }

    #[test]
    fn discs_and_counts() {
        assert!(!several_discs(&[]));
        assert!(!several_discs(&[song(Some(1), 200), song(None, 200)]));
        assert!(several_discs(&[song(Some(1), 200), song(Some(2), 200)]));
        let playlist = |tracks: Vec<Track>, next_offset, total| Collection {
            item: Item::Playlist(Playlist { track_count: Some(480), ..Playlist::new(Some("p.1"), "Mix") }),
            tracks,
            next_offset,
            total,
        };
        // the first page of a long playlist: the number the service states
        assert_eq!(song_count(&playlist(vec![song(None, 200); 100], Some(100), Some(500))), 500);
        assert_eq!(song_count(&playlist(vec![song(None, 200); 100], Some(100), None)), 480);
        // everything is here: what is listed
        assert_eq!(song_count(&playlist(vec![song(None, 200); 3], None, Some(500))), 3);
    }

    #[test]
    fn header_and_footer_words() {
        let album = Collection {
            item: Item::Album(Album {
                genre: Some("Alternative".into()),
                release_date: Some("2013-05-17".into()),
                audio_traits: vec!["atmos".into(), "lossless".into()],
                ..Album::new(Some("1"), "Album A", "Artist A")
            }),
            tracks: vec![song(Some(1), 340); 13],
            next_offset: None,
            total: None,
        };
        assert_eq!(meta_of(&album), ("ALTERNATIVE · 2013".to_string(), vec!["Dolby Atmos", "Lossless"]));
        assert_eq!(footer_text(&album, true), "17 May 2013\n13 songs, 1 hour 14 minutes");
        let long = Collection {
            item: Item::Playlist(Playlist {
                last_modified: Some("2026-11-01T00:00:00Z".into()),
                ..Playlist::new(Some("p.0"), "Mix")
            }),
            tracks: vec![song(None, 180); 100],
            next_offset: Some(100),
            total: Some(500),
        };
        // while the songs arrive the footer says how many there are, not how long the first ones play
        assert_eq!(footer_text(&long, false), "500 songs");
        assert_eq!(meta_of(&long).0, "500 SONGS · UPDATED 1 NOVEMBER 2026");
        let whole = Collection { tracks: vec![song(None, 180); 500], next_offset: None, ..long };
        assert_eq!(footer_text(&whole, true), "500 songs, 25 hours");
        let empty = Collection { tracks: Vec::new(), total: Some(0), ..whole };
        assert_eq!(footer_text(&empty, true), "");
        assert_eq!(meta_of(&empty).0, "UPDATED 1 NOVEMBER 2026");
    }
}
