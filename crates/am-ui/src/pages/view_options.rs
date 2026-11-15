//! The library's pages of albums, artists, playlists and songs, in the order and the view the
//! user chose.
//!
//! "View Options" at the end of the header bar offers "Sort By" (the orders that apply to the
//! page) and "View As" (a grid or a list). What was chosen is remembered per page in the settings
//! (`sort.albums`, `view.albums`...); `--sort` and `--view` choose it for a screenshot.
//!
//! The rows stay in the library's store, which fills in pages while the page is shown. A
//! `gtk::SortListModel` over the store puts them in order with a `gtk::StringSorter` that
//! compares one key per row (`sorting`), made once when the model first needs it and compared
//! byte by byte. It is there from the start (`prepare`), so that the pages that arrive are merged
//! in as they come, whether the page is shown or not. A new order is sorted by a second model
//! that works a millisecond at a time between frames, and takes the place of the first once it
//! is complete: the window never waits for a sort, and never shows a list half sorted.
//!
//! "Go to Song" (`show_song`) leads to the Songs page with the song's row in the middle of the
//! page, selected and lit up for a moment. A song that has not arrived yet is waited for while
//! the list loads, and its row is kept in the middle as more rows arrive until the list is
//! complete or the user moves it; when the list is complete without it, nothing more happens.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use amui::types::Track;
use gtk::{gdk, gio, glib};

use crate::app::ctx;
use crate::data::Item;
use crate::library::{Kind, find_song, item_of, track_of};
use crate::nav::Dest;
use crate::pages::grid::{card_grid, empty_texts, with_states};
use crate::pages::songs::{Place, track_list_in};
use crate::pages::{card_action, scrolled, shell};
use crate::sorting::{self, Order};
use crate::widgets::card::CardHandler;
use crate::widgets::item_row::{ItemRow, MenuHandler};
use crate::widgets::track_row::{RowStyle, TrackRow};

/// How a page shows its albums, artists or playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Grid,
    List,
}

impl View {
    /// The name on the command line, in the settings and in the menu.
    pub fn name(self) -> &'static str {
        match self {
            View::Grid => "grid",
            View::List => "list",
        }
    }

    pub fn parse(name: &str) -> Option<View> {
        [View::Grid, View::List].into_iter().find(|view| view.name() == name)
    }
}

/// What the page of a list of the library offers.
struct Choices {
    /// The page in the settings: `sort.NAME`, `view.NAME`.
    name: &'static str,
    /// The orders of "Sort By" as the menu lists them; the first is the one a page starts in.
    /// None: the page is in the order of the names.
    orders: &'static [Order],
    /// "View As": a grid or a list. A page without it is a list.
    views: bool,
}

impl Choices {
    fn of(kind: Kind) -> Option<Choices> {
        use Order::*;
        Some(match kind {
            Kind::Albums => {
                Choices { name: "albums", orders: &[Title, Artist, RecentlyAdded, ReleaseDate], views: true }
            }
            Kind::Artists => Choices { name: "artists", orders: &[], views: true },
            Kind::Playlists => Choices { name: "playlists", orders: &[Title, RecentlyAdded], views: true },
            Kind::Songs => Choices { name: "songs", orders: &[Title, Artist, Album, RecentlyAdded], views: false },
            // newest first, and nothing else
            Kind::Recent => return None,
        })
    }

    fn offers(&self, order: Order) -> bool {
        self.orders.contains(&order)
    }

    /// The name of a choice in the settings.
    fn setting(&self, what: &str) -> String {
        format!("{what}.{}", self.name)
    }

    /// The order the page starts in: `--sort`, else the one chosen last time, else the first.
    fn first_order(&self) -> Order {
        let ctx = ctx();
        let given = ctx.options.sort.filter(|order| self.offers(*order));
        let saved = || Order::parse(&ctx.settings.text(&self.setting("sort"), "")).filter(|order| self.offers(*order));
        given.or_else(saved).or_else(|| self.orders.first().copied()).unwrap_or(Order::Title)
    }

    /// The view the page starts in: `--view`, else the one chosen last time, else the grid.
    fn first_view(&self) -> View {
        if !self.views {
            return View::List;
        }
        let ctx = ctx();
        let saved = || View::parse(&ctx.settings.text(&self.setting("view"), ""));
        ctx.options.view.or_else(saved).unwrap_or(View::Grid)
    }
}

/// The key of a row of the library's lists in `order`: a song, or an album, artist or playlist.
fn row_key(row: &glib::Object, order: Order) -> String {
    let Some(boxed) = row.downcast_ref::<glib::BoxedAnyObject>() else { return String::new() };
    if let Ok(track) = boxed.try_borrow::<Track>() {
        return sorting::track_key(&track, order);
    }
    boxed.try_borrow::<Item>().map(|item| sorting::item_key(&item, order)).unwrap_or_default()
}

/// The rows of the list of `kind` in `order`: sorted at once, or a millisecond at a time between
/// frames (`incremental`).
fn sort_model(kind: Kind, order: Order, incremental: bool) -> gtk::SortListModel {
    // the songs come the most recently added first, and they do not say when they were added:
    // that order is the store's own
    if kind == Kind::Songs && order == Order::RecentlyAdded {
        let model = gtk::SortListModel::new(None::<gio::ListModel>, None::<gtk::Sorter>);
        model.set_incremental(incremental);
        model.set_model(Some(ctx().library.store(kind)));
        return model;
    }
    let key = gtk::ClosureExpression::with_callback(Vec::<gtk::Expression>::new(), move |values: &[glib::Value]| {
        let row = values.first().and_then(|row| row.get::<glib::Object>().ok());
        row.map(|row| row_key(&row, order)).unwrap_or_default()
    });
    let sorter = gtk::StringSorter::new(Some(&key));
    // the keys are made to be compared byte by byte, as they are
    sorter.set_ignore_case(false);
    sorter.set_collation(gtk::Collation::None);
    let model = gtk::SortListModel::new(None::<gio::ListModel>, Some(sorter));
    model.set_incremental(incremental);
    model.set_model(Some(ctx().library.store(kind)));
    model
}

/// The model of a new order while it is sorted, and what follows once it is shown.
type Next = (gtk::SortListModel, Box<dyn FnOnce()>);

/// A list of the library in the order of its page.
struct Sorted {
    kind: Kind,
    /// The order it is in, or is being sorted into.
    order: Cell<Order>,
    /// The sorted model that is shown, alone in this list.
    slot: gio::ListStore,
    /// What the views show: the model in `slot`.
    shown: gtk::FlattenListModel,
    next: RefCell<Option<Next>>,
}

impl Sorted {
    fn new(kind: Kind, order: Order) -> Rc<Sorted> {
        let slot = gio::ListStore::new::<gtk::SortListModel>();
        slot.append(&sort_model(kind, order, false));
        let shown = gtk::FlattenListModel::new(Some(slot.clone()));
        Rc::new(Sorted { kind, order: Cell::new(order), slot, shown, next: RefCell::default() })
    }

    /// Sort the list into `order` in the background and show it so once it is complete; `done`
    /// runs then. An order asked for later takes the place of one that is not complete.
    fn sort(self: &Rc<Self>, order: Order, done: impl FnOnce() + 'static) {
        self.order.set(order);
        let next = sort_model(self.kind, order, true);
        let weak = Rc::downgrade(self);
        next.connect_pending_notify(move |next| {
            if next.pending() == 0 {
                // after the model has told its views what it changed last
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(sorted) = weak.upgrade() {
                        sorted.complete();
                    }
                });
            }
        });
        self.next.replace(Some((next, Box::new(done))));
        // an empty list is sorted already
        self.complete();
    }

    /// Show the new order if it is sorted.
    fn complete(&self) {
        if !self.next.borrow().as_ref().is_some_and(|(next, _)| next.pending() == 0) {
            return;
        }
        let Some((next, done)) = self.next.take() else { return };
        // the pages that arrive from now on are merged in at once
        next.set_incremental(false);
        self.slot.splice(0, self.slot.n_items(), &[next]);
        done();
    }
}

thread_local! {
    /// The lists of the library in the orders of their pages, one per kind.
    static SORTED: RefCell<HashMap<Kind, Rc<Sorted>>> = RefCell::default();
    /// The Songs page while it lives: where "Go to Song" leads.
    static SONGS: RefCell<Weak<Page>> = const { RefCell::new(Weak::new()) };
    /// The song "Go to Song" leads to, until its row is in the middle of the page for good.
    static SOUGHT: RefCell<Option<Sought>> = const { RefCell::new(None) };
    /// A look for it is due once the list has taken in what changed.
    static SEEK_DUE: Cell<bool> = const { Cell::new(false) };
}

/// The song "Go to Song" leads to.
#[derive(Debug, Clone)]
struct Sought {
    track: Track,
    /// Its row was shown while the list was still loading: it is kept in the middle of the page
    /// as more rows arrive, until the list is complete or the user moves the list.
    shown: bool,
}

/// The list of `kind`, in the order its page starts in; made when it is first asked for.
fn sorted(kind: Kind, choices: &Choices) -> Rc<Sorted> {
    SORTED.with(|lists| {
        lists.borrow_mut().entry(kind).or_insert_with(|| Sorted::new(kind, choices.first_order())).clone()
    })
}

/// Put the library's lists in order from the start, as they load: a page that is opened later
/// finds its list sorted, and does not sort it all at once then.
pub fn prepare() {
    for kind in [Kind::Albums, Kind::Artists, Kind::Playlists, Kind::Songs] {
        if let Some(choices) = Choices::of(kind) {
            sorted(kind, &choices);
        }
    }
}

/// A list view of the items of `model` (rows carrying `data::Item`), one `ItemRow` each: a click
/// or Enter opens an item, as with a card, and the menu is the cards'.
pub fn item_list(model: &impl IsA<gio::ListModel>) -> gtk::ListView {
    let menu: MenuHandler = Rc::new(crate::menus::item_menu);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        if let Some(cell) = object.downcast_ref::<gtk::ListItem>() {
            cell.set_child(Some(&ItemRow::new(menu.clone())));
        }
    });
    factory.connect_bind(|_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        if let Some(row) = cell.child().and_downcast::<ItemRow>() {
            row.bind(cell.item().as_ref().and_then(item_of).as_ref());
        }
    });
    factory.connect_unbind(|_, object| {
        let row = object.downcast_ref::<gtk::ListItem>().and_then(|cell| cell.child()).and_downcast::<ItemRow>();
        if let Some(row) = row {
            row.bind(None);
        }
    });
    let view = gtk::ListView::new(Some(gtk::NoSelection::new(Some(model.clone()))), Some(factory));
    view.add_css_class("items");
    view.set_single_click_activate(true);
    view.connect_activate(|view, position| {
        if let Some(item) = view.model().and_then(|model| model.item(position)).as_ref().and_then(item_of) {
            crate::actions::open(&item);
        }
    });
    view
}

/// The selection of a list or grid view.
fn selection(view: &gtk::Widget) -> Option<gtk::SelectionModel> {
    if let Some(list) = view.downcast_ref::<gtk::ListView>() {
        return list.model();
    }
    view.downcast_ref::<gtk::GridView>().and_then(|grid| grid.model())
}

/// Bring the first item of a list or grid view into sight (`flags`: and the keyboard onto it).
fn scroll_to_first(view: &gtk::Widget, flags: gtk::ListScrollFlags) {
    if selection(view).is_none_or(|model| model.n_items() == 0) {
        return;
    }
    if let Some(list) = view.downcast_ref::<gtk::ListView>() {
        list.scroll_to(0, flags, None);
    } else if let Some(grid) = view.downcast_ref::<gtk::GridView>() {
        grid.scroll_to(0, flags, None);
    }
}

/// Keep a view that is at its top there while rows arrive. The list fills in pages, and their
/// rows go wherever the order puts them; when some go above the first row shown, a list view
/// keeps that row where it is, and so moves away from the top.
fn stay_at_top(view: &gtk::Widget, scroller: &gtk::ScrolledWindow) {
    let Some(model) = selection(view) else { return };
    let (view, adjustment) = (view.downgrade(), scroller.vadjustment());
    // after the view's own handler: it has moved to the row it keeps
    model.connect_items_changed(move |_, _, _, _| {
        if let Some(view) = view.upgrade()
            && adjustment.value() < 1.0
        {
            scroll_to_first(&view, gtk::ListScrollFlags::NONE);
        }
    });
}

/// One of the pages while it lives.
struct Page {
    kind: Kind,
    choices: Choices,
    sorted: Rc<Sorted>,
    /// The views made so far, named after their `View`; the songs have their list only.
    views: gtk::Stack,
    button: gtk::MenuButton,
}

impl Page {
    /// The scrolled view of `view`, made the first time it is asked for.
    fn view(&self, view: View) -> gtk::ScrolledWindow {
        if let Some(scroller) = self.views.child_by_name(view.name()).and_downcast::<gtk::ScrolledWindow>() {
            return scroller;
        }
        let content: gtk::Widget = match (self.kind, view) {
            (Kind::Songs, _) => {
                let place = Place { songs_page: true, ..Place::default() };
                track_list_in(&self.sorted.shown, RowStyle::LIST, place).upcast()
            }
            (_, View::Grid) => {
                let handler: CardHandler = Rc::new(card_action);
                card_grid(&self.sorted.shown, handler).upcast()
            }
            (_, View::List) => item_list(&self.sorted.shown).upcast(),
        };
        let scroller = scrolled(&content);
        stay_at_top(&content, &scroller);
        self.views.add_named(&scroller, Some(view.name()));
        scroller
    }

    /// Whether the keyboard is in the content of the page (on a card or a row).
    fn has_focus(&self) -> bool {
        let focus = self.views.root().and_then(|root| root.focus());
        focus.is_some_and(|widget| widget.is_ancestor(&self.views))
    }

    /// The view that is shown, back at its top; with `focus`, the keyboard on its first item.
    fn to_top(&self, focus: bool) {
        let Some(scroller) = self.views.visible_child().and_downcast::<gtk::ScrolledWindow>() else { return };
        scroller.vadjustment().set_value(0.0);
        let Some(view) = scroller.child() else { return };
        if focus {
            // the view moves the keyboard by itself only when it has it already
            scroll_to_first(&view, gtk::ListScrollFlags::FOCUS);
            view.grab_focus();
        } else {
            scroll_to_first(&view, gtk::ListScrollFlags::NONE);
        }
    }

    /// Show the items as `view`, from the top; the keyboard follows when it was in the other.
    fn show_view(&self, view: View) {
        let focus = self.has_focus();
        let scroller = self.view(view);
        self.views.set_visible_child(&scroller);
        self.button.set_icon_name(match (self.choices.views, view) {
            (false, _) => "view-sort-descending-symbolic",
            (true, View::Grid) => "view-grid-symbolic",
            (true, View::List) => "view-list-symbolic",
        });
        self.to_top(focus);
    }

    /// Put the items in `order`; the page shows its top once they are.
    fn sort_by(self: &Rc<Self>, order: Order) {
        let (page, focus) = (Rc::downgrade(self), self.has_focus());
        self.sorted.sort(order, move || {
            if let Some(page) = page.upgrade() {
                page.to_top(focus);
            }
        });
    }

    /// Whether every row of the list is there and in its order: nothing more is on its way.
    fn complete(&self) -> bool {
        !ctx().library.is_loading(self.kind) && self.sorted.next.borrow().is_none()
    }

    /// Bring the row at `position` (the song `track`) to the middle of the page once the list
    /// has laid it out. `first`: the row is also selected, gets the keyboard and lights up.
    fn reveal(&self, position: u32, track: &Track, first: bool) {
        let Some(scroller) = self.views.visible_child().and_downcast::<gtk::ScrolledWindow>() else { return };
        let Some(list) = scroller.child().and_downcast::<gtk::ListView>() else { return };
        let flags =
            if first { gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT } else { gtk::ListScrollFlags::NONE };
        list.scroll_to(position, flags, None);
        let (scroller, track, frames) = (scroller.downgrade(), track.clone(), Cell::new(0));
        list.add_tick_callback(move |list, _| {
            let Some(scroller) = scroller.upgrade() else { return glib::ControlFlow::Break };
            // the frame after `scroll_to` has placed the row: where it is now can be read
            frames.set(frames.get() + 1);
            let row = (frames.get() > 1).then(|| shown_row(list, &track)).flatten();
            let Some(row) = row.filter(|_| scroller.height() > 0) else {
                return if frames.get() < 60 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break };
            };
            // the middle of what the scrolled window shows (the list's own height leaves out its
            // padding at the foot, where rows show too)
            if let Some(bounds) = row.compute_bounds(&scroller) {
                let adjustment = scroller.vadjustment();
                let middle = (bounds.y() + bounds.height() / 2.0) as f64;
                let end = (adjustment.upper() - adjustment.page_size()).max(0.0);
                adjustment.set_value((adjustment.value() + middle - scroller.height() as f64 / 2.0).clamp(0.0, end));
            }
            if first {
                row.flash();
            }
            glib::ControlFlow::Break
        });
    }
}

/// The row of `list` that shows `track`, laid out where the list has it.
fn shown_row(list: &gtk::ListView, track: &Track) -> Option<TrackRow> {
    let mut child = list.first_child();
    while let Some(cell) = child {
        child = cell.next_sibling();
        if let Some(row) = cell.first_child().and_downcast::<TrackRow>()
            && row.is_mapped()
            && row.height() > 0
            && row.track().is_some_and(|shown| shown.library_id == track.library_id && shown.key() == track.key())
        {
            return Some(row);
        }
    }
    None
}

/// "Go to Song": the Songs page with `track` (a song of any list) in the middle of it, selected
/// and lit up for a moment, once it is there.
pub fn show_song(track: &Track) {
    ctx().nav.go(Dest::Songs);
    SOUGHT.replace(Some(Sought { track: track.clone(), shown: false }));
    seek();
}

/// Look for the song "Go to Song" leads to in the Songs page's list, and bring it into view.
fn seek() {
    SEEK_DUE.set(false);
    let (Some(page), Some(sought)) = (SONGS.with_borrow(Weak::upgrade), SOUGHT.with_borrow(Clone::clone)) else {
        return;
    };
    let complete = page.complete();
    match find_song(&page.sorted.shown, &sought.track) {
        Some(position) => {
            // the library's own copy of the song: the row shows that one
            let listed = page.sorted.shown.item(position).as_ref().and_then(track_of);
            page.reveal(position, listed.as_ref().unwrap_or(&sought.track), !sought.shown);
            SOUGHT.replace((!complete).then_some(Sought { shown: true, ..sought }));
        }
        // the list is complete without it: nothing to show
        None if complete => {
            SOUGHT.take();
        }
        // it may be among the rows that are on their way
        None => {}
    }
}

/// Look again once the list has taken in a change (rows arrived, a new order).
fn seek_soon() {
    if SOUGHT.with_borrow(Option::is_some) && !SEEK_DUE.replace(true) {
        glib::idle_add_local_once(seek);
    }
}

/// Let the Songs page answer "Go to Song": where it leads, and what makes it look again.
fn follow_sought(page: &Rc<Page>, scroller: &gtk::ScrolledWindow) {
    SONGS.replace(Rc::downgrade(page));
    let ctx = ctx();
    page.sorted.shown.connect_items_changed(|_, _, _, _| seek_soon());
    // the list is complete: the last look
    ctx.library.changed.connect(&page.views, |_, kind| {
        if *kind == Kind::Songs {
            seek_soon();
        }
    });
    // on another page the song is no longer sought
    ctx.nav.changed.connect(&page.views, |_, dest| {
        if *dest != Dest::Songs {
            SOUGHT.take();
        }
    });
    // the user moves the list: its row stays where it is
    let input = gtk::EventControllerLegacy::new();
    input.set_propagation_phase(gtk::PropagationPhase::Capture);
    input.connect_event(|_, event| {
        let moves = matches!(
            event.event_type(),
            gdk::EventType::Scroll | gdk::EventType::ButtonPress | gdk::EventType::KeyPress
        );
        if moves && SOUGHT.with_borrow(|sought| sought.as_ref().is_some_and(|sought| sought.shown)) {
            SOUGHT.take();
        }
        glib::Propagation::Proceed
    });
    scroller.add_controller(input);
}

/// "View Options": "Sort By" and "View As", as `choices` has them.
fn menu_button(choices: &Choices) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    if !choices.orders.is_empty() {
        let orders = gio::Menu::new();
        for order in choices.orders {
            orders.append(Some(order.label()), Some(&format!("view.sort::{}", order.name())));
        }
        menu.append_section(Some("Sort By"), &orders);
    }
    if choices.views {
        let views = gio::Menu::new();
        views.append(Some("Grid"), Some("view.view::grid"));
        views.append(Some("List"), Some("view.view::list"));
        menu.append_section(Some("View As"), &views);
    }
    gtk::MenuButton::builder().menu_model(&menu).tooltip_text("View Options").build()
}

/// The actions of the menu: `view.sort` and `view.view`, whose states are what is chosen.
fn actions(page: &Rc<Page>, order: Order, view: View) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    let chosen = |action: &gio::SimpleAction| action.state().and_then(|state| state.str().map(str::to_owned));
    if !page.choices.orders.is_empty() {
        let sort = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &order.name().to_variant());
        let page = page.clone();
        sort.connect_change_state(move |action, value| {
            let wanted = value.and_then(|value| value.str()).and_then(Order::parse);
            let Some(order) = wanted.filter(|order| page.choices.offers(*order)) else { return };
            if chosen(action).as_deref() != Some(order.name()) {
                action.set_state(&order.name().to_variant());
                ctx().settings.set(&page.choices.setting("sort"), order.name());
                page.sort_by(order);
            }
        });
        group.add_action(&sort);
    }
    if page.choices.views {
        let shown = gio::SimpleAction::new_stateful("view", Some(glib::VariantTy::STRING), &view.name().to_variant());
        let page = page.clone();
        shown.connect_change_state(move |action, value| {
            let Some(view) = value.and_then(|value| value.str()).and_then(View::parse) else { return };
            if chosen(action).as_deref() != Some(view.name()) {
                action.set_state(&view.name().to_variant());
                ctx().settings.set(&page.choices.setting("view"), view.name());
                page.show_view(view);
            }
        });
        group.add_action(&shown);
    }
    group
}

/// `--show view-options`: the menu of the page the window starts on opens by itself (for a
/// screenshot).
fn open_when_asked(dest: &Dest, button: &gtk::MenuButton) {
    let ctx = ctx();
    if ctx.options.show.as_deref() != Some("view-options") || ctx.options.page.as_ref() != Some(dest) {
        return;
    }
    let opened = Cell::new(false);
    button.connect_map(move |button| {
        if opened.replace(true) {
            return;
        }
        let button = button.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if let Some(button) = button.upgrade() {
                button.popup();
            }
        });
    });
}

/// The page of the library's albums, artists, playlists or songs.
pub fn page(dest: &Dest, kind: Kind) -> adw::NavigationPage {
    let Some(choices) = Choices::of(kind) else { return crate::pages::grid::library_page(dest, kind) };
    let (sorted, view) = (sorted(kind, &choices), choices.first_view());
    let order = sorted.order.get();
    let button = menu_button(&choices);
    let page = Rc::new(Page { kind, sorted, choices, views: gtk::Stack::new(), button });
    page.show_view(view);
    if kind == Kind::Songs {
        follow_sought(&page, &page.view(View::List));
    }
    let (title, text) = empty_texts(kind);
    let shell = shell(dest.title(), &with_states(kind, &page.views, title, text));
    shell.header.pack_end(&page.button);
    shell.page.insert_action_group("view", Some(&actions(&page, order, view)));
    open_when_asked(dest, &page.button);
    shell.page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_page_offers() {
        let orders = |kind: Kind| Choices::of(kind).map(|choices| choices.orders.to_vec());
        use Order::*;
        assert_eq!(orders(Kind::Albums), Some(vec![Title, Artist, RecentlyAdded, ReleaseDate]));
        assert_eq!(orders(Kind::Songs), Some(vec![Title, Artist, Album, RecentlyAdded]));
        assert_eq!(orders(Kind::Playlists), Some(vec![Title, RecentlyAdded]));
        assert_eq!(orders(Kind::Artists), Some(vec![]));
        assert_eq!(orders(Kind::Recent), None);
        let views = |kind: Kind| Choices::of(kind).is_some_and(|choices| choices.views);
        assert!(views(Kind::Albums) && views(Kind::Artists) && views(Kind::Playlists) && !views(Kind::Songs));
        assert_eq!(Choices::of(Kind::Albums).unwrap().setting("sort"), "sort.albums");
        for view in [View::Grid, View::List] {
            assert_eq!(View::parse(view.name()), Some(view));
        }
        assert_eq!(View::parse("Grid"), None);
    }
}
