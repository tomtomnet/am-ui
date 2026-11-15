//! Playing Next: the songs after the current one, in a panel beside the pages.
//!
//! The list follows the player's queue by changing only what changed. When a song ends its row
//! is removed and every other row stays as it is: a queue can hold a thousand songs, and
//! replacing all of them on every song stalled the earlier application for seconds. A row knows
//! its place in the list, not in the queue; the place in the queue is worked out when something
//! is done with the row. Like `TrackRow`, a row takes its height from the fonts of its labels
//! (`Metrics`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use amui::types::Track;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::app::ctx;
use crate::library::boxed;
use crate::menus::{self, TrackTarget};
use crate::pages::scrolled;
use crate::widgets::thumb::Thumb;
use crate::widgets::{activity, clock, font_serial, label_line, label_width};

/// The width of the panel.
pub const WIDTH: f64 = 340.0;
/// The smallest artwork: it grows with the two lines of text beside it.
const ART: i32 = 40;
/// How much taller the artwork is than the two lines of text, and the room above and below it.
const ART_OVER_TEXT: i32 = 8;
const ART_ROOM: i32 = 12;
/// The room of the heading over the first of Autoplay's songs, beside its line.
const HEADING_ROOM: i32 = 24;
const PAD: i32 = 10;
const GAP: i32 = 10;
/// The duration and, under the pointer, the button that removes the song share this much room
/// (at the usual text size; wider with larger text), and the room beside the widest duration.
const SLOT: i32 = 44;
const SLOT_ROOM: i32 = 8;
const BUTTON: i32 = 30;

/// The sizes of a row that come from its fonts, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    /// A line of the title, of the artist and of the heading.
    pub title: i32,
    pub artist: i32,
    pub heading: i32,
    /// The widest duration ("00:00").
    pub duration: i32,
}

impl Metrics {
    pub fn art(&self) -> i32 {
        (self.title + self.artist + ART_OVER_TEXT).max(ART)
    }

    /// The song's part of a row, and the heading's.
    pub fn song_height(&self) -> i32 {
        self.art() + ART_ROOM
    }

    pub fn heading_height(&self) -> i32 {
        self.heading + HEADING_ROOM
    }

    pub fn slot(&self) -> i32 {
        SLOT.max(self.duration + SLOT_ROOM).max(BUTTON)
    }
}
/// A dragged row this close to the upper or lower edge of the list scrolls it.
const EDGE: f64 = 48.0;
/// Pixels per second the list scrolls at most while a row is dragged at its edge.
const PULL: f64 = 900.0;

/// What a row of the list shows: a song, and whether "Autoplay" stands over it (the first of
/// the songs Autoplay added).
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    track: Track,
    heading: bool,
}

fn entry_of(object: &glib::Object) -> Option<Entry> {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .and_then(|boxed| boxed.try_borrow::<Entry>().ok().map(|entry| entry.clone()))
}

/// What tells two rows apart: the song, and the heading over it (a row that gains or loses the
/// heading is a row of its own, made again).
fn row_key(entry: &Entry) -> String {
    if entry.heading { format!("{}\u{1}autoplay", entry.track.key()) } else { entry.track.key() }
}

/// The song of a row's key.
fn song_key(row: &str) -> &str {
    row.split('\u{1}').next().unwrap_or(row)
}

/// One change to a list: at `at`, `remove` rows go and `insert` rows of the new list come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub remove: usize,
    pub insert: usize,
}

/// The one change that turns a list showing `old` into one showing `new`: what the two have in
/// common at the front and at the back is not touched. `None` when they are the same.
pub fn splice<T: PartialEq>(old: &[T], new: &[T]) -> Option<Splice> {
    let front = old.iter().zip(new).take_while(|(before, after)| before == after).count();
    let back =
        old[front..].iter().rev().zip(new[front..].iter().rev()).take_while(|(before, after)| before == after).count();
    let change = Splice { at: front, remove: old.len() - front - back, insert: new.len() - front - back };
    (change.remove > 0 || change.insert > 0).then_some(change)
}

/// Where a row taken from `from` goes when it is dropped into the gap before row `gap` (the
/// number of rows for the gap after the last one): its place afterwards, or `None` when that is
/// where it is already.
pub fn landing(from: usize, gap: usize) -> Option<usize> {
    if gap == from || gap == from + 1 {
        None
    } else if gap > from {
        // taking the row out moves the gap up by one
        Some(gap - 1)
    } else {
        Some(gap)
    }
}

/// The place in the whole queue of the list's first row: the song after the current one.
pub fn first_row(current: i64) -> usize {
    (current + 1).max(0) as usize
}

/// What a dragged row carries: where it was taken from and which song it is.
#[derive(Debug, Clone, PartialEq, Eq, glib::Boxed)]
#[boxed_type(name = "AmQueueDrag")]
pub struct Dragged {
    from: u32,
    key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowAction {
    Remove,
    /// The context menu, at this point of the row.
    Menu(f64, f64),
    /// A dragged row was dropped on this one: into the gap before it, or the one after it.
    Drop {
        dragged: Dragged,
        after: bool,
    },
}

/// Called with the row, its place in the list, its song and what the user did with it.
pub type RowHandler = Rc<dyn Fn(&QueueRow, u32, &Track, RowAction)>;

mod imp {
    use super::*;

    pub struct QueueRow {
        /// The cell of the list this row sits in: it knows the row's place.
        pub cell: glib::WeakRef<gtk::ListItem>,
        pub track: RefCell<Option<Track>>,
        pub handler: RefCell<Option<RowHandler>>,
        pub thumb: Thumb,
        pub title: gtk::Label,
        pub artist: gtk::Label,
        pub duration: gtk::Label,
        pub remove: gtk::Button,
        /// "Autoplay", over the first of the songs Autoplay added.
        pub heading: gtk::Label,
        /// Where the row was taken hold of when a drag began.
        pub held: Cell<(f64, f64)>,
        /// The metrics of the fonts, with the `font_serial` they were measured at.
        pub metrics: Cell<Option<(u64, Metrics)>>,
    }

    impl Default for QueueRow {
        fn default() -> Self {
            let label = |class: &str| {
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_single_line_mode(true);
                label.add_css_class(class);
                label
            };
            let artist = label("row-artist");
            artist.add_css_class("dim-label");
            let duration = label("row-duration");
            duration.set_xalign(1.0);
            duration.add_css_class("numeric");
            duration.add_css_class("dim-label");
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("flat");
            remove.add_css_class("circular");
            remove.add_css_class("hover-only");
            remove.set_tooltip_text(Some("Remove"));
            remove.set_focus_on_click(false);
            let heading = label("queue-section");
            heading.set_text("Autoplay");
            heading.set_visible(false);
            QueueRow {
                cell: glib::WeakRef::new(),
                track: RefCell::default(),
                handler: RefCell::default(),
                thumb: Thumb::new(ART, 5.0),
                title: label("row-title"),
                artist,
                duration,
                remove,
                heading,
                held: Cell::new((0.0, 0.0)),
                metrics: Cell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for QueueRow {
        const NAME: &'static str = "AmQueueRow";
        type Type = super::QueueRow;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("queuerow");
        }
    }

    impl ObjectImpl for QueueRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            for child in [
                self.thumb.upcast_ref::<gtk::Widget>(),
                self.title.upcast_ref(),
                self.artist.upcast_ref(),
                self.duration.upcast_ref(),
                self.remove.upcast_ref(),
                self.heading.upcast_ref(),
            ] {
                child.set_parent(&*obj);
            }
            let weak = obj.downgrade();
            self.remove.connect_clicked(move |_| {
                if let Some(row) = weak.upgrade() {
                    row.emit(RowAction::Remove);
                }
            });

            // a right click opens the song's menu (a click plays the song: the list sees to that)
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let weak = obj.downgrade();
            click.connect_pressed(move |gesture, _, x, y| {
                if let Some(row) = weak.upgrade() {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    row.emit(RowAction::Menu(x, y));
                }
            });
            obj.add_controller(click);

            // the row can be dragged to another place in the list
            let drag = gtk::DragSource::new();
            drag.set_actions(gdk::DragAction::MOVE);
            let weak = obj.downgrade();
            drag.connect_prepare(move |_, x, y| {
                let row = weak.upgrade()?;
                let dragged = Dragged { from: row.position()?, key: row.imp().track.borrow().as_ref()?.key() };
                row.imp().held.set((x, y));
                Some(gdk::ContentProvider::for_value(&dragged.to_value()))
            });
            let weak = obj.downgrade();
            drag.connect_drag_begin(move |source, _| {
                let Some(row) = weak.upgrade() else { return };
                // the pointer carries a picture of the row as it is now; the row itself fades
                let (x, y) = row.imp().held.get();
                source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row)).current_image()), x as i32, y as i32);
                row.add_css_class("dragged");
            });
            let weak = obj.downgrade();
            drag.connect_drag_end(move |_, _, _| {
                if let Some(row) = weak.upgrade() {
                    row.remove_css_class("dragged");
                }
            });
            obj.add_controller(drag);

            // and another row can be dropped on it: a line shows the gap it will land in
            let drop = gtk::DropTarget::new(Dragged::static_type(), gdk::DragAction::MOVE);
            let weak = obj.downgrade();
            drop.connect_motion(move |_, _, y| {
                if let Some(row) = weak.upgrade() {
                    row.show_gap(Some(row.lower_half(y)));
                }
                gdk::DragAction::MOVE
            });
            let weak = obj.downgrade();
            drop.connect_leave(move |_| {
                if let Some(row) = weak.upgrade() {
                    row.show_gap(None);
                }
            });
            let weak = obj.downgrade();
            drop.connect_drop(move |_, value, _, y| {
                let (Some(row), Ok(dragged)) = (weak.upgrade(), value.get::<Dragged>()) else { return false };
                row.show_gap(None);
                row.emit(RowAction::Drop { dragged, after: row.lower_half(y) });
                true
            });
            obj.add_controller(drop);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for QueueRow {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                (200, WIDTH as i32, -1, -1)
            } else {
                let metrics = self.obj().metrics();
                let heading = if self.heading.is_visible() { metrics.heading_height() } else { 0 };
                let high = metrics.song_height() + heading;
                (high, high, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, full_height: i32, _baseline: i32) {
            let metrics = self.obj().metrics();
            // every child is measured before it is placed, as GTK asks
            let tall = |child: &gtk::Widget, wide: i32| {
                let _ = child.measure(gtk::Orientation::Horizontal, -1);
                child.measure(gtk::Orientation::Vertical, wide).1
            };
            // the heading over the song, when the row has one: the song keeps the rest
            let above = if self.heading.is_visible() { metrics.heading_height() } else { 0 };
            let height = full_height - above;
            let put = |child: &gtk::Widget, x: i32, y: i32, wide: i32, high: i32| {
                child.size_allocate(&gtk::Allocation::new(x, y + above, wide.max(1), high.max(1)), -1);
            };
            // `child` in the middle of the line from `top` that is `high` tall
            let centred = |child: &gtk::Widget, x: i32, wide: i32, top: i32, high: i32| {
                let own = tall(child, wide);
                put(child, x, top + (high - own) / 2, wide, own);
            };
            if above > 0 {
                let high = tall(self.heading.upcast_ref(), width - 2 * PAD);
                let heading = gtk::Allocation::new(PAD, above - high - 6, (width - 2 * PAD).max(1), high.max(1));
                self.heading.size_allocate(&heading, -1);
            }
            let art = metrics.art();
            let _ = tall(self.thumb.upcast_ref(), art);
            put(self.thumb.upcast_ref(), PAD, (height - art) / 2, art, art);

            // the duration and the button share the room at the right end
            let slot_width = metrics.slot();
            let slot = width - PAD - slot_width;
            centred(self.duration.upcast_ref(), slot, slot_width, 0, height);
            centred(self.remove.upcast_ref(), width - PAD - BUTTON, BUTTON, 0, height);

            let text = PAD + art + GAP;
            let wide = (slot - GAP - text).max(40);
            let top = (height - metrics.title - metrics.artist) / 2;
            centred(self.title.upcast_ref(), text, wide, top, metrics.title);
            centred(self.artist.upcast_ref(), text, wide, top + metrics.title, metrics.artist);

            // a menu that was opened on this row is placed by the row
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.present();
                }
                child = widget.next_sibling();
            }
        }
    }
}

glib::wrapper! {
    pub struct QueueRow(ObjectSubclass<imp::QueueRow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl QueueRow {
    fn new(cell: &gtk::ListItem, handler: RowHandler) -> QueueRow {
        let row: QueueRow = glib::Object::new();
        row.imp().cell.set(Some(cell));
        row.imp().handler.replace(Some(handler));
        row
    }

    /// The sizes that come from the fonts of the labels, measured again when one of those fonts
    /// changes.
    fn metrics(&self) -> Metrics {
        let imp = self.imp();
        let serial = font_serial(&[
            imp.title.upcast_ref(),
            imp.artist.upcast_ref(),
            imp.heading.upcast_ref(),
            imp.duration.upcast_ref(),
        ]);
        if let Some((measured, metrics)) = imp.metrics.get()
            && measured == serial
        {
            return metrics;
        }
        let metrics = Metrics {
            title: label_line(&imp.title),
            artist: label_line(&imp.artist),
            heading: label_line(&imp.heading),
            duration: label_width(&imp.duration, "00:00"),
        };
        imp.metrics.set(Some((serial, metrics)));
        metrics
    }

    /// The row's place in the list.
    fn position(&self) -> Option<u32> {
        self.imp().cell.upgrade().map(|cell| cell.position()).filter(|position| *position != gtk::INVALID_LIST_POSITION)
    }

    /// Show "Autoplay" over the row, or not.
    fn set_heading(&self, shown: bool) {
        if self.imp().heading.is_visible() != shown {
            self.imp().heading.set_visible(shown);
            self.queue_resize();
        }
    }

    fn bind(&self, track: &Track) {
        let imp = self.imp();
        imp.title.set_text(&track.name);
        imp.artist.set_text(&track.artist);
        imp.duration.set_text(&track.duration_ms.map(|ms| clock(ms as f64 / 1000.0)).unwrap_or_default());
        imp.thumb.set_artwork(track.artwork.as_ref());
        imp.track.replace(Some(track.clone()));
    }

    fn unbind(&self) {
        let imp = self.imp();
        imp.track.replace(None);
        imp.thumb.set_artwork(None);
        // a row that is given another song is no longer the one that was dragged or pointed at
        self.remove_css_class("dragged");
        self.show_gap(None);
    }

    fn lower_half(&self, y: f64) -> bool {
        y >= self.height() as f64 / 2.0
    }

    /// Show the line of the gap a dragged row would land in: the one after this row, the one
    /// before it, or none.
    fn show_gap(&self, after: Option<bool>) {
        for (class, shown) in [("drop-above", after == Some(false)), ("drop-below", after == Some(true))] {
            if shown {
                self.add_css_class(class);
            } else {
                self.remove_css_class(class);
            }
        }
    }

    fn emit(&self, action: RowAction) {
        let imp = self.imp();
        let (handler, track) = (imp.handler.borrow().clone(), imp.track.borrow().clone());
        if let (Some(handler), Some(track), Some(position)) = (handler, track, self.position()) {
            handler(self, position, &track, action);
        }
    }
}

/// While a row is dragged near the upper or lower edge of the list, the list scrolls: faster the
/// closer to the edge. A tick callback runs only while it does (at the end of the list it stops,
/// and the next movement of the row starts it again).
fn scroll_while_dragging(scroller: &gtk::ScrolledWindow) {
    let speed = Rc::new(Cell::new(0.0f64));
    let running = Rc::new(Cell::new(false));
    let motion = gtk::DropControllerMotion::new();
    {
        let (speed, weak) = (speed.clone(), scroller.downgrade());
        motion.connect_motion(move |_, _, y| {
            let Some(scroller) = weak.upgrade() else { return };
            let height = scroller.height() as f64;
            let pull = if y < EDGE {
                (y - EDGE) / EDGE
            } else if y > height - EDGE {
                (y - (height - EDGE)) / EDGE
            } else {
                0.0
            };
            speed.set(pull.clamp(-1.0, 1.0) * PULL);
            if speed.get() == 0.0 || running.replace(true) {
                return;
            }
            let (speed, running, before) = (speed.clone(), running.clone(), Cell::new(None::<i64>));
            scroller.add_tick_callback(move |scroller, clock| {
                activity::note(activity::Source::Queue);
                let now = clock.frame_time();
                let passed = before.replace(Some(now)).map_or(0.0, |before| (now - before) as f64 / 1e6).min(0.05);
                let adjustment = scroller.vadjustment();
                let was = adjustment.value();
                adjustment.set_value(was + speed.get() * passed);
                // the row left the edge, or the list is at its end: nothing moves any more
                if speed.get() == 0.0 || (passed > 0.0 && adjustment.value() == was) {
                    running.set(false);
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            });
        });
    }
    motion.connect_leave(move |_| speed.set(0.0));
    scroller.add_controller(motion);
}

/// The panel: the heading with "Clear", and the list of the songs that play next.
pub fn build() -> gtk::Widget {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    // the songs of the rows, to tell what changed when the queue does
    let shown: Rc<RefCell<Vec<String>>> = Rc::default();

    let handler: RowHandler = {
        let shown = shown.clone();
        Rc::new(move |row, position, track, action| {
            let player = ctx().player.clone();
            let (first, position) = (first_row(player.queue_index()), position as usize);
            match action {
                RowAction::Remove => player.playback.remove(first + position),
                RowAction::Menu(x, y) => {
                    let target = TrackTarget { queue_index: Some(first + position), ..TrackTarget::new(track) };
                    menus::track_menu(&target, row, x, y);
                }
                RowAction::Drop { dragged, after } => {
                    // a song may have ended while the row was dragged: its song says where it is now
                    let from = {
                        let shown = shown.borrow();
                        match shown.get(dragged.from as usize) {
                            Some(key) if song_key(key) == dragged.key => Some(dragged.from as usize),
                            _ => shown.iter().position(|key| song_key(key) == dragged.key),
                        }
                    };
                    let moved = from.and_then(|from| Some((from, landing(from, position + after as usize)?)));
                    if let Some((from, to)) = moved {
                        player.playback.reorder(first + from, first + to);
                    }
                }
            }
        })
    };

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        if let Some(cell) = object.downcast_ref::<gtk::ListItem>() {
            cell.set_child(Some(&QueueRow::new(cell, handler.clone())));
        }
    });
    factory.connect_bind(|_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        if let (Some(row), Some(entry)) =
            (cell.child().and_downcast::<QueueRow>(), cell.item().as_ref().and_then(entry_of))
        {
            row.bind(&entry.track);
            row.set_heading(entry.heading);
        }
    });
    factory.connect_unbind(|_, object| {
        if let Some(row) =
            object.downcast_ref::<gtk::ListItem>().and_then(|cell| cell.child()).and_downcast::<QueueRow>()
        {
            row.unbind();
        }
    });
    let list = gtk::ListView::new(Some(gtk::NoSelection::new(Some(store.clone()))), Some(factory));
    list.add_css_class("queue");
    // a click on a row (or Enter on it) jumps to its song
    list.set_single_click_activate(true);
    list.connect_activate(|_, position| {
        let player = ctx().player.clone();
        player.playback.jump(first_row(player.queue_index()) + position as usize);
    });
    // Delete removes the song of the row that has the keyboard
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(|keys, key, _, modifiers| {
        let plain = !modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
        let focus = keys.widget().and_then(|list| list.root()).and_then(|root| root.focus());
        let row = focus.and_then(|focus| {
            focus
                .first_child()
                .and_downcast::<QueueRow>()
                .or_else(|| focus.ancestor(QueueRow::static_type()).and_downcast())
        });
        match row {
            Some(row) if plain && matches!(key, gdk::Key::Delete | gdk::Key::KP_Delete | gdk::Key::BackSpace) => {
                row.emit(RowAction::Remove);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    list.add_controller(keys);
    let scroller = scrolled(&list);
    scroll_while_dragging(&scroller);

    let empty = gtk::Label::new(Some("No upcoming songs"));
    empty.add_css_class("dim-label");
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scroller, Some("songs"));

    let heading = gtk::Label::new(Some("Playing Next"));
    heading.add_css_class("queue-heading");
    let clear = gtk::Button::with_label("Clear");
    clear.add_css_class("flat");
    clear.set_sensitive(false);
    clear.connect_clicked(|_| ctx().player.playback.clear_upcoming());
    // Autoplay: similar music goes on where the queue ends
    let autoplay = gtk::ToggleButton::new();
    autoplay.set_icon_name("am-autoplay-symbolic");
    autoplay.set_tooltip_text(Some("Autoplay"));
    autoplay.add_css_class("flat");
    autoplay.add_css_class("autoplay-toggle");
    autoplay.set_active(ctx().player.view().autoplay);
    autoplay.connect_toggled(|button| {
        let player = ctx().player.clone();
        if player.view().autoplay != button.is_active() {
            player.playback.set_autoplay(button.is_active());
        }
    });
    ctx().player.changed.connect(&autoplay, |button, _| {
        let on = ctx().player.view().autoplay;
        if button.is_active() != on {
            button.set_active(on);
        }
    });
    let header = adw::HeaderBar::new();
    header.set_show_title(false);
    header.pack_start(&heading);
    header.pack_end(&clear);
    header.pack_end(&autoplay);
    let panel = adw::ToolbarView::new();
    panel.add_css_class("queue-panel");
    panel.add_top_bar(&header);
    panel.set_content(Some(&stack));

    // bring the list up to date with the player's queue
    let sync = Rc::new(move || {
        let player = ctx().player.clone();
        // "Autoplay" over the first of the songs it added
        let first = player.upcoming_autoplay().iter().position(|flag| *flag);
        let entries: Vec<Entry> = player
            .upcoming()
            .into_iter()
            .enumerate()
            .map(|(at, track)| Entry { track, heading: Some(at) == first })
            .collect();
        let keys: Vec<String> = entries.iter().map(row_key).collect();
        let change = splice(&shown.borrow(), &keys);
        if let Some(change) = change {
            let rows: Vec<glib::BoxedAnyObject> =
                entries.into_iter().skip(change.at).take(change.insert).map(boxed).collect();
            store.splice(change.at as u32, change.remove as u32, &rows);
            shown.replace(keys);
        }
        let any = store.n_items() > 0;
        stack.set_visible_child_name(if any { "songs" } else { "empty" });
        clear.set_sensitive(any);
    });
    // a hidden panel leaves its list alone and brings it up to date when it shows
    let player = ctx().player.clone();
    for signal in [&player.queue_changed, &player.song_changed] {
        let sync = sync.clone();
        signal.connect(&panel, move |panel, _| {
            if panel.is_mapped() {
                sync();
            }
        });
    }
    panel.connect_map(move |_| sync());
    panel.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(at: usize, remove: usize, insert: usize) -> Option<Splice> {
        Some(Splice { at, remove, insert })
    }

    /// The list after `change` was applied to one showing `old`, to become one showing `new`.
    fn applied(old: &[u32], new: &[u32]) -> Vec<u32> {
        let mut list = old.to_vec();
        if let Some(change) = splice(old, new) {
            list.splice(
                change.at..change.at + change.remove,
                new[change.at..change.at + change.insert].iter().copied(),
            );
        }
        list
    }

    #[test]
    fn the_next_song_removes_one_row() {
        // 900 songs wait; the first of them begins to play
        let old: Vec<u32> = (0..900).collect();
        assert_eq!(splice(&old, &old[1..]), change(0, 1, 0));
        // a jump over 40 songs removes those and nothing else
        assert_eq!(splice(&old, &old[41..]), change(0, 41, 0));
        assert_eq!(splice(&old, &old), None);
    }

    #[test]
    fn changes_to_the_queue_touch_only_what_changed() {
        let old = [1, 2, 3, 4, 5];
        // a song removed in the middle
        assert_eq!(splice(&old, &[1, 2, 4, 5]), change(2, 1, 0));
        // songs that play next come in at the front, songs added to the queue at the end
        assert_eq!(splice(&old, &[8, 9, 1, 2, 3, 4, 5]), change(0, 0, 2));
        assert_eq!(splice(&old, &[1, 2, 3, 4, 5, 8, 9]), change(5, 0, 2));
        // back to the song before: it is in the list again
        assert_eq!(splice(&old[1..], &old), change(0, 0, 1));
        // "Clear", and the first songs of an empty list
        assert_eq!(splice(&old, &[]), change(0, 5, 0));
        assert_eq!(splice(&[], &old), change(0, 0, 5));
        assert_eq!(splice::<u32>(&[], &[]), None);
        // a row dragged from the second place to the fourth: the rows between are replaced
        assert_eq!(splice(&old, &[1, 3, 4, 2, 5]), change(1, 3, 3));
        // another queue altogether
        assert_eq!(splice(&old, &[6, 7, 8]), change(0, 5, 3));
    }

    #[test]
    fn the_change_gives_the_new_list() {
        let cases: [(&[u32], &[u32]); 8] = [
            (&[1, 2, 3, 4, 5], &[2, 3, 4, 5]),
            (&[1, 2, 3, 4, 5], &[1, 3, 4, 2, 5]),
            (&[1, 2, 3], &[9, 1, 2, 3, 8]),
            (&[1, 2, 3], &[]),
            (&[], &[4, 5]),
            // the same song more than once
            (&[7, 7, 3, 7], &[7, 3, 7]),
            (&[7, 7, 7], &[7, 7, 7, 7]),
            (&[1, 2, 1, 2], &[2, 1, 2, 1]),
        ];
        for (old, new) in cases {
            assert_eq!(applied(old, new), new, "{old:?} to {new:?}");
        }
    }

    #[test]
    fn a_dropped_row_lands_in_the_gap_it_was_dropped_in() {
        // rows 0 1 2 3 4; the gaps are numbered by the row that follows them, 5 is the end
        assert_eq!(landing(1, 0), Some(0));
        assert_eq!(landing(1, 4), Some(3));
        assert_eq!(landing(1, 5), Some(4));
        assert_eq!(landing(4, 0), Some(0));
        assert_eq!(landing(0, 5), Some(4));
        // the gaps on either side of the row itself leave it where it is
        assert_eq!(landing(1, 1), None);
        assert_eq!(landing(1, 2), None);
        assert_eq!(landing(4, 5), None);
        // the result is where the row is after `remove(from)` and `insert(to)`
        for from in 0..5 {
            for gap in 0..=5 {
                let mut rows = vec![0, 1, 2, 3, 4];
                let Some(to) = landing(from, gap) else { continue };
                let row = rows.remove(from);
                rows.insert(to, row);
                let before: Vec<i32> = (0..gap as i32).filter(|other| *other != from as i32).collect();
                assert_eq!(rows[..to], before[..], "from {from} into gap {gap}");
                assert_eq!(rows[to], from as i32);
            }
        }
    }

    #[test]
    fn a_row_is_as_large_as_its_text() {
        // this desktop's usual text size (10 points): the sizes the rows were drawn with
        let usual = Metrics { title: 17, artist: 15, heading: 16, duration: 36 };
        let sizes = |metrics: Metrics| (metrics.art(), metrics.song_height(), metrics.heading_height(), metrics.slot());
        assert_eq!(sizes(usual), (ART, 52, 40, SLOT));
        // text twice as large: artwork as tall as the two lines, room for the widest duration
        let double = Metrics { title: 33, artist: 30, heading: 31, duration: 71 };
        assert_eq!(sizes(double), (71, 83, 55, 79));
    }

    #[test]
    fn rows_count_from_the_song_after_the_current_one() {
        assert_eq!(first_row(2), 3);
        assert_eq!(first_row(0), 1);
        // nothing is playing: the first row is the first song of the queue
        assert_eq!(first_row(-1), 0);
    }
}
