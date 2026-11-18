//! Search: the field of the sidebar, what it found, and the page that shows it.
//!
//! What the library holds for the term shows as it is typed: it is found in the library's lists
//! in memory (`finder`). Apple Music's catalog is searched a moment after the last key, and so is
//! the library through Apple while those lists may lack something (before they are loaded, while
//! they are loaded again): what Apple finds that they lack follows what they hold. The page shows
//! one of the two, at the user's choice; Apple Music's has what the library holds above its own
//! results, in a part that stays in place when they come. An Apple Music link pasted into the
//! field is opened instead (`links`). The term and what it found stay for the session, so going
//! back to the page finds them as they were.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use amui::types::{SearchResults, Track};
use gtk::{gdk, gio, glib, graphene};

use crate::actions;
use crate::app::ctx;
use crate::data::{Item, Music, Problem, Ref, Res};
use crate::finder::{Finder, Found};
use crate::library::{Library, boxed, track_of};
use crate::links::{self, Link};
use crate::menus::{self, TrackTarget};
use crate::nav::Dest;
use crate::pages::songs::track_list;
use crate::pages::{card_action, scrolled, shell};
use crate::signal::{Signal, background};
use crate::widgets::card::Card;
use crate::widgets::item_row::{ItemRow, MenuHandler};
use crate::widgets::thumb::Thumb;
use crate::widgets::track_row::RowStyle;

/// Apple is asked this long after the last key.
const TYPING_PAUSE_MS: u32 = 300;
/// A search that takes longer than this says so with a spinner.
const SPINNER_AFTER: Duration = Duration::from_millis(350);
/// How many albums, artists and playlists a scope asks for and shows at most.
const CARDS: usize = 12;
/// How many songs a scope shows.
const SONGS: usize = 8;
/// How many songs, and albums, artists and playlists, Apple Music's results show of the library
/// above their own: the songs, and one line of the others in a wide page.
const LIBRARY_SONGS: usize = 4;
const LIBRARY_ITEMS: usize = 3;
/// The width an album, artist or playlist of the library would like there, three side by side.
const LIBRARY_ITEM_WIDTH: i32 = 340;
/// Below this width of the page (pixels at the usual text size) the top result stands above the
/// songs, not beside them.
const NARROW: f64 = 940.0;
/// The artwork of the top result, and the width of its tile beside the songs.
const TOP_ART: i32 = 110;
const TOP_WIDTH: i32 = 320;

/// Where a search looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Apple Music's catalog.
    #[default]
    Catalog,
    Library,
}

impl Scope {
    pub const ALL: [Scope; 2] = [Scope::Catalog, Scope::Library];

    /// As the command line and the page's widgets call it.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Catalog => "apple-music",
            Scope::Library => "library",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Scope::Catalog => "Apple Music",
            Scope::Library => "Your Library",
        }
    }

    pub fn parse(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.name() == name)
    }
}

/// A request to one scope. Only the answer to the latest request of a scope is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

/// What a scope has to show.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    /// Nothing: no search, or one that has only just started.
    Nothing,
    /// The search is taking its time.
    Spinner,
    Results(Rc<SearchResults>),
    /// The search for this term found nothing.
    NoResults(String),
    Failed(Problem),
}

#[derive(Debug)]
struct Answer {
    /// The term that was searched for.
    term: String,
    result: Result<Rc<SearchResults>, Problem>,
}

#[derive(Debug, Default)]
struct Asked {
    /// The latest request; 0 when no answer is wanted.
    ticket: u64,
    /// Its answer is still to come...
    waiting: bool,
    /// ...and has been for long enough to say so.
    slow: bool,
    /// The last answer taken. It stays on the page until the next one arrives or the spinner
    /// takes its place.
    answer: Option<Answer>,
}

/// The term and the searches for it, without the machinery that runs them.
#[derive(Debug, Default)]
pub struct Model {
    term: String,
    /// What the library's lists in memory hold for the term (`finder`), found as it was typed.
    local: Rc<Found>,
    /// Those lists hold the whole library: Apple's search of the library is not asked.
    complete: bool,
    issued: u64,
    scopes: [Asked; 2],
}

fn is_empty(results: &SearchResults) -> bool {
    results.top.is_none()
        && results.songs.is_empty()
        && results.albums.is_empty()
        && results.artists.is_empty()
        && results.playlists.is_empty()
}

/// `first`, followed by what `more` has that `first` has not. A song, an album, an artist or a
/// playlist is the same when it has the same library id, or the same key.
fn merged(first: &SearchResults, more: &SearchResults) -> SearchResults {
    fn join<T: Clone>(first: &[T], more: &[T], ids: impl Fn(&T) -> (Option<&str>, String)) -> Vec<T> {
        let same = |a: &T, b: &T| {
            let ((a_id, a_key), (b_id, b_key)) = (ids(a), ids(b));
            (a_id.is_some() && a_id == b_id) || a_key == b_key
        };
        let mut all = first.to_vec();
        all.extend(more.iter().filter(|item| !first.iter().any(|shown| same(shown, item))).cloned());
        all
    }
    SearchResults {
        songs: join(&first.songs, &more.songs, |song| (song.library_id.as_deref(), song.key())),
        albums: join(&first.albums, &more.albums, |album| (album.library_id.as_deref(), album.key())),
        artists: join(&first.artists, &more.artists, |artist| (artist.library_id.as_deref(), artist.key())),
        playlists: join(&first.playlists, &more.playlists, |list| (list.library_id.as_deref(), list.key())),
        top: None,
    }
}

impl Model {
    /// Take a new term; false when it is the term already searched for (white space around it
    /// does not count). No answer to a request for the old term is wanted any more, and what the
    /// library's lists held was for the old term; an empty term also forgets what was found.
    pub fn set_term(&mut self, term: &str) -> bool {
        let term = term.trim();
        if term == self.term {
            return false;
        }
        self.term = term.to_string();
        self.local = Rc::default();
        for asked in &mut self.scopes {
            asked.ticket = 0;
            asked.waiting = false;
            asked.slow = false;
            if term.is_empty() {
                asked.answer = None;
            }
        }
        true
    }

    /// What the library's lists in memory hold for the term; false when they held that already.
    pub fn set_local(&mut self, found: Found) -> bool {
        if *self.local == found {
            return false;
        }
        self.local = Rc::new(found);
        true
    }

    /// What the library's lists in memory hold for the term.
    pub fn local(&self) -> Rc<Found> {
        self.local.clone()
    }

    /// Whether the library's lists in memory hold the whole library; false when that is known.
    pub fn set_complete(&mut self, complete: bool) -> bool {
        std::mem::replace(&mut self.complete, complete) != complete
    }

    /// Whether Apple is asked for `scope` at all: for its catalog always, for the library only
    /// while the lists in memory may lack something.
    pub fn needs(&self, scope: Scope) -> bool {
        scope == Scope::Catalog || !self.complete
    }

    /// Whether Apple is to be asked for `scope` now: it needs to be, and has neither a request for
    /// the term on its way nor an answer for it (a problem is asked again).
    pub fn wants(&self, scope: Scope) -> bool {
        let asked = &self.scopes[scope as usize];
        let answered = asked.answer.as_ref().is_some_and(|answer| answer.term == self.term && answer.result.is_ok());
        !self.term.is_empty() && self.needs(scope) && !asked.waiting && !answered
    }

    /// Start a request of `scope` for the current term.
    pub fn begin(&mut self, scope: Scope) -> Ticket {
        self.issued += 1;
        let asked = &mut self.scopes[scope as usize];
        asked.ticket = self.issued;
        asked.waiting = true;
        asked.slow = false;
        Ticket(self.issued)
    }

    /// The answer to a request. False when that request is not the latest of its scope any more
    /// (the term changed, or the scope was asked again): the answer is dropped.
    pub fn finish(&mut self, scope: Scope, ticket: Ticket, result: Res<SearchResults>) -> bool {
        let asked = &mut self.scopes[scope as usize];
        if asked.ticket != ticket.0 || !asked.waiting {
            return false;
        }
        asked.waiting = false;
        asked.slow = false;
        asked.answer = Some(Answer { term: self.term.clone(), result: result.map(Rc::new) });
        true
    }

    /// A request has taken long. True when its scope has to say so now: the request is still the
    /// latest and unanswered.
    pub fn overdue(&mut self, scope: Scope, ticket: Ticket) -> bool {
        let asked = &mut self.scopes[scope as usize];
        if asked.ticket != ticket.0 || !asked.waiting || asked.slow {
            return false;
        }
        asked.slow = true;
        true
    }

    pub fn shown(&self, scope: Scope) -> Shown {
        if scope == Scope::Library {
            return self.library_shown();
        }
        let asked = &self.scopes[scope as usize];
        if asked.waiting && asked.slow {
            return Shown::Spinner;
        }
        match &asked.answer {
            None => Shown::Nothing,
            Some(Answer { result: Err(problem), .. }) => Shown::Failed(problem.clone()),
            Some(Answer { term, result: Ok(results) }) if is_empty(results) => Shown::NoResults(term.clone()),
            Some(Answer { result: Ok(results), .. }) => Shown::Results(results.clone()),
        }
    }

    /// What the library holds for the term: what its lists in memory hold, at once, followed by
    /// what Apple's search of the library found that they lack.
    fn library_shown(&self) -> Shown {
        if self.term.is_empty() {
            return Shown::Nothing;
        }
        let asked = &self.scopes[Scope::Library as usize];
        let answer = asked.answer.as_ref().filter(|answer| answer.term == self.term);
        let results = match answer {
            Some(Answer { result: Ok(more), .. }) => Rc::new(merged(&self.local.results, more)),
            _ => self.local.results.clone(),
        };
        if !is_empty(&results) {
            return Shown::Results(results);
        }
        if self.complete {
            return Shown::NoResults(self.term.clone());
        }
        // what the lists may lack is for Apple to say; its last answer stays until the spinner
        match answer {
            _ if asked.waiting && asked.slow => Shown::Spinner,
            Some(Answer { result: Err(problem), .. }) => Shown::Failed(problem.clone()),
            Some(_) => Shown::NoResults(self.term.clone()),
            None => Shown::Nothing,
        }
    }
}

/// The search of the window: the term, what was found, and which of the two scopes is shown.
pub struct Search {
    model: RefCell<Model>,
    scope: Cell<Scope>,
    /// The library's lists, ready to be searched as the term is typed.
    finder: Rc<Finder>,
    /// The state of those lists (`Finder::generation`) when they were last searched.
    searched: Cell<u64>,
    /// What this scope has to show changed (both scopes say so when the term changes).
    pub changed: Signal<Scope>,
}

impl Search {
    /// A search of `library` and of Apple Music whose page shows `scope` until the user chooses
    /// the other one.
    pub fn new(scope: Scope, library: &Rc<Library>) -> Rc<Search> {
        let search = Rc::new(Search {
            model: RefCell::default(),
            scope: Cell::new(scope),
            finder: Finder::new(library),
            searched: Cell::new(0),
            changed: Signal::new(),
        });
        // what the lists hold for the term changes with them
        let weak = Rc::downgrade(&search);
        library.changed.connect_forever(move |_| {
            if let Some(search) = weak.upgrade() {
                search.library_changed();
            }
        });
        search
    }

    pub fn term(&self) -> String {
        self.model.borrow().term.clone()
    }

    /// The scope the user chose (for the session).
    pub fn scope(&self) -> Scope {
        self.scope.get()
    }

    pub fn set_scope(&self, scope: Scope) {
        self.scope.set(scope);
    }

    pub fn shown(&self, scope: Scope) -> Shown {
        self.model.borrow().shown(scope)
    }

    /// What the library's lists in memory hold for the term, found as it was typed.
    pub fn local(&self) -> Rc<Found> {
        self.model.borrow().local()
    }

    /// Take the term of the field as it is typed: what the library's lists hold for it is there
    /// at once. False when it is the term already there (white space around it does not count).
    pub fn set_term(&self, term: &str) -> bool {
        if !self.model.borrow_mut().set_term(term) {
            return false;
        }
        self.look_in_library();
        Scope::ALL.iter().for_each(|scope| self.changed.emit(scope));
        true
    }

    /// Ask Apple for the term, a moment after the last key: for its catalog, and for the library
    /// while the lists in memory may lack something. A scope that was asked for the term already
    /// is not asked again.
    pub fn search(self: &Rc<Self>, music: &Arc<dyn Music>) {
        for scope in Scope::ALL {
            let wanted = self.model.borrow().wants(scope);
            if wanted {
                self.ask(music, scope, true);
            }
        }
    }

    /// Ask one scope again for the current term (after a problem).
    pub fn retry(self: &Rc<Self>, music: &Arc<dyn Music>, scope: Scope) {
        if !self.term().is_empty() {
            self.ask(music, scope, true);
        }
    }

    /// Ask Apple again for the current term, for fresh data. What a scope has found stays in view
    /// until the new answer is there, however long that takes.
    pub fn refresh(self: &Rc<Self>, music: &Arc<dyn Music>) {
        if self.term().is_empty() {
            return;
        }
        for scope in Scope::ALL {
            let needed = self.model.borrow().needs(scope);
            if needed {
                let found = matches!(self.shown(scope), Shown::Results(_) | Shown::NoResults(_));
                self.ask(music, scope, !found);
            }
        }
    }

    /// Search the library's lists in memory for the term. True when something changed: what they
    /// hold for it, or whether they hold the whole library.
    fn look_in_library(&self) -> bool {
        let term = self.term();
        self.searched.set(self.finder.generation());
        let found = if term.is_empty() { Found::default() } else { self.finder.find(&term, CARDS) };
        let mut model = self.model.borrow_mut();
        let complete = model.set_complete(self.finder.complete());
        model.set_local(found) || complete
    }

    /// The library's lists changed, or started or stopped loading.
    fn library_changed(&self) {
        let changed = if self.finder.generation() != self.searched.get() {
            self.look_in_library()
        } else {
            self.model.borrow_mut().set_complete(self.finder.complete())
        };
        if changed && !self.term().is_empty() {
            Scope::ALL.iter().for_each(|scope| self.changed.emit(scope));
        }
    }

    /// Start a request of `scope`; `spinner`: say so when it takes long.
    fn ask(self: &Rc<Self>, music: &Arc<dyn Music>, scope: Scope, spinner: bool) {
        let (ticket, term) = {
            let mut model = self.model.borrow_mut();
            (model.begin(scope), model.term.clone())
        };
        self.changed.emit(&scope);
        let (music, search) = (music.clone(), self.clone());
        background(
            move || match scope {
                Scope::Catalog => music.search_catalog(&term, CARDS as i64),
                Scope::Library => music.search_library(&term, CARDS as i64),
            },
            move |answer| {
                let taken = search.model.borrow_mut().finish(scope, ticket, answer);
                if taken {
                    search.changed.emit(&scope);
                }
            },
        );
        if spinner {
            let search = self.clone();
            glib::timeout_add_local_once(SPINNER_AFTER, move || {
                let overdue = search.model.borrow_mut().overdue(scope, ticket);
                if overdue {
                    search.changed.emit(&scope);
                }
            });
        }
    }
}

// --- the field

/// Whether the keyboard is in the field.
fn has_keyboard(entry: &gtk::SearchEntry) -> bool {
    entry
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| focus.is_ancestor(entry) || &focus == entry.upcast_ref::<gtk::Widget>())
}

/// Take the keyboard out of the field.
fn leave(entry: &gtk::SearchEntry) {
    if let Some(root) = entry.root() {
        root.set_focus(None::<&gtk::Widget>);
    }
}

/// Open what an Apple Music link points at: the page of an album, a playlist or an artist; a
/// song plays.
fn open_link(link: &Link) {
    let ctx = ctx();
    let target = Ref { catalog_id: Some(link.id.clone()), library_id: None };
    match link.kind {
        links::Kind::Album => ctx.nav.go(Dest::Album(target)),
        links::Kind::Playlist => ctx.nav.go(Dest::Playlist(target)),
        links::Kind::Artist => ctx.nav.go(Dest::Artist(target)),
        links::Kind::Song => {
            let (music, id) = (ctx.music.clone(), link.id.clone());
            background(
                move || music.song(&id),
                |song| match song {
                    Ok(track) => actions::play_tracks(vec![track], 0),
                    Err(problem) => crate::app::ctx().toast.emit(&problem.message),
                },
            );
        }
    }
}

/// The text of the field changed: what the library holds for it shows at once.
fn edited(entry: &gtk::SearchEntry) {
    let ctx = ctx();
    let text = entry.text();
    // a link is opened once the typing pauses (`typed`), not searched for
    if links::parse_music_link(&text).is_some() {
        return;
    }
    if ctx.search.set_term(&text) && !ctx.search.term().is_empty() {
        // typing more on the page goes nowhere: it is the current destination already
        ctx.nav.go(Dest::Search);
    }
}

/// The typing paused (at once when the field was emptied), or Enter was pressed in the field:
/// Apple is asked for the term.
fn typed(entry: &gtk::SearchEntry, entered: bool) {
    let ctx = ctx();
    let text = entry.text();
    if let Some(link) = links::parse_music_link(&text) {
        open_link(&link);
        // emptying the field ends the search that was there, like any emptying
        entry.set_text("");
        return;
    }
    let changed = ctx.search.set_term(&text);
    ctx.search.search(&ctx.music);
    if (changed || entered) && !ctx.search.term().is_empty() {
        ctx.nav.go(Dest::Search);
    }
}

/// Connect the search field of the sidebar and the window to the search: typing and Enter, a
/// pasted link, the `win.search` action (Ctrl+F, see `shortcuts`), Escape and a click anywhere
/// else, and the `--search` option.
pub fn attach(window: &adw::ApplicationWindow, entry: &gtk::SearchEntry) {
    entry.connect_changed(edited);
    entry.set_search_delay(TYPING_PAUSE_MS);
    entry.connect_search_changed(|entry| typed(entry, false));
    entry.connect_activate(|entry| typed(entry, true));
    // Escape
    entry.connect_stop_search(leave);

    let action = gio::SimpleAction::new("search", None);
    let weak = entry.downgrade();
    action.connect_activate(move |_, _| {
        if let Some(entry) = weak.upgrade() {
            // the results appear where the Now Playing view is
            ctx().show_now_playing.emit(&false);
            entry.grab_focus();
            entry.select_region(0, -1);
        }
    });
    window.add_action(&action);

    // a click anywhere but in the field takes the keyboard out of it
    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = entry.downgrade();
    click.connect_pressed(move |gesture, _, x, y| {
        let (Some(entry), Some(window)) = (weak.upgrade(), gesture.widget()) else { return };
        // a click in a popover (the field's own menu is one) is not a click in the window
        let surface = window.native().and_then(|native| native.surface());
        if gesture.current_event().and_then(|event| event.surface()) != surface {
            return;
        }
        let inside = entry
            .compute_bounds(&window)
            .is_some_and(|bounds| bounds.contains_point(&graphene::Point::new(x as f32, y as f32)));
        if !inside && has_keyboard(&entry) {
            leave(&entry);
        }
    });
    window.add_controller(click);

    // `--search TERM`, as if it had been typed
    if let Some(term) = &ctx().options.search {
        entry.set_text(term);
        typed(entry, false);
    }
}

// --- the page

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("search-heading");
    label
}

/// What an item is, where items of every kind come together (the top result, the library's
/// part): "Artist", "Album · ARTIST", "Song · ARTIST", "Playlist", "Station".
fn kind_line(item: &Item) -> String {
    let by = |kind: &str, artist: &str| {
        if artist.is_empty() { kind.to_string() } else { format!("{kind} · {artist}") }
    };
    match item {
        Item::Artist(_) => "Artist".to_string(),
        Item::Album(album) => by("Album", &album.artist),
        Item::Track(track) => by("Song", &track.artist),
        Item::Playlist(_) => "Playlist".to_string(),
        Item::Station(_) => "Station".to_string(),
    }
}

/// A click on the top result: a song plays, among the listed songs when it is one of them;
/// anything else opens its page (a station plays).
fn open_top(top: &Item, listed: &gio::ListStore) {
    let Item::Track(track) = top else { return actions::open(top) };
    let songs: Vec<Track> = (0..listed.n_items()).filter_map(|i| listed.item(i).as_ref().and_then(track_of)).collect();
    match songs.iter().position(|song| song.key() == track.key()) {
        Some(index) => actions::play_tracks(songs, index),
        None => actions::play_tracks(vec![track.clone()], 0),
    }
}

/// The large tile of Apple's top result, under its heading.
struct Top {
    column: gtk::Box,
    tile: gtk::Button,
    art: Thumb,
    name: gtk::Label,
    kind: gtk::Label,
    result: Rc<RefCell<Option<Item>>>,
}

impl Top {
    fn new(listed: &gio::ListStore, narrow: &adw::Breakpoint) -> Top {
        let art = Thumb::new(TOP_ART, 8.0);
        art.set_shadow(true);
        art.set_halign(gtk::Align::Start);
        art.set_valign(gtk::Align::Center);
        // a long name is cut where the tile ends: it asks for no more than `chars` of width
        let line = |class: &str, chars: i32| {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(chars);
            label.add_css_class(class);
            label
        };
        let (name, kind) = (line("search-top-name", 22), line("dim-label", 30));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        text.append(&name);
        text.append(&kind);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
        content.append(&art);
        content.append(&text);
        // in a narrow page the tile is as wide as the page: the artwork stands beside the text
        narrow.add_setter(&content, "orientation", Some(&gtk::Orientation::Horizontal.to_value()));

        let result: Rc<RefCell<Option<Item>>> = Rc::default();
        let tile = gtk::Button::new();
        tile.add_css_class("search-top");
        tile.set_size_request(TOP_WIDTH, -1);
        tile.set_child(Some(&content));
        {
            let (result, listed) = (result.clone(), listed.clone());
            tile.connect_clicked(move |_| {
                let top = result.borrow().clone();
                if let Some(top) = top {
                    open_top(&top, &listed);
                }
            });
        }
        // its menu on a right click
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        {
            let result = result.clone();
            click.connect_pressed(move |gesture, _, x, y| {
                let (Some(top), Some(tile)) = (result.borrow().clone(), gesture.widget()) else { return };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                match &top {
                    Item::Track(track) => menus::track_menu(&TrackTarget::new(track), &tile, x, y),
                    other => menus::item_menu(other, &tile, x, y),
                }
            });
        }
        tile.add_controller(click);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("search-top-column");
        // beside the songs the tile keeps its width, also when there are no songs
        column.set_hexpand(false);
        column.set_valign(gtk::Align::Start);
        column.append(&heading("Top Result"));
        column.append(&tile);
        Top { column, tile, art, name, kind, result }
    }

    fn show(&self, top: Option<&Item>) {
        self.column.set_visible(top.is_some());
        self.result.replace(top.cloned());
        let Some(item) = top else { return self.art.set_artwork(None) };
        let line = kind_line(item);
        self.art.set_round(matches!(item, Item::Artist(_)));
        self.art.set_artwork(item.artwork());
        self.name.set_text(item.title());
        self.kind.set_text(&line);
        self.tile.set_tooltip_text(Some(item.title()));
        self.tile.update_property(&[gtk::accessible::Property::Label(&format!("{}, {line}", item.title()))]);
    }
}

/// Albums, artists or playlists under a heading: cards that wrap onto further lines.
struct Cards {
    section: gtk::Box,
    flow: gtk::FlowBox,
    cards: RefCell<Vec<Card>>,
}

impl Cards {
    fn new(title: &str) -> Cards {
        let flow = gtk::FlowBox::new();
        flow.add_css_class("search-cards");
        flow.set_homogeneous(true);
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_min_children_per_line(2);
        flow.set_max_children_per_line(14);
        flow.set_valign(gtk::Align::Start);
        // Enter on a focused card opens it
        flow.connect_child_activated(|_, cell| {
            if let Some(item) = cell.child().and_downcast::<Card>().and_then(|card| card.item()) {
                actions::open(&item);
            }
        });
        let section = gtk::Box::new(gtk::Orientation::Vertical, 0);
        section.append(&heading(title));
        section.append(&flow);
        Cards { section, flow, cards: RefCell::default() }
    }

    /// Show `items` in the cards that are there, with more cards when there are too few.
    fn show(&self, items: Vec<Item>) {
        let mut cards = self.cards.borrow_mut();
        while cards.len() < items.len() {
            let card = Card::new();
            card.set_handler(Rc::new(card_action));
            self.flow.insert(&card, -1);
            cards.push(card);
        }
        for (index, card) in cards.iter().enumerate() {
            let item = items.get(index);
            card.bind(item);
            if let Some(cell) = card.parent() {
                cell.set_visible(item.is_some());
            }
        }
        self.section.set_visible(!items.is_empty());
    }
}

/// What a search found, under headings: the songs (beside Apple Music's top result), then the
/// albums, the artists and the playlists.
struct Sections {
    body: gtk::Box,
    first: gtk::Box,
    top: Option<Top>,
    songs_column: gtk::Box,
    songs: gio::ListStore,
    albums: Cards,
    artists: Cards,
    playlists: Cards,
}

impl Sections {
    fn new(scope: Scope, narrow: &adw::Breakpoint) -> Sections {
        let songs = gio::ListStore::new::<glib::BoxedAnyObject>();
        let songs_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        songs_column.add_css_class("search-songs");
        songs_column.set_hexpand(true);
        songs_column.append(&heading("Songs"));
        songs_column.append(&track_list(&songs, RowStyle::LIST));
        // Apple names a top result for its catalog only
        let top = (scope == Scope::Catalog).then(|| Top::new(&songs, narrow));
        let first = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        first.add_css_class("search-first");
        if let Some(top) = &top {
            first.append(&top.column);
        }
        first.append(&songs_column);
        // in a narrow page the top result stands above the songs
        narrow.add_setter(&first, "orientation", Some(&gtk::Orientation::Vertical.to_value()));

        let (albums, artists, playlists) = (Cards::new("Albums"), Cards::new("Artists"), Cards::new("Playlists"));
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.add_css_class("search-results");
        for section in [&first, &albums.section, &artists.section, &playlists.section] {
            body.append(section);
        }
        Sections { body, first, top, songs_column, songs, albums, artists, playlists }
    }

    fn show(&self, results: &SearchResults) {
        if let Some(top) = &self.top {
            top.show(results.top.as_ref());
        }
        let rows: Vec<glib::BoxedAnyObject> = results.songs.iter().take(SONGS).cloned().map(boxed).collect();
        self.songs_column.set_visible(!rows.is_empty());
        self.first.set_visible(!rows.is_empty() || self.top.is_some() && results.top.is_some());
        self.songs.splice(0, self.songs.n_items(), &rows);
        self.albums.show(results.albums.iter().take(CARDS).cloned().map(Item::Album).collect());
        self.artists.show(results.artists.iter().take(CARDS).cloned().map(Item::Artist).collect());
        self.playlists.show(results.playlists.iter().take(CARDS).cloned().map(Item::Playlist).collect());
    }
}

/// The library's part of Apple Music's results, above them: what the library's lists hold for
/// the term, there as it is typed. Its songs, then its albums, artists and playlists in rows
/// that say what each is.
struct InLibrary {
    section: gtk::Box,
    songs: gio::ListStore,
    songs_list: gtk::ListView,
    items: gtk::FlowBox,
    rows: RefCell<Vec<ItemRow>>,
    all: gtk::Button,
}

impl InLibrary {
    /// `see_all` shows all that the library holds for the term.
    fn new(see_all: impl Fn() + 'static) -> InLibrary {
        let title = heading("In Your Library");
        title.set_hexpand(true);
        let all = gtk::Button::with_label("See All");
        all.add_css_class("flat");
        all.set_valign(gtk::Align::Center);
        all.connect_clicked(move |_| see_all());
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        top.add_css_class("search-library-top");
        top.append(&title);
        top.append(&all);

        let songs = gio::ListStore::new::<glib::BoxedAnyObject>();
        let songs_list = track_list(&songs, RowStyle::LIST);
        let items = gtk::FlowBox::new();
        items.add_css_class("search-items");
        items.set_homogeneous(true);
        items.set_selection_mode(gtk::SelectionMode::None);
        items.set_min_children_per_line(1);
        items.set_max_children_per_line(3);
        items.set_valign(gtk::Align::Start);
        // a click on a row, or Enter on a focused one, opens its item
        items.connect_child_activated(|_, cell| {
            if let Some(item) = cell.child().and_downcast::<ItemRow>().and_then(|row| row.item()) {
                actions::open(&item);
            }
        });
        let section = gtk::Box::new(gtk::Orientation::Vertical, 0);
        section.add_css_class("search-library");
        section.append(&top);
        section.append(&songs_list);
        section.append(&items);
        InLibrary { section, songs, songs_list, items, rows: RefCell::default(), all }
    }

    /// Show what the library holds; false when it holds nothing (the part is hidden then).
    fn show(&self, found: &Found) -> bool {
        let songs: Vec<glib::BoxedAnyObject> =
            found.results.songs.iter().take(LIBRARY_SONGS).cloned().map(boxed).collect();
        self.songs_list.set_visible(!songs.is_empty());
        self.songs.splice(0, self.songs.n_items(), &songs);
        let items = &found.items[..found.items.len().min(LIBRARY_ITEMS)];
        let mut rows = self.rows.borrow_mut();
        while rows.len() < items.len() {
            let menu: MenuHandler = Rc::new(menus::item_menu);
            let row = ItemRow::new(menu);
            row.set_natural_width(LIBRARY_ITEM_WIDTH);
            self.items.insert(&row, -1);
            rows.push(row);
        }
        for (index, row) in rows.iter().enumerate() {
            let item = items.get(index);
            row.bind(item);
            if let Some(item) = item {
                row.set_line(&kind_line(item));
            }
            if let Some(cell) = row.parent() {
                cell.set_visible(item.is_some());
            }
        }
        self.items.set_visible(!items.is_empty());
        self.all.set_visible(found.results.songs.len() > LIBRARY_SONGS || found.items.len() > LIBRARY_ITEMS);
        self.section.set_visible(!found.is_empty());
        !found.is_empty()
    }
}

/// What one scope found, with the states around it: nothing yet, a spinner, "No Results", a
/// problem. Apple Music's has the library's part above them: while that part shows something,
/// the states stand under it, and Apple's results come in below it without moving it.
fn results(scope: Scope, narrow: &adw::Breakpoint, see_all: impl Fn() + 'static) -> gtk::ScrolledWindow {
    let sections = Sections::new(scope, narrow);
    let in_library = (scope == Scope::Catalog).then(|| InLibrary::new(see_all));

    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    let empty = adw::StatusPage::new();
    let failed = adw::StatusPage::new();
    failed.set_icon_name(Some("am-network-error-symbolic"));
    failed.set_title(match scope {
        Scope::Catalog => "Couldn't Search Apple Music",
        Scope::Library => "Couldn't Search Your Library",
    });
    let again = gtk::Button::with_label("Try Again");
    again.add_css_class("pill");
    again.set_halign(gtk::Align::Center);
    again.connect_clicked(move |_| {
        let ctx = ctx();
        ctx.search.retry(&ctx.music, scope);
    });
    failed.set_child(Some(&again));

    let states = gtk::Stack::new();
    states.set_transition_type(gtk::StackTransitionType::Crossfade);
    states.set_transition_duration(120);
    // as tall as what it shows, or as the rest of the page when that is taller
    states.set_vhomogeneous(false);
    states.set_vexpand(true);
    states.add_named(&gtk::Box::new(gtk::Orientation::Vertical, 0), Some("nothing"));
    states.add_named(&sections.body, Some("results"));
    states.add_named(&spinner, Some("searching"));
    states.add_named(&empty, Some("empty"));
    states.add_named(&failed, Some("failed"));

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Some(part) = &in_library {
        column.append(&part.section);
    }
    column.append(&states);
    let viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    // the lists of songs are as tall as their rows only in their natural size
    viewport.set_vscroll_policy(gtk::ScrollablePolicy::Natural);
    viewport.set_child(Some(&column));
    let scroller = scrolled(&viewport);

    // what the sections show, and the term of what the page shows
    let (on_page, on_page_term) = (RefCell::new(None::<Rc<SearchResults>>), RefCell::new(String::new()));
    let show = move |scroller: &gtk::ScrolledWindow| {
        let search = ctx().search.clone();
        // the library's part is there at once, and stays when Apple's answer comes
        let under_library = in_library.as_ref().is_some_and(|part| part.show(&search.local()));
        let state = match search.shown(scope) {
            Shown::Nothing => "nothing",
            Shown::Spinner => "searching",
            Shown::Results(results) => {
                // a fresh answer that says the same changes nothing
                if on_page.borrow().as_ref() != Some(&results) {
                    sections.show(&results);
                    on_page.replace(Some(results));
                }
                "results"
            }
            Shown::NoResults(_) if under_library => {
                empty.set_title("No Results in Apple Music");
                empty.set_description(None);
                "empty"
            }
            Shown::NoResults(term) => {
                empty.set_title(&format!("No Results for “{term}”"));
                empty.set_description(Some("Check the spelling or try a new search."));
                "empty"
            }
            Shown::Failed(problem) => {
                failed.set_description(Some(&glib::markup_escape_text(&problem.message)));
                "failed"
            }
        };
        // under the library's part a problem or "No Results" takes little room
        for status in [&empty, &failed] {
            if under_library {
                status.add_css_class("compact");
            } else {
                status.remove_css_class("compact");
            }
        }
        states.set_visible_child_name(state);
        // what another term found starts at its top; fresh results for the same term leave the
        // page where the user is
        let term = search.term();
        if on_page_term.replace(term.clone()) != term {
            scroller.vadjustment().set_value(0.0);
        }
    };
    show(&scroller);
    ctx().search.changed.connect(&scroller, move |scroller, changed| {
        if *changed == scope {
            show(scroller);
        }
    });
    scroller
}

/// The prompt while there is no term, the results of the chosen scope otherwise.
fn show_prompt_or_results(content: &gtk::Stack) {
    content.set_visible_child_name(if ctx().search.term().is_empty() { "prompt" } else { "results" });
}

pub fn page(dest: &Dest) -> adw::NavigationPage {
    let search = ctx().search.clone();
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        NARROW,
        adw::LengthUnit::Sp,
    ));
    let choice = adw::ToggleGroup::new();
    for scope in Scope::ALL {
        choice.add(adw::Toggle::builder().name(scope.name()).label(scope.title()).build());
    }
    choice.set_active_name(Some(search.scope().name()));
    // "See All" of the library's part: the Your Library side
    let weak = choice.downgrade();
    let see_all = move || {
        if let Some(choice) = weak.upgrade() {
            choice.set_active_name(Some(Scope::Library.name()));
        }
    };
    let scopes = gtk::Stack::new();
    scopes.set_transition_type(gtk::StackTransitionType::Crossfade);
    scopes.set_transition_duration(120);
    for scope in Scope::ALL {
        scopes.add_named(&results(scope, &narrow, see_all.clone()), Some(scope.name()));
    }
    scopes.set_visible_child_name(search.scope().name());

    let prompt = adw::StatusPage::new();
    prompt.add_css_class("search-prompt");
    prompt.set_icon_name(Some("am-system-search-symbolic"));
    prompt.set_description(Some("Search Apple Music and your library"));
    let content = gtk::Stack::new();
    content.set_transition_type(gtk::StackTransitionType::Crossfade);
    content.set_transition_duration(120);
    content.add_named(&prompt, Some("prompt"));
    content.add_named(&scopes, Some("results"));
    show_prompt_or_results(&content);
    search.changed.connect(&content, |content, _| show_prompt_or_results(content));
    ctx().refreshed.connect(&content, |_, _| {
        let ctx = ctx();
        ctx.search.refresh(&ctx.music);
    });

    choice.connect_active_name_notify(move |choice| {
        if let Some(scope) = choice.active_name().and_then(|name| Scope::parse(&name)) {
            ctx().search.set_scope(scope);
            scopes.set_visible_child_name(scope.name());
        }
    });

    let layout = adw::BreakpointBin::new();
    layout.set_size_request(360, 240);
    layout.set_child(Some(&content));
    layout.add_breakpoint(narrow);
    let shell = shell(dest.title(), &layout);
    shell.header.set_title_widget(Some(&choice));
    shell.page
}

#[cfg(test)]
mod tests {
    use amui::types::{Album, Artist, Playlist, Station};

    use super::*;

    /// What a search found: one song of that name.
    fn found(song: &str) -> SearchResults {
        SearchResults { songs: vec![Track::new(Some("1000302"), song, "Glass Orbit")], ..SearchResults::default() }
    }

    /// The names of the songs a scope shows.
    fn songs(model: &Model, scope: Scope) -> Vec<String> {
        match model.shown(scope) {
            Shown::Results(results) => results.songs.iter().map(|song| song.name.clone()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn an_answer_for_an_older_term_is_dropped() {
        let mut model = Model::default();
        assert!(model.set_term("amber"));
        let older = model.begin(Scope::Catalog);
        assert!(model.set_term("amber harbor"));
        // the answer for "amber" arrives when the term has changed, before the new search starts
        assert!(!model.finish(Scope::Catalog, older, Ok(found("Amber"))));
        let newer = model.begin(Scope::Catalog);
        // ...and after it has started
        assert!(!model.finish(Scope::Catalog, older, Ok(found("Amber"))));
        assert_eq!(model.shown(Scope::Catalog), Shown::Nothing);
        assert!(model.finish(Scope::Catalog, newer, Ok(found("Amber Harbor"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Amber Harbor"]);
        // ...and after the answer for the new term is there
        assert!(!model.finish(Scope::Catalog, older, Ok(found("Amber"))));
        assert!(!model.finish(Scope::Catalog, older, Err(Problem::new("No connection"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Amber Harbor"]);
    }

    #[test]
    fn the_scopes_are_asked_and_answered_apart() {
        let mut model = Model::default();
        model.set_term("velvet");
        let catalog = model.begin(Scope::Catalog);
        let library = model.begin(Scope::Library);
        // a request is of one scope
        assert!(!model.finish(Scope::Library, catalog, Ok(found("Velvet"))));
        assert!(!model.finish(Scope::Catalog, library, Ok(found("Velvet"))));
        // the library answers first
        assert!(model.finish(Scope::Library, library, Ok(found("Velvet in the Tide"))));
        assert_eq!(songs(&model, Scope::Library), ["Velvet in the Tide"]);
        assert_eq!(model.shown(Scope::Catalog), Shown::Nothing);
        assert!(model.finish(Scope::Catalog, catalog, Ok(found("Velvet Orbit"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Velvet Orbit"]);
        assert_eq!(songs(&model, Scope::Library), ["Velvet in the Tide"]);
        // an answer is taken once
        assert!(!model.finish(Scope::Catalog, catalog, Ok(found("Velvet"))));
    }

    #[test]
    fn the_same_term_is_not_searched_again() {
        let mut model = Model::default();
        assert!(!model.set_term(""));
        assert!(!model.set_term("   "));
        assert!(model.set_term("neon"));
        assert!(!model.set_term("neon"));
        assert!(!model.set_term("  neon "));
        assert!(model.set_term("neon tide"));
        assert_eq!(model.term, "neon tide");
    }

    #[test]
    fn emptying_the_term_forgets_what_was_found_and_what_was_asked() {
        let mut model = Model::default();
        model.set_term("harbor");
        let catalog = model.begin(Scope::Catalog);
        let library = model.begin(Scope::Library);
        assert!(model.finish(Scope::Library, library, Ok(found("Harbor"))));
        assert!(model.set_term(" "));
        assert_eq!(model.shown(Scope::Library), Shown::Nothing);
        assert!(!model.finish(Scope::Catalog, catalog, Ok(found("Harbor"))));
        assert!(!model.overdue(Scope::Catalog, catalog));
        assert_eq!(model.shown(Scope::Catalog), Shown::Nothing);
    }

    #[test]
    fn what_was_found_stays_until_the_next_answer_or_the_spinner() {
        let mut model = Model::default();
        model.set_term("amber");
        let first = model.begin(Scope::Catalog);
        model.finish(Scope::Catalog, first, Ok(found("Amber")));
        model.set_term("amber tide");
        let second = model.begin(Scope::Catalog);
        assert_eq!(songs(&model, Scope::Catalog), ["Amber"]);
        // a quick answer takes its place at once
        assert!(model.finish(Scope::Catalog, second, Ok(found("Amber Tide"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Amber Tide"]);
        assert!(!model.overdue(Scope::Catalog, second));
        // a slow one shows the spinner in between
        model.set_term("amber tide drift");
        let third = model.begin(Scope::Catalog);
        assert!(!model.overdue(Scope::Catalog, second));
        assert_eq!(songs(&model, Scope::Catalog), ["Amber Tide"]);
        assert!(model.overdue(Scope::Catalog, third));
        assert!(!model.overdue(Scope::Catalog, third));
        assert_eq!(model.shown(Scope::Catalog), Shown::Spinner);
        assert!(model.finish(Scope::Catalog, third, Ok(SearchResults::default())));
        assert_eq!(model.shown(Scope::Catalog), Shown::NoResults("amber tide drift".to_string()));
    }

    #[test]
    fn asking_a_scope_again_leaves_the_other_alone() {
        let mut model = Model::default();
        model.set_term("signal");
        let catalog = model.begin(Scope::Catalog);
        let library = model.begin(Scope::Library);
        let problem = Problem::new("No connection");
        assert!(model.finish(Scope::Catalog, catalog, Err(problem.clone())));
        assert_eq!(model.shown(Scope::Catalog), Shown::Failed(problem.clone()));
        // "Try Again": the problem stays until the spinner or the answer
        let again = model.begin(Scope::Catalog);
        assert_eq!(model.shown(Scope::Catalog), Shown::Failed(problem));
        assert!(model.overdue(Scope::Catalog, again));
        assert_eq!(model.shown(Scope::Catalog), Shown::Spinner);
        // the library's answer is still the one wanted
        assert!(model.finish(Scope::Library, library, Ok(found("Signal"))));
        assert_eq!(songs(&model, Scope::Library), ["Signal"]);
        assert!(model.finish(Scope::Catalog, again, Ok(found("Signal Drift"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Signal Drift"]);
        // asked twice in a row, the first answer is not wanted
        let one = model.begin(Scope::Library);
        let two = model.begin(Scope::Library);
        assert!(!model.finish(Scope::Library, one, Ok(found("Signal One"))));
        assert!(model.finish(Scope::Library, two, Ok(found("Signal Two"))));
        assert_eq!(songs(&model, Scope::Library), ["Signal Two"]);
    }

    /// What the library's lists hold: songs of these names (with the library ids "i.NAME").
    fn in_lists(names: &[&str]) -> Found {
        let songs = names
            .iter()
            .map(|name| Track { library_id: Some(format!("i.{name}")), ..Track::new(None, *name, "Glass Orbit") })
            .collect();
        Found { results: Rc::new(SearchResults { songs, ..SearchResults::default() }), items: Vec::new() }
    }

    /// What Apple's search of the library found: the same songs, as the lists have them.
    fn from_apple(names: &[&str]) -> SearchResults {
        (*in_lists(names).results).clone()
    }

    #[test]
    fn the_library_answers_at_once_from_its_lists() {
        let mut model = Model::default();
        assert!(model.set_complete(true));
        assert!(!model.set_complete(true));
        assert!(model.set_term("amber"));
        // no request: what the lists hold is the answer
        assert!(model.set_local(in_lists(&["Amber", "Amber Harbor"])));
        assert_eq!(songs(&model, Scope::Library), ["Amber", "Amber Harbor"]);
        assert!(!model.set_local(in_lists(&["Amber", "Amber Harbor"])));
        // the lists hold the whole library: Apple is asked for its catalog only
        assert!(model.wants(Scope::Catalog));
        assert!(!model.wants(Scope::Library));
        assert!(!model.needs(Scope::Library));
        // a new term: what the lists held was for the old one
        assert!(model.set_term("amber h"));
        assert_eq!(model.shown(Scope::Library), Shown::NoResults("amber h".to_string()));
        model.set_local(in_lists(&["Amber Harbor"]));
        assert_eq!(songs(&model, Scope::Library), ["Amber Harbor"]);
        // the catalog's own answer is not the library's
        let catalog = model.begin(Scope::Catalog);
        assert!(model.finish(Scope::Catalog, catalog, Ok(found("Amber Harbor (Live)"))));
        assert_eq!(songs(&model, Scope::Catalog), ["Amber Harbor (Live)"]);
        assert_eq!(songs(&model, Scope::Library), ["Amber Harbor"]);
        // an empty term forgets them
        model.set_term("");
        assert_eq!(model.shown(Scope::Library), Shown::Nothing);
        assert!(!model.set_local(Found::default()));
    }

    #[test]
    fn apple_adds_to_the_lists_what_they_lack_while_they_may_lack_something() {
        let mut model = Model::default();
        model.set_term("amber");
        model.set_local(in_lists(&["Amber", "Amber Harbor"]));
        // the lists are loading: Apple is asked for the library too, and the lists answer first
        assert!(model.wants(Scope::Library));
        let library = model.begin(Scope::Library);
        assert!(!model.wants(Scope::Library));
        assert!(model.overdue(Scope::Library, library));
        assert_eq!(songs(&model, Scope::Library), ["Amber", "Amber Harbor"]);
        // what Apple found that the lists hold already is not shown twice
        let mut answer = from_apple(&["Amber Harbor", "Amber Tide", "Amber"]);
        answer.albums = vec![amui::types::Album::new(Some("al1"), "Amber Days", "Glass Orbit")];
        assert!(model.finish(Scope::Library, library, Ok(answer)));
        assert_eq!(songs(&model, Scope::Library), ["Amber", "Amber Harbor", "Amber Tide"]);
        let Shown::Results(results) = model.shown(Scope::Library) else { panic!("results") };
        assert_eq!(results.albums.len(), 1);
        // answered: not asked again for the term
        assert!(!model.wants(Scope::Library));
        // what Apple found for an older term is not added to what the lists hold for this one
        model.set_term("amber h");
        model.set_local(in_lists(&["Amber Harbor"]));
        assert_eq!(songs(&model, Scope::Library), ["Amber Harbor"]);
        assert!(model.wants(Scope::Library));
    }

    #[test]
    fn without_the_lists_apple_says_what_the_library_holds() {
        let mut model = Model::default();
        model.set_term("velvet");
        // nothing in the lists, which may lack something: nothing to say before Apple answers
        assert_eq!(model.shown(Scope::Library), Shown::Nothing);
        let first = model.begin(Scope::Library);
        assert_eq!(model.shown(Scope::Library), Shown::Nothing);
        assert!(model.overdue(Scope::Library, first));
        assert_eq!(model.shown(Scope::Library), Shown::Spinner);
        let problem = Problem::new("No connection");
        assert!(model.finish(Scope::Library, first, Err(problem.clone())));
        assert_eq!(model.shown(Scope::Library), Shown::Failed(problem.clone()));
        // a problem is asked again; it stays until the spinner or the answer
        assert!(model.wants(Scope::Library));
        let again = model.begin(Scope::Library);
        assert_eq!(model.shown(Scope::Library), Shown::Failed(problem));
        assert!(model.finish(Scope::Library, again, Ok(SearchResults::default())));
        assert_eq!(model.shown(Scope::Library), Shown::NoResults("velvet".to_string()));
        // once the lists hold the whole library, they say so themselves
        model.set_term("velvet tide");
        assert_eq!(model.shown(Scope::Library), Shown::Nothing);
        model.set_complete(true);
        assert_eq!(model.shown(Scope::Library), Shown::NoResults("velvet tide".to_string()));
        assert!(!model.wants(Scope::Library));
    }

    #[test]
    fn a_scope_is_asked_once_for_a_term() {
        let mut model = Model::default();
        // nothing is asked for no term
        assert!(!model.wants(Scope::Catalog) && !model.wants(Scope::Library));
        model.set_term("signal");
        let catalog = model.begin(Scope::Catalog);
        // on its way: not asked again (Enter after the typing paused)
        assert!(!model.wants(Scope::Catalog));
        assert!(model.finish(Scope::Catalog, catalog, Ok(found("Signal"))));
        assert!(!model.wants(Scope::Catalog));
        // typed on and back before the typing paused: the answer is the one for the term
        model.set_term("signals");
        assert!(model.wants(Scope::Catalog));
        model.set_term("signal");
        assert!(!model.wants(Scope::Catalog));
        assert_eq!(songs(&model, Scope::Catalog), ["Signal"]);
        // the term typed on, then asked: the answer for "signal" is not this term's
        model.set_term("signal drift");
        let drift = model.begin(Scope::Catalog);
        model.finish(Scope::Catalog, drift, Ok(found("Signal Drift")));
        model.set_term("signal");
        assert!(model.wants(Scope::Catalog));
    }

    #[test]
    fn what_apple_finds_follows_what_the_lists_hold() {
        let album = |id: Option<&str>, name: &str| amui::types::Album {
            library_id: id.map(str::to_owned),
            ..amui::types::Album::new(None, name, "Glass Orbit")
        };
        let first = SearchResults {
            songs: from_apple(&["Amber"]).songs,
            albums: vec![album(Some("l.1"), "Amber Days")],
            ..SearchResults::default()
        };
        let more = SearchResults {
            songs: from_apple(&["Amber Tide", "Amber"]).songs,
            albums: vec![album(Some("l.1"), "Amber Days (renamed)"), album(Some("l.2"), "Amber Nights")],
            artists: vec![Artist::new(Some("ar1"), "Amber Lanterns")],
            playlists: vec![Playlist::new(Some("p.1"), "Amber Mix")],
            top: Some(Item::Artist(Artist::new(Some("ar1"), "Amber Lanterns"))),
        };
        let all = merged(&first, &more);
        let names = |songs: &[Track]| songs.iter().map(|song| song.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&all.songs), ["Amber", "Amber Tide"]);
        let albums: Vec<&str> = all.albums.iter().map(|album| album.name.as_str()).collect();
        assert_eq!(albums, ["Amber Days", "Amber Nights"]);
        assert_eq!((all.artists.len(), all.playlists.len(), all.top), (1, 1, None));
        assert_eq!(merged(&first, &SearchResults::default()), first);
    }

    #[test]
    fn what_an_item_is() {
        let album = Album::new(Some("al3"), "Halo Fever Neon", "Static Meadow");
        assert_eq!(kind_line(&Item::Album(album)), "Album · Static Meadow");
        assert_eq!(kind_line(&Item::Album(Album::new(Some("al3"), "Halo Fever Neon", ""))), "Album");
        let song = Track::new(Some("1000302"), "Amber Harbor", "Glass Orbit");
        assert_eq!(kind_line(&Item::Track(song)), "Song · Glass Orbit");
        assert_eq!(kind_line(&Item::Artist(Artist::new(Some("ar7"), "Glass Orbit"))), "Artist");
        assert_eq!(kind_line(&Item::Playlist(Playlist::new(None, "Harbor Radar"))), "Playlist");
        let station = Station { id: "ra.7".into(), name: "Tide Station".into(), ..Station::default() };
        assert_eq!(kind_line(&Item::Station(station)), "Station");
    }

    #[test]
    fn scopes_by_name() {
        assert_eq!(Scope::parse("apple-music"), Some(Scope::Catalog));
        assert_eq!(Scope::parse("library"), Some(Scope::Library));
        assert_eq!(Scope::parse("radio"), None);
        assert_eq!(Scope::default(), Scope::Catalog);
    }
}
