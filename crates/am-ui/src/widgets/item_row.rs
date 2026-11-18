//! One album, artist or playlist in a list: artwork (round for an artist), the title and the
//! line under it, the year of an album, and a "more" button while the pointer is over the row.
//!
//! Built from ordinary widgets placed by hand, like `TrackRow`, so that the years line up from
//! row to row. The row does not open what it shows: the list it is in activates its rows. A
//! right click and the "more" button ask for the menu. Like `TrackRow`, it takes its height and
//! the width of the years from the fonts of its labels (`Metrics`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, pango};

use crate::data::Item;
use crate::pages::detail::year;
use crate::widgets::cover::blank_icon;
use crate::widgets::thumb::Thumb;
use crate::widgets::{font_serial, label_line, label_width};

/// The narrowest a row can be, and how wide it would like to be in a list of its own.
const MIN_WIDTH: i32 = 280;
const NATURAL_WIDTH: i32 = 720;
const PAD: i32 = 14;
/// The smallest artwork: it grows with the two lines of text beside it.
const ART: i32 = 48;
/// How much taller the artwork is than the two lines of text, and the room above and below it.
const ART_OVER_TEXT: i32 = 16;
const ART_ROOM: i32 = 12;
const GAP: i32 = 14;
const BUTTON: i32 = 32;
/// The column of the years at the usual text size, and its room beside four digits.
const YEAR_WIDTH: i32 = 52;
const YEAR_ROOM: i32 = 20;

/// The sizes of a row that come from its fonts, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    /// A line of the title, and one of the line under it.
    pub title: i32,
    pub line: i32,
    /// Four digits of a year.
    pub year: i32,
}

impl Metrics {
    pub fn art(&self) -> i32 {
        (self.title + self.line + ART_OVER_TEXT).max(ART)
    }

    pub fn height(&self) -> i32 {
        self.art() + ART_ROOM
    }

    pub fn year_column(&self) -> i32 {
        YEAR_WIDTH.max(self.year + YEAR_ROOM)
    }
}

/// Called for the menu of the item a row shows, at a point of the row.
pub type MenuHandler = Rc<dyn Fn(&Item, &ItemRow, f64, f64)>;

mod imp {
    use super::*;

    pub struct ItemRow {
        pub item: RefCell<Option<Item>>,
        pub menu: RefCell<Option<MenuHandler>>,
        /// How wide the row would like to be.
        pub natural: Cell<i32>,
        pub thumb: Thumb,
        pub title: gtk::Label,
        pub line: gtk::Label,
        pub year: gtk::Label,
        pub more: gtk::Button,
        /// The metrics of the fonts, with the `font_serial` they were measured at.
        pub metrics: Cell<Option<(u64, Metrics)>>,
    }

    impl Default for ItemRow {
        fn default() -> Self {
            let label = |class: &str| {
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(pango::EllipsizeMode::End);
                label.set_single_line_mode(true);
                label.add_css_class(class);
                label
            };
            let line = label("row-line");
            line.add_css_class("dim-label");
            let year = label("row-year");
            year.set_xalign(1.0);
            year.add_css_class("numeric");
            year.add_css_class("dim-label");
            let more = gtk::Button::from_icon_name("am-view-more-horizontal-symbolic");
            more.add_css_class("flat");
            more.add_css_class("circular");
            more.add_css_class("hover-only");
            more.set_tooltip_text(Some("More"));
            more.set_focus_on_click(false);
            more.set_focusable(false);
            ItemRow {
                item: RefCell::default(),
                menu: RefCell::default(),
                natural: Cell::new(NATURAL_WIDTH),
                thumb: Thumb::new(ART, 6.0),
                title: label("row-title"),
                line,
                year,
                more,
                metrics: Cell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ItemRow {
        const NAME: &'static str = "AmItemRow";
        type Type = super::ItemRow;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("itemrow");
        }
    }

    impl ObjectImpl for ItemRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            for child in [
                self.thumb.upcast_ref::<gtk::Widget>(),
                self.title.upcast_ref(),
                self.line.upcast_ref(),
                self.year.upcast_ref(),
                self.more.upcast_ref(),
            ] {
                child.set_parent(&*obj);
            }
            let weak = obj.downgrade();
            self.more.connect_clicked(move |button| {
                if let Some(row) = weak.upgrade() {
                    let at = button
                        .compute_point(&row, &graphene::Point::new(BUTTON as f32 / 2.0, BUTTON as f32))
                        .unwrap_or_default();
                    row.ask_menu(at.x() as f64, at.y() as f64);
                }
            });

            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let weak = obj.downgrade();
            click.connect_pressed(move |gesture, _, x, y| {
                if let Some(row) = weak.upgrade() {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    row.ask_menu(x, y);
                }
            });
            obj.add_controller(click);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ItemRow {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                (MIN_WIDTH, self.natural.get().max(MIN_WIDTH), -1, -1)
            } else {
                let height = self.obj().metrics().height();
                (height, height, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let metrics = self.obj().metrics();
            // every child is measured before it is placed, as GTK asks
            let natural = |child: &gtk::Widget, orientation: gtk::Orientation| child.measure(orientation, -1).1;
            // `child` in the middle of the line from `top` that is `high` tall
            let put = |child: &gtk::Widget, x: i32, w: i32, top: i32, high: i32| {
                let _ = natural(child, gtk::Orientation::Horizontal);
                let h = natural(child, gtk::Orientation::Vertical);
                child.size_allocate(&gtk::Allocation::new(x, top + (high - h) / 2, w.max(1), h), -1);
            };
            let art = metrics.art();
            let _ = natural(self.thumb.upcast_ref(), gtk::Orientation::Horizontal);
            self.thumb.size_allocate(&gtk::Allocation::new(PAD, (height - art) / 2, art, art), -1);
            let more_x = width - PAD - BUTTON;
            put(self.more.upcast_ref(), more_x, BUTTON, 0, height);
            let text_x = PAD + art + GAP;
            let mut text_end = more_x - GAP;
            let dated = self.year.is_visible();
            if dated {
                let column = metrics.year_column();
                text_end -= column;
                put(self.year.upcast_ref(), text_end, column, 0, height);
                text_end -= GAP;
            }
            let text_width = (text_end - text_x).max(1);
            if self.line.is_visible() {
                let top = (height - metrics.title - metrics.line) / 2;
                put(self.title.upcast_ref(), text_x, text_width, top, metrics.title);
                put(self.line.upcast_ref(), text_x, text_width, top + metrics.title, metrics.line);
            } else {
                put(self.title.upcast_ref(), text_x, text_width, 0, height);
            }
        }
    }
}

glib::wrapper! {
    pub struct ItemRow(ObjectSubclass<imp::ItemRow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ItemRow {
    /// A row that asks `menu` for the menu of what it shows.
    pub fn new(menu: MenuHandler) -> ItemRow {
        let row: ItemRow = glib::Object::new();
        row.imp().menu.replace(Some(menu));
        row
    }

    pub fn item(&self) -> Option<Item> {
        self.imp().item.borrow().clone()
    }

    /// The sizes that come from the fonts of the labels, measured again when one of those fonts
    /// changes.
    pub fn metrics(&self) -> Metrics {
        let imp = self.imp();
        let serial = font_serial(&[imp.title.upcast_ref(), imp.line.upcast_ref(), imp.year.upcast_ref()]);
        if let Some((measured, metrics)) = imp.metrics.get()
            && measured == serial
        {
            return metrics;
        }
        let metrics = Metrics {
            title: label_line(&imp.title),
            line: label_line(&imp.line),
            year: label_width(&imp.year, "0000"),
        };
        imp.metrics.set(Some((serial, metrics)));
        metrics
    }

    /// Show `item` (or nothing).
    pub fn bind(&self, item: Option<&Item>) {
        let imp = self.imp();
        imp.item.replace(item.cloned());
        let Some(item) = item else {
            imp.thumb.set_artwork(None);
            imp.thumb.set_blank(None);
            return;
        };
        imp.thumb.set_round(matches!(item, Item::Artist(_)));
        imp.thumb.set_artwork(item.artwork());
        imp.thumb.set_blank(blank_icon(item));
        imp.title.set_text(item.title());
        let (line, released) = texts(item);
        imp.line.set_text(&line);
        imp.line.set_visible(!line.is_empty());
        imp.year.set_text(&released);
        imp.year.set_visible(!released.is_empty());
        let name = if line.is_empty() { item.title().to_string() } else { format!("{}, {line}", item.title()) };
        self.update_property(&[gtk::accessible::Property::Label(&name)]);
        self.queue_allocate();
    }

    /// How wide the row would like to be: less than in a list of its own where rows stand side
    /// by side (a flow box lays its children out at that width).
    pub fn set_natural_width(&self, width: i32) {
        self.imp().natural.set(width);
        self.queue_resize();
    }

    /// Say `line` under the title instead, without a year: in a list that mixes albums, artists
    /// and playlists, the line says what each is.
    pub fn set_line(&self, line: &str) {
        let imp = self.imp();
        imp.line.set_text(line);
        imp.line.set_visible(!line.is_empty());
        imp.year.set_visible(false);
        if let Some(item) = imp.item.borrow().as_ref() {
            self.update_property(&[gtk::accessible::Property::Label(&format!("{}, {line}", item.title()))]);
        }
        self.queue_allocate();
    }

    fn ask_menu(&self, x: f64, y: f64) {
        let menu = self.imp().menu.borrow().clone();
        let item = self.imp().item.borrow().clone();
        if let (Some(menu), Some(item)) = (menu, item) {
            menu(&item, self, x, y);
        }
    }
}

/// The line under the title of an item and the year at the end of its row (of an album that
/// says when it was released). An artist is a name and nothing else, and a playlist says who
/// made it only when that is not the user: every row of a list of playlists would say
/// "Playlist" otherwise.
fn texts(item: &Item) -> (String, String) {
    match item {
        Item::Artist(_) => (String::new(), String::new()),
        Item::Album(album) => (album.artist.clone(), album.release_date.as_deref().and_then(year).unwrap_or_default()),
        Item::Playlist(playlist) => (playlist.curator.clone().unwrap_or_default(), String::new()),
        _ => (item.subtitle(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use amui::types::{Album, Artist, Playlist};

    use super::*;

    #[test]
    fn a_row_is_as_large_as_its_text() {
        // this desktop's usual text size (10 points): the size the rows were drawn with
        let usual = Metrics { title: 17, line: 15, year: 32 };
        assert_eq!((usual.art(), usual.height(), usual.year_column()), (ART, 60, YEAR_WIDTH));
        // text twice as large: artwork as tall as the two lines, room for four digits
        let double = Metrics { title: 33, line: 30, year: 64 };
        assert_eq!((double.art(), double.height(), double.year_column()), (79, 91, 84));
    }

    #[test]
    fn what_a_row_says_about_its_item() {
        let album = Album { release_date: Some("2019-03-08".into()), ..Album::new(None, "Winter Signal", "Nova Vale") };
        assert_eq!(texts(&Item::Album(album.clone())), ("Nova Vale".to_string(), "2019".to_string()));
        // an album that does not say when it was released
        let undated = Album { release_date: None, ..album };
        assert_eq!(texts(&Item::Album(undated)), ("Nova Vale".to_string(), String::new()));
        // a playlist of the library, and one that names who made it
        assert_eq!(texts(&Item::Playlist(Playlist::new(Some("p.1"), "Amber Mix"))), (String::new(), String::new()));
        let curated = Playlist { curator: Some("Glass Orbit".into()), ..Playlist::new(None, "Quiet Hours") };
        assert_eq!(texts(&Item::Playlist(curated)), ("Glass Orbit".to_string(), String::new()));
        // an artist is a name and nothing else
        assert_eq!(texts(&Item::Artist(Artist::new(None, "Nova Vale"))), (String::new(), String::new()));
    }
}
