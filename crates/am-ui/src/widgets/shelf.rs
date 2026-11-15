//! A shelf: a heading and under it one row of items that scrolls sideways (Home, New, Radio).
//!
//! The row is a row of cards, of large cards under a line that says why each is there (the row
//! that leads a page; it may have no heading), or, for songs, of columns of four compact rows.
//! It moves with the touchpad (two fingers sideways) and with two round arrow buttons that show
//! at its edges while the pointer is over the shelf; each press moves it by about what is in
//! view. The row runs from one edge of the page to the other; at rest its first item is in line
//! with the heading.
//!
//! The items are built with the shelf. Their artwork is asked for when they come near what is in
//! view, across the row and down the page, and let go of when they are far from it: an item
//! outside that window is kept unmapped. Songs take a width and a height from their fonts, so
//! the row's geometry and the arrows' place are worked out from the items as they are laid out.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use amui::types::Track;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use crate::data::{self, Item, ShelfLayout};
use crate::menus::TrackTarget;
use crate::scroll::{SmoothScroll, Tuning};
use crate::widgets::card::{self, Card, CardAction, CardHandler};
use crate::widgets::song_cell::{self, SongAction, SongCell, SongHandler};

/// From the edge of the page to the first item: where the grids of the library begin, too.
pub const SIDE: i32 = 30;
const CARD: i32 = card::NATURAL_WIDTH;
/// A card of the row that leads a page.
const LARGE_CARD: i32 = 240;
/// The line over a large card, and the room between the two.
const NOTE: i32 = 18;
const NOTE_GAP: i32 = 6;
const GAP: i32 = 20;
const COLUMN_GAP: i32 = 14;
/// Songs per column.
const ROWS: usize = 4;
/// Room above and below the items, for the shadow of the artwork.
const ABOVE: i32 = 8;
const BELOW: i32 = 6;
/// From the edge of the page to an arrow button.
const ARROW_INSET: i32 = 8;

/// The geometry of a row: `count` items of equal width, a gap between them, and a margin before
/// the first and after the last.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Strip {
    pub margin: f64,
    pub item: f64,
    pub gap: f64,
    pub count: usize,
}

impl Strip {
    fn stride(&self) -> f64 {
        self.item + self.gap
    }

    /// The left edge of the item at `index`.
    fn start(&self, index: usize) -> f64 {
        self.margin + index as f64 * self.stride()
    }

    /// The width of the whole row.
    pub fn width(&self) -> f64 {
        if self.count == 0 { 0.0 } else { 2.0 * self.margin + self.count as f64 * self.stride() - self.gap }
    }

    /// How far the row scrolls when `page` of it is in view.
    pub fn end(&self, page: f64) -> f64 {
        (self.width() - page).max(0.0)
    }

    /// Where the "next" arrow moves a row that is at `value` with `page` of it in view: the first
    /// item that is cut off at the right edge, or beyond it, comes to the front, in line with
    /// the heading.
    pub fn next_page(&self, value: f64, page: f64) -> f64 {
        let stride = self.stride();
        let cut = (0..self.count).find(|index| self.start(*index) + self.item > value + page + 0.5);
        let mut to = match cut {
            Some(index) => index as f64 * stride,
            None => self.end(page),
        };
        if to < value + 1.0 {
            // a view narrower than one item still moves on
            to = ((value / stride).floor() + 1.0) * stride;
        }
        to.min(self.end(page))
    }

    /// Where the "previous" arrow moves it: the last item that is cut off at the left edge, or
    /// before it, comes into view at the right, and an item is in line with the heading again.
    pub fn previous_page(&self, value: f64, page: f64) -> f64 {
        let stride = self.stride();
        let cut = (0..self.count).rev().find(|index| self.start(*index) < value - 0.5);
        let mut to = match cut {
            Some(index) => ((self.start(index) + self.item - page) / stride).ceil() * stride,
            None => 0.0,
        };
        if to > value - 1.0 {
            to = ((value / stride).ceil() - 1.0) * stride;
        }
        to.clamp(0.0, self.end(page))
    }

    /// The items to keep ready when the row shows `page` of itself, is at `value` and on its way
    /// to `target`: what is in view now, what will be on arrival (an arrow button brings in a
    /// whole page), and one item more on either side.
    pub fn window(&self, value: f64, target: f64, page: f64) -> Range<usize> {
        if page <= 0.0 {
            return 0..0;
        }
        let stride = self.stride();
        let from = value.min(target) - stride;
        let to = value.max(target) + page + stride;
        let first = ((from - self.margin - self.item) / stride).floor() + 1.0;
        let last = ((to - self.margin) / stride).ceil();
        let last = (last.max(0.0) as usize).min(self.count);
        (first.max(0.0) as usize).min(last)..last
    }
}

/// How `count` songs fill columns of `rows`: the places in the shelf of each column's songs.
pub fn columns(count: usize, rows: usize) -> Vec<Range<usize>> {
    let rows = rows.max(1);
    (0..count).step_by(rows).map(|first| first..(first + rows).min(count)).collect()
}

mod imp {
    use super::*;

    pub struct Shelf {
        pub heading: gtk::Label,
        pub overlay: gtk::Overlay,
        pub scroller: gtk::ScrolledWindow,
        pub row: gtk::Box,
        pub back: gtk::Button,
        pub forward: gtk::Button,
        pub scroll: RefCell<Option<SmoothScroll>>,
        pub strip: Cell<Strip>,
        /// What the row holds, in order: cards, or columns of songs.
        pub items: RefCell<Vec<gtk::Widget>>,
        /// The pointer is over the shelf.
        pub pointer: Cell<bool>,
        /// The shelf is in view or near it on its page.
        pub near: Cell<bool>,
        /// The page the shelf scrolls on, listened to while the shelf is mapped.
        pub page: RefCell<Option<(gtk::ScrolledWindow, [glib::SignalHandlerId; 2])>>,
    }

    impl Default for Shelf {
        fn default() -> Self {
            let arrow = |icon: &str, name: &str| {
                let button = gtk::Button::from_icon_name(icon);
                button.add_css_class("circular");
                button.add_css_class("shelf-arrow");
                button.set_valign(gtk::Align::Start);
                button.set_focusable(false);
                button.set_focus_on_click(false);
                button.set_can_target(false);
                button.update_property(&[gtk::accessible::Property::Label(name)]);
                button
            };
            Shelf {
                heading: gtk::Label::builder().accessible_role(gtk::AccessibleRole::Heading).build(),
                overlay: gtk::Overlay::new(),
                scroller: gtk::ScrolledWindow::new(),
                row: gtk::Box::new(gtk::Orientation::Horizontal, GAP),
                back: arrow("go-previous-symbolic", "Previous"),
                forward: arrow("go-next-symbolic", "Next"),
                scroll: RefCell::default(),
                strip: Cell::default(),
                items: RefCell::default(),
                pointer: Cell::new(false),
                near: Cell::new(false),
                page: RefCell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Shelf {
        const NAME: &'static str = "AmShelf";
        type Type = super::Shelf;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("shelf");
            class.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for Shelf {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.heading.add_css_class("shelf-title");
            self.heading.set_xalign(0.0);
            self.heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.heading.set_single_line_mode(true);
            self.heading.set_margin_start(SIDE);
            self.heading.set_margin_end(SIDE);
            self.heading.set_parent(&*obj);

            self.row.set_margin_top(ABOVE);
            self.row.set_margin_bottom(BELOW);
            self.row.set_valign(gtk::Align::Start);
            self.scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
            self.scroller.set_child(Some(&self.row));
            self.overlay.set_child(Some(&self.scroller));
            self.overlay.add_overlay(&self.back);
            self.overlay.add_overlay(&self.forward);
            self.overlay.set_parent(&*obj);
            let weak = obj.downgrade();
            self.overlay.connect_get_child_position(move |overlay, arrow| weak.upgrade()?.arrow_place(overlay, arrow));

            let weak = obj.downgrade();
            self.back.connect_clicked(move |_| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.turn_page(false);
                }
            });
            let weak = obj.downgrade();
            self.forward.connect_clicked(move |_| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.turn_page(true);
                }
            });

            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, _, _| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.set_pointer_inside(true);
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.set_pointer_inside(false);
                }
            });
            obj.add_controller(motion);

            let adjustment = self.scroller.hadjustment();
            let weak = obj.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.update_arrows();
                    shelf.update_window();
                }
            });
            let weak = obj.downgrade();
            adjustment.connect_changed(move |_| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.update_arrows();
                    shelf.update_window();
                }
            });
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Shelf {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (heading_minimum, heading_natural, _, _) = self.heading.measure(orientation, -1);
            let (row_minimum, row_natural, _, _) = self.overlay.measure(orientation, -1);
            if orientation == gtk::Orientation::Horizontal {
                (heading_minimum.max(row_minimum), heading_natural.max(row_natural), -1, -1)
            } else {
                (heading_minimum + row_minimum, heading_natural + row_natural, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            // the items are as wide as their fonts make them (a column of songs)
            if let Some(first) = self.items.borrow().first() {
                let (_, natural, _, _) = first.measure(gtk::Orientation::Horizontal, -1);
                let strip = self.strip.get();
                if natural > 0 && strip.item != natural as f64 {
                    self.strip.set(Strip { item: natural as f64, ..strip });
                }
            }
            // a shelf without a title has no heading
            let mut heading = 0;
            if self.heading.is_visible() {
                let _ = self.heading.measure(gtk::Orientation::Horizontal, -1);
                heading = self.heading.measure(gtk::Orientation::Vertical, -1).1.min(height);
                self.heading.size_allocate(&gtk::Allocation::new(0, 0, width, heading), -1);
            }
            let _ = self.overlay.measure(gtk::Orientation::Horizontal, -1);
            let _ = self.overlay.measure(gtk::Orientation::Vertical, -1);
            self.overlay.size_allocate(&gtk::Allocation::new(0, heading, width, (height - heading).max(1)), -1);
            // the shelf has its place on the page now
            self.obj().update_near();
        }

        fn map(&self) {
            self.parent_map();
            let obj = self.obj();
            // follow the page up and down, to know when the shelf comes near what is in view
            let page = obj.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>();
            if let Some(page) = page {
                let adjustment = page.vadjustment();
                let weak = obj.downgrade();
                let moved = adjustment.connect_value_changed(move |_| {
                    if let Some(shelf) = weak.upgrade() {
                        shelf.update_near();
                    }
                });
                let weak = obj.downgrade();
                let resized = adjustment.connect_changed(move |_| {
                    if let Some(shelf) = weak.upgrade() {
                        shelf.update_near();
                    }
                });
                self.page.replace(Some((page, [moved, resized])));
            }
            obj.update_near();
        }

        fn unmap(&self) {
            if let Some((page, handlers)) = self.page.take() {
                let adjustment = page.vadjustment();
                for handler in handlers {
                    adjustment.disconnect(handler);
                }
            }
            // out of sight: the items let go of their artwork
            if self.near.replace(false) {
                self.obj().update_window();
            }
            self.parent_unmap();
        }

        /// Tab goes from shelf to shelf (the arrow keys move within one), and comes in at the
        /// first item that is in view.
        fn focus(&self, direction: gtk::DirectionType) -> bool {
            let obj = self.obj();
            if !matches!(direction, gtk::DirectionType::TabForward | gtk::DirectionType::TabBackward) {
                return self.parent_focus(direction);
            }
            if obj.focus_child().is_some() {
                return false;
            }
            let value = self.scroller.hadjustment().value();
            let strip = self.strip.get();
            let items = self.items.borrow();
            let in_view = (0..items.len()).find(|index| strip.start(*index) >= value - 0.5);
            match in_view.and_then(|index| items.get(index)) {
                Some(item) => item.child_focus(direction),
                None => self.parent_focus(direction),
            }
        }
    }
}

glib::wrapper! {
    pub struct Shelf(ObjectSubclass<imp::Shelf>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

thread_local! {
    /// The shelves that came near what their page shows and have yet to map their items.
    static ARRIVED: RefCell<Vec<glib::WeakRef<Shelf>>> = const { RefCell::new(Vec::new()) };
}

impl Shelf {
    /// The shelf of `shelf`; `on_card` is told what the user does with its cards. Songs play
    /// from here, with the songs of the shelf as the queue.
    pub fn new(shelf: &data::Shelf, on_card: CardHandler) -> Shelf {
        let widget: Shelf = glib::Object::new();
        let imp = widget.imp();
        // the row that leads a page comes without a title
        imp.heading.set_text(&shelf.title);
        imp.heading.set_visible(!shelf.title.is_empty());
        if !shelf.title.is_empty() {
            widget.update_property(&[gtk::accessible::Property::Label(&shelf.title)]);
        }

        let songs: Vec<Track> = shelf
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Track(track) => Some(track.clone()),
                _ => None,
            })
            .collect();
        let cards = |width: i32, items: Vec<gtk::Widget>| {
            let strip = Strip { margin: SIDE as f64, item: width as f64, gap: GAP as f64, count: items.len() };
            (items, strip)
        };
        // the items and where they are (a column of songs takes its width from its fonts: the
        // first layout gives the strip its width)
        let (items, strip) = match shelf.layout {
            ShelfLayout::Songs if !songs.is_empty() => {
                imp.row.set_spacing(COLUMN_GAP);
                let items = song_columns(songs);
                let strip = Strip {
                    // the artwork, not the highlight around a row, is in line with the heading
                    margin: (SIDE - song_cell::PAD) as f64,
                    item: song_cell::WIDTH as f64,
                    gap: COLUMN_GAP as f64,
                    count: items.len(),
                };
                (items, strip)
            }
            ShelfLayout::Hero => {
                // a line over every card, as soon as one of them has something to say
                let said = shelf.notes.iter().any(|note| !note.is_empty());
                let note = |index: usize| said.then(|| shelf.notes.get(index).map_or("", String::as_str));
                let large = shelf.items.iter().enumerate().map(|(index, item)| large_card(item, note(index), &on_card));
                cards(LARGE_CARD, large.collect())
            }
            _ => cards(CARD, shelf.items.iter().map(|item| shelf_card(item, CARD, &on_card).upcast()).collect()),
        };
        imp.row.set_margin_start(strip.margin as i32);
        imp.row.set_margin_end(strip.margin as i32);
        for item in &items {
            // nothing is mapped (and no artwork asked for) before the shelf knows where it is
            item.set_child_visible(false);
            imp.row.append(item);
        }
        imp.items.replace(items);
        imp.strip.set(strip);
        // a wheel click moves the row by one item; the arrow buttons over the row scroll it too
        let tuning = Tuning { wheel_step: strip.stride(), ..Tuning::default() };
        imp.scroll.replace(Some(SmoothScroll::attach_sideways(&imp.scroller, &imp.overlay, tuning)));
        widget
    }

    /// The pointer came over the shelf or left it: its arrows show while it is there.
    pub fn set_pointer_inside(&self, inside: bool) {
        if self.imp().pointer.replace(inside) != inside {
            self.update_arrows();
        }
    }

    /// Move the row by about what is in view, as an arrow button does.
    pub fn turn_page(&self, forward: bool) {
        let imp = self.imp();
        let Some(scroll) = imp.scroll.borrow().clone() else { return };
        let page = imp.scroller.hadjustment().page_size();
        let strip = imp.strip.get();
        // from where the row is heading: a second press while it moves goes a page further
        let from = scroll.target();
        scroll.glide_to(if forward { strip.next_page(from, page) } else { strip.previous_page(from, page) });
    }

    /// Where the overlay puts an arrow button: at either end of the row, in the middle of the
    /// artwork of the items as they are laid out (a card's artwork is a square as wide as the
    /// card, under its note when it has one; a column of songs is in the middle of itself).
    fn arrow_place(&self, overlay: &gtk::Overlay, arrow: &gtk::Widget) -> Option<gdk::Rectangle> {
        let imp = self.imp();
        let middle = {
            let items = imp.items.borrow();
            let first = items.first()?;
            let card = first.downcast_ref::<Card>().cloned().or_else(|| first.last_child().and_downcast::<Card>());
            match card {
                Some(card) => card.compute_bounds(overlay).map(|bounds| bounds.y() + bounds.width() / 2.0)?,
                None => first.compute_bounds(overlay).map(|bounds| bounds.y() + bounds.height() / 2.0)?,
            }
        };
        let (_, width, _, _) = arrow.measure(gtk::Orientation::Horizontal, -1);
        let (_, height, _, _) = arrow.measure(gtk::Orientation::Vertical, -1);
        let back = arrow == imp.back.upcast_ref::<gtk::Widget>();
        let x = if back { ARROW_INSET } else { overlay.width() - ARROW_INSET - width };
        Some(gdk::Rectangle::new(x, middle.round() as i32 - height / 2, width, height))
    }

    /// An arrow shows while the pointer is over the shelf and there is more in its direction.
    fn update_arrows(&self) {
        let imp = self.imp();
        let adjustment = imp.scroller.hadjustment();
        let end = adjustment.upper() - adjustment.page_size();
        let inside = imp.pointer.get();
        let show = |button: &gtk::Button, shown: bool| {
            if shown {
                button.add_css_class("shown");
            } else {
                button.remove_css_class("shown");
            }
            button.set_can_target(shown);
        };
        show(&imp.back, inside && adjustment.value() > adjustment.lower() + 1.0);
        show(&imp.forward, inside && adjustment.value() < end - 1.0);
    }

    /// Map the items in and around what is in view (that is when they ask for their artwork),
    /// and none while the shelf is far from what the page shows.
    fn update_window(&self) {
        let imp = self.imp();
        let adjustment = imp.scroller.hadjustment();
        let window = if imp.near.get() {
            let value = adjustment.value();
            let target = imp.scroll.borrow().as_ref().map_or(value, SmoothScroll::target);
            imp.strip.get().window(value, target, adjustment.page_size())
        } else {
            0..0
        };
        // from the right: artwork that was asked for last is fetched first
        for (index, item) in imp.items.borrow().iter().enumerate().rev() {
            // the item with the keyboard focus stays, wherever the row goes
            let focused = item.is_focus() || item.focus_child().is_some();
            item.set_child_visible(window.contains(&index) || focused);
        }
    }

    /// Whether the shelf is in view on its page, or about to be: within half a page of it.
    fn update_near(&self) {
        let imp = self.imp();
        // only a shelf that is shown and has its place can be near anything
        let placed = self.is_mapped() && self.width() > 0;
        let page = imp.page.borrow().as_ref().map(|(page, _)| page.clone());
        let near = match page.as_ref().and_then(|page| Some((self.compute_bounds(page)?, page.height() as f32))) {
            Some((bounds, height)) => {
                let ahead = height / 2.0;
                placed && bounds.y() + bounds.height() > -ahead && bounds.y() < height + ahead
            }
            // not on a page that scrolls: in view whenever it is shown
            None => placed,
        };
        if imp.near.replace(near) == near {
            return;
        }
        if !near {
            self.update_window();
            return;
        }
        // Several shelves come near together when their page is built or opened. Artwork that
        // was asked for last is fetched first, so they map their items once all of them know
        // their place, the last shelf first: the one at the top gets its artwork before the rest.
        let others_wait = ARRIVED.with_borrow_mut(|arrived| {
            arrived.push(self.downgrade());
            arrived.len() > 1
        });
        if !others_wait {
            glib::idle_add_local_full(glib::Priority::DEFAULT, || {
                for shelf in ARRIVED.take().iter().rev().filter_map(glib::WeakRef::upgrade) {
                    shelf.update_window();
                }
                glib::ControlFlow::Break
            });
        }
    }
}

/// A large card under the line that says why it is there (`None`: a row without such lines).
fn large_card(item: &Item, note: Option<&str>, handler: &CardHandler) -> gtk::Widget {
    let card = shelf_card(item, LARGE_CARD, handler);
    let Some(note) = note else { return card.upcast() };
    let line = gtk::Label::new(Some(note));
    line.add_css_class("shelf-note");
    line.add_css_class("dim-label");
    line.set_xalign(0.0);
    line.set_single_line_mode(true);
    line.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // as wide as the card, however long the line is
    line.set_max_width_chars(1);
    line.set_size_request(-1, NOTE);
    let item = gtk::Box::new(gtk::Orientation::Vertical, NOTE_GAP);
    item.set_valign(gtk::Align::Start);
    item.append(&line);
    item.append(&card);
    item.upcast()
}

/// A card of a shelf: of a fixed width, and within reach of the keyboard (Enter opens it, the
/// Menu key or Shift+F10 asks for its menu).
fn shelf_card(item: &Item, width: i32, handler: &CardHandler) -> Card {
    let card = Card::new();
    card.set_fixed_width(width);
    card.set_valign(gtk::Align::Start);
    card.set_handler(handler.clone());
    card.bind(Some(item));
    card.set_focusable(true);
    let keys = gtk::EventControllerKey::new();
    let (weak, handler) = (card.downgrade(), handler.clone());
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(card) = weak.upgrade() else { return glib::Propagation::Proceed };
        let Some(item) = card.item() else { return glib::Propagation::Proceed };
        let middle = card.width() as f64 / 2.0;
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter => handler(&item, CardAction::Open, &card),
            gdk::Key::Menu => handler(&item, CardAction::Menu(middle, middle), &card),
            gdk::Key::F10 if modifiers.contains(gdk::ModifierType::SHIFT_MASK) => {
                handler(&item, CardAction::Menu(middle, middle), &card)
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    card.add_controller(keys);
    card
}

/// The songs of a shelf in columns of four rows. A click plays the song with the others as
/// the queue.
fn song_columns(songs: Vec<Track>) -> Vec<gtk::Widget> {
    let queue = Rc::new(songs);
    let handler: SongHandler = {
        let queue = queue.clone();
        Rc::new(move |index, track, action, cell| match action {
            SongAction::Play => crate::actions::play_tracks(queue.to_vec(), index),
            SongAction::Menu(x, y) => crate::menus::track_menu(&TrackTarget::new(track), cell, x, y),
            SongAction::OpenArtist => crate::actions::open_artist_of(track),
        })
    };
    let player = crate::app::ctx().player.clone();
    let now = player.now_key();
    columns(queue.len(), ROWS)
        .into_iter()
        .map(|places| {
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            column.set_valign(gtk::Align::Start);
            let last = places.end - 1;
            for index in places {
                let cell = SongCell::new(index, &queue[index], handler.clone());
                cell.set_separator(index != last);
                cell.set_playing(now.as_deref() == Some(queue[index].key().as_str()));
                // the row follows the song that is playing
                player.song_changed.connect(&cell, |cell, _| {
                    let now = crate::app::ctx().player.now_key();
                    cell.set_playing(cell.track().is_some_and(|track| now.as_deref() == Some(track.key().as_str())));
                });
                column.append(&cell);
            }
            column.upcast()
        })
        .collect()
}

/// A shelf's worth of grey shapes, for a page that waits for its first answer.
pub fn placeholder() -> gtk::Widget {
    let shape = |class: &str, width: i32, height: i32| {
        let shape = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        shape.add_css_class("shelf-shape");
        shape.add_css_class(class);
        shape.set_size_request(width, height);
        shape.set_halign(gtk::Align::Start);
        shape
    };
    // the shapes take the room of what they stand for, so that little moves when it arrives
    let heading = shape("line", 168, 18);
    heading.set_margin_start(SIDE);
    heading.set_margin_top(4);
    heading.set_margin_bottom(3);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
    row.set_margin_start(SIDE);
    row.set_margin_top(ABOVE);
    row.set_margin_bottom(BELOW);
    for _ in 0..10 {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.append(&shape("art", CARD, CARD));
        let title = shape("line", 124, 10);
        title.set_margin_top(12);
        card.append(&title);
        let subtitle = shape("line", 78, 9);
        subtitle.set_margin_top(9);
        subtitle.set_margin_bottom(11);
        card.append(&subtitle);
        row.append(&card);
    }
    // cut off at the edge of the page like a shelf, without taking part in scrolling
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    scroller.set_child(Some(&row));
    scroller.set_can_target(false);
    let shelf = gtk::Box::new(gtk::Orientation::Vertical, 0);
    shelf.add_css_class("shelf-placeholder");
    shelf.append(&heading);
    shelf.append(&scroller);
    shelf.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twelve cards as a shelf lays them out.
    fn cards(count: usize) -> Strip {
        Strip { margin: 30.0, item: 176.0, gap: 20.0, count }
    }

    #[test]
    fn row_width() {
        assert_eq!(cards(0).width(), 0.0);
        assert_eq!(cards(1).width(), 236.0);
        assert_eq!(cards(12).width(), 2392.0);
        assert_eq!(cards(12).end(1180.0), 1212.0);
        // a row that fits does not scroll
        assert_eq!(cards(3).end(1180.0), 0.0);
    }

    #[test]
    fn next_arrow_brings_the_first_cut_item_to_the_front() {
        let strip = cards(12);
        // five cards are whole in 1180 px, the sixth is cut: it becomes the first
        assert_eq!(strip.next_page(0.0, 1180.0), 980.0);
        // the last page ends with the margin after the last card
        assert_eq!(strip.next_page(980.0, 1180.0), 1212.0);
        assert_eq!(strip.next_page(1212.0, 1180.0), 1212.0);
        // from anywhere in between (after a fling), a card is in line with the heading again
        assert_eq!(strip.next_page(100.0, 1180.0), 1176.0);
        assert_eq!(strip.next_page(250.0, 1180.0), 1212.0);
    }

    #[test]
    fn previous_arrow_goes_back_the_same_way() {
        let strip = cards(12);
        assert_eq!(strip.previous_page(0.0, 1180.0), 0.0);
        assert_eq!(strip.previous_page(980.0, 1180.0), 0.0);
        // from the end: the card cut at the left edge comes into view whole, at the right
        assert_eq!(strip.previous_page(1212.0, 1180.0), 392.0);
        assert_eq!(strip.previous_page(392.0, 1180.0), 0.0);
    }

    #[test]
    fn arrows_move_about_one_view() {
        for (count, page) in [(8, 640.0), (12, 1180.0), (16, 1180.0), (20, 1660.0), (20, 400.0)] {
            let strip = cards(count);
            let mut value = 0.0;
            let mut presses = 0;
            while value < strip.end(page) {
                let next = strip.next_page(value, page);
                assert!(next > value, "{count} cards, {page} px: stuck at {value}");
                assert!(next - value <= page, "{count} cards, {page} px: {value} -> {next} skips cards");
                // at least half of the view, or one card when the view is that narrow
                let least = (page * 0.5).min(strip.stride());
                assert!(
                    next - value >= least || next == strip.end(page),
                    "{count} cards, {page} px: {value} -> {next}"
                );
                value = next;
                presses += 1;
            }
            assert!(presses <= count, "{presses} presses for {count} cards");
            while value > 0.0 {
                let previous = strip.previous_page(value, page);
                assert!(previous < value, "{count} cards, {page} px: stuck at {value}");
                assert!(value - previous <= page, "{count} cards, {page} px: {value} -> {previous} skips cards");
                value = previous;
            }
        }
    }

    #[test]
    fn a_view_narrower_than_a_card_still_moves() {
        let strip = cards(6);
        assert_eq!(strip.next_page(0.0, 150.0), 196.0);
        assert_eq!(strip.next_page(196.0, 150.0), 392.0);
        assert_eq!(strip.previous_page(392.0, 150.0), 196.0);
        assert_eq!(strip.previous_page(196.0, 150.0), 0.0);
    }

    #[test]
    fn items_near_the_view_are_kept_ready() {
        let strip = cards(20);
        // nothing before the shelf has a width
        assert_eq!(strip.window(0.0, 0.0, 0.0), 0..0);
        // at rest: the six in view (the sixth is cut) and the next one
        assert_eq!(strip.window(0.0, 0.0, 1180.0), 0..7);
        // a page on: one before, the seven in view (the first and the last are cut), one after
        assert_eq!(strip.window(980.0, 980.0, 1180.0), 3..12);
        // on the way to the next page: both pages
        assert_eq!(strip.window(0.0, 980.0, 1180.0), 0..12);
        assert_eq!(strip.window(600.0, 980.0, 1180.0), 2..12);
        // and on the way back
        assert_eq!(strip.window(1960.0, 980.0, 1180.0), 3..17);
        // at the end of the row
        assert_eq!(strip.window(2780.0, 2780.0, 1180.0), 13..20);
        assert_eq!(cards(3).window(0.0, 0.0, 1180.0), 0..3);
        assert_eq!(cards(0).window(0.0, 0.0, 1180.0), 0..0);
    }

    #[test]
    fn songs_fill_columns_of_four() {
        assert_eq!(columns(0, 4), Vec::<Range<usize>>::new());
        assert_eq!(columns(3, 4), vec![0..3]);
        assert_eq!(columns(8, 4), vec![0..4, 4..8]);
        assert_eq!(columns(10, 4), vec![0..4, 4..8, 8..10]);
        assert_eq!(columns(16, 4).len(), 4);
        // never a column without rows
        assert_eq!(columns(5, 0), vec![0..1, 1..2, 2..3, 3..4, 4..5]);
    }
}
