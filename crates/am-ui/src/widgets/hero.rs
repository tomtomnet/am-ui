//! The header of an album's, a playlist's or an artist's page: a picture with what there is to
//! say and to do beside it.
//!
//! The picture is as large as the page allows (up to its largest size) and the text takes the
//! rest; in a window too narrow for both, the picture goes on top and the text below it.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Space left and right of the header: where the rows of a song list begin.
pub const MARGIN: i32 = 26;
const TOP: i32 = 20;
const BOTTOM: i32 = 22;
const GAP: i32 = 30;
/// Space between the picture and the text when they are stacked.
const STACK_GAP: i32 = 18;
/// The part of the width the picture takes while it is smaller than its largest size.
const SHARE: f32 = 0.36;

/// How a header of `width` is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The side of the picture.
    pub picture: i32,
    /// The width of the text.
    pub text: i32,
    /// The picture is above the text, not beside it.
    pub stacked: bool,
}

/// The layout for a header of `width` whose picture may be `smallest` to `largest` pixels and
/// whose text needs `text_minimum`.
pub fn layout(width: i32, smallest: i32, largest: i32, text_minimum: i32) -> Layout {
    let inner = (width - 2 * MARGIN).max(1);
    let picture = ((inner as f32 * SHARE) as i32).clamp(smallest, largest);
    let text = inner - picture - GAP;
    if text >= text_minimum {
        Layout { picture, text, stacked: false }
    } else {
        // on top of the text the picture can be larger again
        Layout { picture: (inner * 11 / 20).clamp(smallest, largest).min(inner), text: inner, stacked: true }
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Hero {
        pub picture: RefCell<Option<gtk::Widget>>,
        pub text: RefCell<Option<gtk::Widget>>,
        pub smallest: Cell<i32>,
        pub largest: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Hero {
        const NAME: &'static str = "AmHero";
        type Type = super::Hero;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("hero");
        }
    }

    impl ObjectImpl for Hero {
        fn dispose(&self) {
            for child in [self.picture.take(), self.text.take()].into_iter().flatten() {
                child.unparent();
            }
        }
    }

    impl Hero {
        fn text_minimum(&self) -> i32 {
            self.text.borrow().as_ref().map(|text| text.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0)
        }

        fn layout(&self, width: i32) -> Layout {
            layout(width, self.smallest.get(), self.largest.get(), self.text_minimum())
        }

        fn text_height(&self, width: i32) -> i32 {
            self.text.borrow().as_ref().map(|text| text.measure(gtk::Orientation::Vertical, width).1).unwrap_or(0)
        }
    }

    impl WidgetImpl for Hero {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let (smallest, largest) = (self.smallest.get(), self.largest.get());
            if orientation == gtk::Orientation::Horizontal {
                let text = self.text.borrow().as_ref().map(|text| text.measure(orientation, -1));
                let (text_minimum, text_natural) =
                    text.map(|(minimum, natural, _, _)| (minimum, natural)).unwrap_or((0, 0));
                // stacked, the header is as narrow as the wider of its two parts
                let minimum = text_minimum.max(smallest) + 2 * MARGIN;
                let natural = largest + GAP + text_natural + 2 * MARGIN;
                return (minimum, natural.max(minimum), -1, -1);
            }
            let width = if for_size > 0 { for_size } else { self.measure(gtk::Orientation::Horizontal, -1).1 };
            let layout = self.layout(width);
            let text = self.text_height(layout.text);
            let content = if layout.stacked { layout.picture + STACK_GAP + text } else { layout.picture.max(text) };
            let height = TOP + content + BOTTOM;
            (height, height, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let layout = self.layout(width);
            let text_height = self.text_height(layout.text);
            let (picture, text) = (self.picture.borrow(), self.text.borrow());
            if let Some(picture) = picture.as_ref() {
                // every child is measured before it is placed, as GTK asks
                let _ = picture.measure(gtk::Orientation::Horizontal, -1);
            }
            if layout.stacked {
                if let Some(picture) = picture.as_ref() {
                    let x = (width - layout.picture) / 2;
                    picture.size_allocate(&gtk::Allocation::new(x, TOP, layout.picture, layout.picture), -1);
                }
                if let Some(text) = text.as_ref() {
                    let y = TOP + layout.picture + STACK_GAP;
                    text.size_allocate(&gtk::Allocation::new(MARGIN, y, layout.text, text_height), -1);
                }
            } else {
                if let Some(picture) = picture.as_ref() {
                    picture.size_allocate(&gtk::Allocation::new(MARGIN, TOP, layout.picture, layout.picture), -1);
                }
                if let Some(text) = text.as_ref() {
                    // as tall as the picture at least: what is to be done sits at its foot
                    let x = MARGIN + layout.picture + GAP;
                    let height = text_height.max(layout.picture);
                    text.size_allocate(&gtk::Allocation::new(x, TOP, layout.text, height), -1);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct Hero(ObjectSubclass<imp::Hero>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Hero {
    /// A header with `picture` drawn `smallest` to `largest` pixels wide and `text` beside it.
    pub fn new(picture: &impl IsA<gtk::Widget>, text: &impl IsA<gtk::Widget>, smallest: i32, largest: i32) -> Hero {
        let hero: Hero = glib::Object::new();
        let imp = hero.imp();
        imp.smallest.set(smallest);
        imp.largest.set(largest);
        picture.set_parent(&hero);
        text.set_parent(&hero);
        imp.picture.replace(Some(picture.clone().upcast()));
        imp.text.replace(Some(text.clone().upcast()));
        hero
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::widgets::tests::Stage;

    /// A header in a wide and in a narrow window (part of
    /// `widgets::tests::widgets_on_a_display`).
    pub(crate) fn on_a_display() {
        let block = |width: i32, height: i32| {
            let block = gtk::Box::new(gtk::Orientation::Vertical, 0);
            block.set_size_request(width, height);
            block
        };
        // a text that needs 300 px of width and is shorter than the picture
        let (picture, text) = (block(-1, -1), block(300, 120));
        let hero = Hero::new(&picture, &text, 180, 264);
        hero.set_valign(gtk::Align::Start);
        let stage = Stage::new(&hero, 900, 500);
        let place = |widget: &gtk::Box| {
            let bounds = widget.compute_bounds(&hero).unwrap();
            (bounds.x() as i32, bounds.y() as i32, bounds.width() as i32, bounds.height() as i32)
        };
        // wide: the picture at its largest, the text beside it and as tall as it
        let wide = layout(hero.width(), 180, 264, 300);
        assert!(!wide.stacked && wide.picture == 264, "{wide:?} in a header of {}", hero.width());
        assert_eq!(place(&picture), (MARGIN, TOP, 264, 264));
        assert_eq!(place(&text), (MARGIN + 264 + GAP, TOP, wide.text, 264));
        assert_eq!(hero.height(), TOP + 264 + BOTTOM);
        // narrow: the picture in the middle, the text below it and as wide as the page
        stage.window.set_default_size(420, 700);
        stage.frames(6);
        let narrow = layout(hero.width(), 180, 264, 300);
        assert!(narrow.stacked, "{narrow:?} in a header of {}", hero.width());
        assert_eq!(place(&picture), ((hero.width() - narrow.picture) / 2, TOP, narrow.picture, narrow.picture));
        assert_eq!(place(&text), (MARGIN, TOP + narrow.picture + STACK_GAP, hero.width() - 2 * MARGIN, 120));
        assert_eq!(hero.height(), TOP + narrow.picture + STACK_GAP + 120 + BOTTOM);
    }

    #[test]
    fn the_picture_gives_way_to_the_text() {
        // a wide page: the largest picture
        assert_eq!(layout(1180, 180, 264, 330), Layout { picture: 264, text: 834, stacked: false });
        // a narrower one: a smaller picture, the text beside it
        assert_eq!(layout(675, 180, 264, 330), Layout { picture: 224, text: 369, stacked: false });
        // too narrow for both: the picture on top, the text as wide as the page
        assert_eq!(layout(600, 180, 264, 330), Layout { picture: 264, text: 548, stacked: true });
        assert_eq!(layout(480, 180, 264, 330), Layout { picture: 235, text: 428, stacked: true });
        assert_eq!(layout(300, 180, 264, 200), Layout { picture: 180, text: 248, stacked: true });
        // never wider than the page
        assert_eq!(layout(200, 180, 264, 200), Layout { picture: 148, text: 148, stacked: true });
    }
}
