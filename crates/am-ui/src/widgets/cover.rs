//! The picture at the top of a page: the artwork of an album or a playlist, four artworks as a
//! mosaic for a playlist that has none of its own, the round picture of an artist, or a neutral
//! tile with initials or an icon when there is nothing to show.
//!
//! Like `Thumb` it asks for its artwork at the size it is drawn when it becomes visible, and
//! lets go of it when it is hidden.

use std::cell::{Cell, RefCell};

use amui::types::Artwork;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};

use crate::art::{self, Key};
use crate::widgets::{text_layout, with_alpha};

/// What a cover without artwork shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Blank {
    #[default]
    Nothing,
    /// A symbolic icon.
    Icon(&'static str),
    /// The initials of a name.
    Initials(String),
}

/// One artwork of a cover and what the loader has of it.
pub struct Tile {
    artwork: Artwork,
    key: Option<Key>,
    texture: Option<gdk::Texture>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Cover {
        pub tiles: RefCell<Vec<Tile>>,
        pub blank: RefCell<Blank>,
        pub size: Cell<i32>,
        pub round: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Cover {
        const NAME: &'static str = "AmCover";
        type Type = super::Cover;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("cover");
            class.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for Cover {}

    impl WidgetImpl for Cover {
        fn map(&self) {
            self.parent_map();
            self.obj().load();
        }

        fn unmap(&self) {
            self.obj().release();
            self.parent_unmap();
        }

        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = self.size.get();
            (size, size, -1, -1)
        }

        fn size_allocate(&self, _width: i32, _height: i32, _baseline: i32) {
            if self.obj().is_mapped() {
                self.obj().load();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let side = obj.width().min(obj.height()) as f32;
            if side <= 0.0 {
                return;
            }
            let colour = obj.color();
            let bounds = graphene::Rect::new(0.0, 0.0, side, side);
            let radius = if self.round.get() { side / 2.0 } else { (side * 0.04).clamp(5.0, 11.0) };
            let shape = gsk::RoundedRect::from_rect(bounds, radius);
            snapshot.append_outset_shadow(
                &shape,
                &gdk::RGBA::new(0.0, 0.0, 0.0, 0.34),
                0.0,
                side * 0.03,
                0.0,
                side * 0.09,
            );
            snapshot.push_rounded_clip(&shape);
            snapshot.append_color(&with_alpha(&colour, 0.08), &bounds);
            let tiles = self.tiles.borrow();
            match tiles.len() {
                0 => obj.draw_blank(snapshot, side, &colour),
                1 => {
                    if let Some(texture) = &tiles[0].texture {
                        snapshot.append_texture(texture, &bounds);
                    }
                }
                _ => {
                    let half = side / 2.0;
                    for (n, tile) in tiles.iter().take(4).enumerate() {
                        if let Some(texture) = &tile.texture {
                            let (column, row) = ((n % 2) as f32, (n / 2) as f32);
                            // a hair of overlap: no seam between the tiles at odd sizes
                            let area = graphene::Rect::new(column * half, row * half, half + 0.5, half + 0.5);
                            snapshot.append_texture(texture, &area);
                        }
                    }
                }
            }
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct Cover(ObjectSubclass<imp::Cover>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Cover {
    /// A cover that asks for `size` logical pixels (its parent may give it more).
    pub fn new(size: i32) -> Cover {
        let cover: Cover = glib::Object::new();
        cover.imp().size.set(size);
        cover
    }

    /// Draw a circle (artists).
    pub fn set_round(&self, round: bool) {
        self.imp().round.set(round);
        self.queue_draw();
    }

    /// What to show while there is no artwork.
    pub fn set_blank(&self, blank: Blank) {
        self.imp().blank.replace(blank);
        self.queue_draw();
    }

    /// One artwork, or four as a mosaic (two or three show the first; none shows the blank).
    pub fn set_artworks(&self, artworks: &[Artwork]) {
        self.release();
        let shown = if artworks.len() >= 4 { &artworks[..4] } else { &artworks[..artworks.len().min(1)] };
        let tiles = shown.iter().map(|artwork| Tile { artwork: artwork.clone(), key: None, texture: None }).collect();
        self.imp().tiles.replace(tiles);
        if self.is_mapped() {
            self.load();
        }
        self.queue_draw();
    }

    fn draw_blank(&self, snapshot: &gtk::Snapshot, side: f32, colour: &gdk::RGBA) {
        match &*self.imp().blank.borrow() {
            Blank::Nothing => {}
            Blank::Initials(text) => {
                let layout = text_layout(self, (side * 0.36) as f64, pango::Weight::Bold, 0.0);
                layout.set_text(text);
                let (width, height) = layout.pixel_size();
                snapshot.save();
                snapshot.translate(&graphene::Point::new((side - width as f32) / 2.0, (side - height as f32) / 2.0));
                snapshot.append_layout(&layout, &with_alpha(colour, 0.5));
                snapshot.restore();
            }
            Blank::Icon(name) => {
                let size = (side * 0.34).round();
                let icon = gtk::IconTheme::for_display(&self.display()).lookup_icon(
                    name,
                    &[],
                    size as i32,
                    self.scale_factor(),
                    self.direction(),
                    gtk::IconLookupFlags::FORCE_SYMBOLIC,
                );
                snapshot.save();
                snapshot.translate(&graphene::Point::new((side - size) / 2.0, (side - size) / 2.0));
                icon.snapshot_symbolic(snapshot, size as f64, size as f64, &[with_alpha(colour, 0.4)]);
                snapshot.restore();
            }
        }
    }

    /// Ask for the artwork at the size it is drawn now.
    fn load(&self) {
        let side = self.width().min(self.height()).max(self.imp().size.get());
        if side <= 0 {
            return;
        }
        let owner = self.as_ptr() as usize;
        let mut tiles = self.imp().tiles.borrow_mut();
        let px = (if tiles.len() > 1 { (side + 1) / 2 } else { side } * self.scale_factor()) as u32;
        for tile in tiles.iter_mut() {
            let wanted = Key::new(&tile.artwork, px);
            if tile.key.as_ref() == Some(&wanted) {
                continue;
            }
            if let Some(old) = tile.key.take() {
                art::loader().cancel(&old, owner);
            }
            let weak = self.downgrade();
            let found = art::loader().request(
                &wanted,
                owner,
                Box::new(move |ready, texture| {
                    let Some(cover) = weak.upgrade() else { return };
                    for tile in cover.imp().tiles.borrow_mut().iter_mut() {
                        if tile.key.as_ref() == Some(ready) {
                            tile.texture = Some(texture.clone());
                        }
                    }
                    cover.queue_draw();
                }),
            );
            tile.key = Some(wanted);
            // what is there stays until the new size has arrived
            if found.is_some() {
                tile.texture = found;
            }
        }
        drop(tiles);
        self.queue_draw();
    }

    /// Let go of the textures (the loader keeps recent ones) and of requests not yet answered.
    fn release(&self) {
        let owner = self.as_ptr() as usize;
        for tile in self.imp().tiles.borrow_mut().iter_mut() {
            if let Some(key) = tile.key.take() {
                art::loader().cancel(&key, owner);
            }
            tile.texture = None;
        }
    }
}

/// The initials shown for a name without a picture: the first letter of its first and of its
/// last word ("Nova Vale" → "NV", "Atlas & The Drift" → "AD").
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|word| word.chars().any(char::is_alphanumeric)).collect();
    let letter = |word: &&str| word.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string());
    let first = words.first().and_then(letter).unwrap_or_default();
    let last = if words.len() > 1 { words.last().and_then(letter).unwrap_or_default() } else { String::new() };
    format!("{first}{last}")
}

/// The artworks a playlist without one of its own is shown with: those of the first four
/// different albums among its songs.
pub fn mosaic<'a>(artworks: impl Iterator<Item = Option<&'a Artwork>>) -> Vec<Artwork> {
    let mut chosen: Vec<Artwork> = Vec::new();
    for artwork in artworks.flatten() {
        if !chosen.iter().any(|other| other.url_template == artwork.url_template) {
            chosen.push(artwork.clone());
            if chosen.len() == 4 {
                break;
            }
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_of_names() {
        assert_eq!(initials("Nova Vale"), "NV");
        assert_eq!(initials("Atlas & The Drift"), "AD");
        assert_eq!(initials("cascade"), "C");
        assert_eq!(initials("  "), "");
        assert_eq!(initials("& Co."), "C");
        assert_eq!(initials("夏の終わり Trio"), "夏T");
    }

    #[test]
    fn mosaic_takes_the_first_four_different_artworks() {
        let art = |n: u32| Artwork::new(format!("file:///covers/{n:04}.jpg"));
        let songs =
            [Some(art(1)), Some(art(1)), None, Some(art(2)), Some(art(3)), Some(art(2)), Some(art(4)), Some(art(5))];
        let chosen = mosaic(songs.iter().map(Option::as_ref));
        assert_eq!(chosen, vec![art(1), art(2), art(3), art(4)]);
        // fewer than four albums: what there is, in order
        let few = [Some(art(7)), Some(art(7)), Some(art(8))];
        assert_eq!(mosaic(few.iter().map(Option::as_ref)), vec![art(7), art(8)]);
        assert!(mosaic([None, None].into_iter()).is_empty());
    }
}
