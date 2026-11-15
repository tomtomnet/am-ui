//! A card of a grid or a shelf: artwork with a title and a line below it, for an album, a
//! playlist, an artist or a station.
//!
//! One widget without children that draws everything in `snapshot()`: a grid recycles a whole row
//! of cards at a time while it scrolls, and cards built from labels cost too much there (measured
//! while the toolkit was chosen). Pointing at a card shows a play button and a "more" button on the
//! artwork. The text is in the card's font (its style's, which follows the desktop's text size):
//! the card is as tall as its artwork and two lines of that font.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};

use crate::art::{self, Key};
use crate::data::Item;
use crate::widgets::{font_layout, line_height, relative_font, scaled, with_alpha};

pub const MIN_WIDTH: i32 = 132;
pub const NATURAL_WIDTH: i32 = 176;
const TEXT_GAP: f32 = 8.0;
const BOTTOM: f32 = 6.0;
/// The sizes of the title, of the line under it and of the "LIVE" tag, in the card's font.
const TITLE_EM: f64 = 1.0;
const SUBTITLE_EM: f64 = 0.94;
const LIVE_EM: f64 = 0.75;
const BUTTON: f32 = 38.0;
const INSET: f32 = 10.0;
/// The tag on the artwork of a station that is on air: Apple Music's red.
const LIVE: gdk::RGBA = gdk::RGBA::new(0.98, 0.18, 0.28, 1.0);

/// What the user did with a card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CardAction {
    Open,
    Play,
    /// The context menu, at this point of the card.
    Menu(f64, f64),
}

pub type CardHandler = Rc<dyn Fn(&Item, CardAction, &Card)>;

/// The part of a card under the pointer.
#[derive(Clone, Copy, PartialEq)]
pub enum Zone {
    None,
    Art,
    Play,
    More,
}

mod imp {
    use super::*;

    pub struct Card {
        pub item: RefCell<Option<Item>>,
        pub handler: RefCell<Option<CardHandler>>,
        pub key: RefCell<Option<Key>>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub title: RefCell<Option<pango::Layout>>,
        pub subtitle: RefCell<Option<pango::Layout>>,
        /// "LIVE", when the card shows a station that is on air.
        pub live: RefCell<Option<pango::Layout>>,
        pub line_heights: Cell<(i32, i32)>,
        pub font_serial: Cell<u32>,
        pub zone: Cell<Zone>,
        /// The width of a card that has one of its own (in a shelf); 0 for a card as wide as
        /// its place (in a grid).
        pub fixed_width: Cell<i32>,
    }

    impl Default for Card {
        fn default() -> Self {
            Card {
                item: RefCell::default(),
                handler: RefCell::default(),
                key: RefCell::default(),
                texture: RefCell::default(),
                title: RefCell::default(),
                subtitle: RefCell::default(),
                live: RefCell::default(),
                line_heights: Cell::new((0, 0)),
                font_serial: Cell::new(0),
                zone: Cell::new(Zone::None),
                fixed_width: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Card {
        const NAME: &'static str = "AmCard";
        type Type = super::Card;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("card");
            class.set_accessible_role(gtk::AccessibleRole::Button);
        }
    }

    impl ObjectImpl for Card {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_motion(move |_, x, y| {
                if let Some(card) = weak.upgrade() {
                    card.point(Some((x as f32, y as f32)));
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(card) = weak.upgrade() {
                    card.point(None);
                }
            });
            obj.add_controller(motion);

            let click = gtk::GestureClick::new();
            click.set_button(0);
            let weak = obj.downgrade();
            click.connect_released(move |gesture, _, x, y| {
                let Some(card) = weak.upgrade() else { return };
                if !card.contains(x, y) {
                    return;
                }
                let action = match (gesture.current_button(), card.zone_at(x as f32, y as f32)) {
                    (gdk::BUTTON_SECONDARY, _) | (_, Zone::More) => CardAction::Menu(x, y),
                    (gdk::BUTTON_PRIMARY, Zone::Play) => CardAction::Play,
                    (gdk::BUTTON_PRIMARY, _) => CardAction::Open,
                    _ => return,
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                card.emit(action);
            });
            obj.add_controller(click);
        }
    }

    impl WidgetImpl for Card {
        // A grid keeps many more cards bound than it shows and unmaps those outside the
        // viewport: artwork is asked for on map and released on unmap.
        fn map(&self) {
            self.parent_map();
            self.obj().load();
        }

        fn unmap(&self) {
            self.obj().release();
            self.parent_unmap();
        }

        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            // as wide as it is at the usual text size times the desktop's text scaling: a card
            // shows as much of its title with larger text
            let obj = self.obj();
            let fixed = scaled(&*obj, self.fixed_width.get());
            if orientation == gtk::Orientation::Horizontal {
                return if fixed > 0 {
                    (fixed, fixed, -1, -1)
                } else {
                    (scaled(&*obj, MIN_WIDTH), scaled(&*obj, NATURAL_WIDTH), -1, -1)
                };
            }
            let width = match (fixed, for_size) {
                (1.., _) => fixed,
                (_, 1..) => for_size,
                _ => scaled(&*obj, NATURAL_WIDTH),
            };
            let height = width + self.obj().text_height();
            (height, height, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            for layout in [&self.title, &self.subtitle] {
                if let Some(layout) = layout.borrow().as_ref() {
                    layout.set_width(width * pango::SCALE);
                }
            }
            if self.obj().is_mapped() {
                self.obj().load();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let item = self.item.borrow();
            let Some(item) = item.as_ref() else { return };
            let side = obj.width() as f32;
            let colour = obj.color();
            let round = matches!(item, Item::Artist(_));
            let bounds = graphene::Rect::new(0.0, 0.0, side, side);
            let shape = gsk::RoundedRect::from_rect(bounds, if round { side / 2.0 } else { 9.0 });
            snapshot.append_outset_shadow(&shape, &gdk::RGBA::new(0.0, 0.0, 0.0, 0.28), 0.0, 3.0, 0.0, 9.0);
            snapshot.push_rounded_clip(&shape);
            match self.texture.borrow().as_ref() {
                Some(texture) => snapshot.append_texture(texture, &bounds),
                None => snapshot.append_color(&with_alpha(&colour, 0.08), &bounds),
            }
            let zone = self.zone.get();
            if zone != Zone::None && !round {
                snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.22), &bounds);
            }
            snapshot.pop();
            if let Some(live) = self.live.borrow().as_ref() {
                draw_live_tag(snapshot, live);
            }
            if zone != Zone::None && !round {
                let (play, more) = buttons(side);
                draw_button(snapshot, &play, zone == Zone::Play, Glyph::Play);
                draw_button(snapshot, &more, zone == Zone::More, Glyph::More);
            }

            obj.ensure_layouts();
            let (title_height, _) = self.line_heights.get();
            snapshot.save();
            snapshot.translate(&graphene::Point::new(0.0, side + TEXT_GAP));
            if let Some(title) = self.title.borrow().as_ref() {
                snapshot.append_layout(title, &colour);
            }
            snapshot.translate(&graphene::Point::new(0.0, title_height as f32));
            if let Some(subtitle) = self.subtitle.borrow().as_ref() {
                snapshot.append_layout(subtitle, &with_alpha(&colour, 0.6));
            }
            snapshot.restore();
        }
    }
}

enum Glyph {
    Play,
    More,
}

/// The play button (bottom left) and the "more" button (bottom right) on an artwork of `side`.
fn buttons(side: f32) -> (graphene::Rect, graphene::Rect) {
    let y = side - INSET - BUTTON;
    (graphene::Rect::new(INSET, y, BUTTON, BUTTON), graphene::Rect::new(side - INSET - BUTTON, y, BUTTON, BUTTON))
}

fn draw_button(snapshot: &gtk::Snapshot, bounds: &graphene::Rect, hot: bool, glyph: Glyph) {
    let circle = gsk::RoundedRect::from_rect(*bounds, BUTTON / 2.0);
    snapshot.push_rounded_clip(&circle);
    let fill = if hot { gdk::RGBA::new(0.98, 0.18, 0.28, 1.0) } else { gdk::RGBA::new(0.12, 0.12, 0.13, 0.78) };
    snapshot.append_color(&fill, bounds);
    snapshot.pop();
    let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
    let (cx, cy) = (bounds.x() + BUTTON / 2.0, bounds.y() + BUTTON / 2.0);
    match glyph {
        Glyph::Play => {
            let builder = gsk::PathBuilder::new();
            builder.move_to(cx - 5.0, cy - 8.0);
            builder.line_to(cx + 9.0, cy);
            builder.line_to(cx - 5.0, cy + 8.0);
            builder.close();
            snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, &white);
        }
        Glyph::More => {
            for dx in [-7.0, 0.0, 7.0] {
                let dot = graphene::Rect::new(cx + dx - 2.0, cy - 2.0, 4.0, 4.0);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(dot, 2.0));
                snapshot.append_color(&white, &dot);
                snapshot.pop();
            }
        }
    }
}

/// The "LIVE" tag in the top left corner of a station's artwork.
fn draw_live_tag(snapshot: &gtk::Snapshot, text: &pango::Layout) {
    let (width, height) = text.pixel_size();
    let tag = graphene::Rect::new(INSET - 2.0, INSET - 2.0, width as f32 + 12.0, height as f32 + 4.0);
    snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(tag, 4.5));
    snapshot.append_color(&LIVE, &tag);
    snapshot.pop();
    snapshot.save();
    snapshot.translate(&graphene::Point::new(tag.x() + 6.0, tag.y() + 2.0));
    snapshot.append_layout(text, &gdk::RGBA::new(1.0, 1.0, 1.0, 1.0));
    snapshot.restore();
}

glib::wrapper! {
    pub struct Card(ObjectSubclass<imp::Card>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Card {
    fn default() -> Self {
        Card::new()
    }
}

impl Card {
    pub fn new() -> Card {
        glib::Object::new()
    }

    pub fn set_handler(&self, handler: CardHandler) {
        self.imp().handler.replace(Some(handler));
    }

    /// Give the card a width of its own (a card of a shelf) instead of that of its place.
    pub fn set_fixed_width(&self, width: i32) {
        if self.imp().fixed_width.replace(width) != width {
            self.queue_resize();
        }
    }

    pub fn item(&self) -> Option<Item> {
        self.imp().item.borrow().clone()
    }

    pub fn has_texture(&self) -> bool {
        self.imp().texture.borrow().is_some()
    }

    /// Show `item` (or nothing).
    pub fn bind(&self, item: Option<&Item>) {
        let imp = self.imp();
        self.release();
        imp.item.replace(item.cloned());
        imp.zone.set(Zone::None);
        if let Some(item) = item {
            self.set_texts();
            self.update_property(&[gtk::accessible::Property::Label(&format!(
                "{}, {}",
                item.title(),
                item.subtitle()
            ))]);
            if self.is_mapped() {
                self.load();
            }
        }
        self.queue_draw();
    }

    /// Put the item's title and line into the text layouts (created here when missing).
    fn set_texts(&self) {
        let imp = self.imp();
        let item = imp.item.borrow();
        let Some(item) = item.as_ref() else { return };
        self.ensure_layouts();
        let round = matches!(item, Item::Artist(_));
        if let Some(title) = imp.title.borrow().as_ref() {
            title.set_text(item.title());
            title.set_alignment(if round { pango::Alignment::Center } else { pango::Alignment::Left });
        }
        if let Some(subtitle) = imp.subtitle.borrow().as_ref() {
            subtitle.set_text(&if round { String::new() } else { item.subtitle() });
        }
        let live = matches!(item, Item::Station(station) if station.live);
        imp.live.replace(live.then(|| {
            let tag = font_layout(self, LIVE_EM, pango::Weight::Bold, 0.0);
            tag.set_text("LIVE");
            tag
        }));
    }

    fn ensure_layouts(&self) {
        let imp = self.imp();
        if imp.title.borrow().is_none() {
            let width = self.width().max(NATURAL_WIDTH) as f32;
            imp.title.replace(Some(font_layout(self, TITLE_EM, pango::Weight::Medium, width)));
            imp.subtitle.replace(Some(font_layout(self, SUBTITLE_EM, pango::Weight::Normal, width)));
        }
        // the font can change after the layouts were made (the style arrives, the desktop's text
        // size or font changes): the layouts take its new size
        let serial = self.pango_context().serial();
        if imp.font_serial.replace(serial) != serial || imp.line_heights.get().0 == 0 {
            for (layout, em, weight) in [
                (&imp.title, TITLE_EM, pango::Weight::Medium),
                (&imp.subtitle, SUBTITLE_EM, pango::Weight::Normal),
                (&imp.live, LIVE_EM, pango::Weight::Bold),
            ] {
                if let Some(layout) = layout.borrow().as_ref() {
                    layout.set_font_description(Some(&relative_font(self, em, weight)));
                    layout.context_changed();
                }
            }
            imp.line_heights.set((
                line_height(self, TITLE_EM, pango::Weight::Medium),
                line_height(self, SUBTITLE_EM, pango::Weight::Normal),
            ));
        }
    }

    fn text_height(&self) -> i32 {
        self.ensure_layouts();
        let (title, subtitle) = self.imp().line_heights.get();
        (TEXT_GAP + BOTTOM) as i32 + title + subtitle
    }

    fn zone_at(&self, x: f32, y: f32) -> Zone {
        let side = self.width() as f32;
        if y > side || matches!(self.imp().item.borrow().as_ref(), Some(Item::Artist(_)) | None) {
            return if y <= side { Zone::Art } else { Zone::None };
        }
        let (play, more) = buttons(side);
        let point = graphene::Point::new(x, y);
        if play.contains_point(&point) {
            Zone::Play
        } else if more.contains_point(&point) {
            Zone::More
        } else {
            Zone::Art
        }
    }

    fn point(&self, at: Option<(f32, f32)>) {
        let zone = at.map(|(x, y)| self.zone_at(x, y)).unwrap_or(Zone::None);
        if self.imp().zone.replace(zone) != zone {
            self.queue_draw();
        }
    }

    fn emit(&self, action: CardAction) {
        let handler = self.imp().handler.borrow().clone();
        let item = self.imp().item.borrow().clone();
        if let (Some(handler), Some(item)) = (handler, item) {
            handler(&item, action, self);
        }
    }

    fn wanted(&self) -> Option<Key> {
        let side = self.width();
        let item = self.imp().item.borrow();
        if side <= 0 {
            return None;
        }
        Some(Key::new(item.as_ref()?.artwork()?, (side * self.scale_factor()) as u32))
    }

    fn load(&self) {
        let imp = self.imp();
        let wanted = self.wanted();
        if *imp.key.borrow() == wanted && (imp.texture.borrow().is_some() || wanted.is_none()) {
            return;
        }
        // keep showing what is there while the new size loads
        let previous = imp.texture.borrow().clone();
        self.release();
        let Some(key) = wanted else { return };
        let weak = self.downgrade();
        let found = art::loader().request(
            &key,
            self.as_ptr() as usize,
            Box::new(move |ready, texture| {
                if let Some(card) = weak.upgrade()
                    && card.imp().key.borrow().as_ref() == Some(ready)
                {
                    card.imp().texture.replace(Some(texture.clone()));
                    card.queue_draw();
                }
            }),
        );
        let same_art = previous.is_some() && imp.key.borrow().is_none();
        imp.key.replace(Some(key));
        imp.texture.replace(found.or(if same_art { previous } else { None }));
        self.queue_draw();
    }

    fn release(&self) {
        let imp = self.imp();
        if let Some(key) = imp.key.take() {
            art::loader().cancel(&key, self.as_ptr() as usize);
        }
        imp.texture.replace(None);
    }
}
