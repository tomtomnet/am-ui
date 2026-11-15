//! A piece of artwork with rounded corners (or round, for artists) that loads when it becomes
//! visible and lets go of its texture when it is hidden.

use std::cell::{Cell, RefCell};

use amui::types::Artwork;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::art::{self, Key};
use crate::widgets::with_alpha;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Thumb {
        pub artwork: RefCell<Option<Artwork>>,
        pub key: RefCell<Option<Key>>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub size: Cell<i32>,
        pub radius: Cell<f32>,
        pub round: Cell<bool>,
        pub shadow: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Thumb {
        const NAME: &'static str = "AmThumb";
        type Type = super::Thumb;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("thumb");
            class.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for Thumb {}

    impl WidgetImpl for Thumb {
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
            let bounds = graphene::Rect::new(0.0, 0.0, side, side);
            let radius = if self.round.get() { side / 2.0 } else { self.radius.get() };
            let shape = gsk::RoundedRect::from_rect(bounds, radius);
            if self.shadow.get() {
                snapshot.append_outset_shadow(
                    &shape,
                    &gdk::RGBA::new(0.0, 0.0, 0.0, 0.35),
                    0.0,
                    side * 0.03,
                    0.0,
                    side * 0.08,
                );
            }
            snapshot.push_rounded_clip(&shape);
            match self.texture.borrow().as_ref() {
                Some(texture) => snapshot.append_texture(texture, &bounds),
                None => snapshot.append_color(&with_alpha(&obj.color(), 0.08), &bounds),
            }
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct Thumb(ObjectSubclass<imp::Thumb>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Thumb {
    /// A square of `size` logical pixels with corners of `radius`.
    pub fn new(size: i32, radius: f32) -> Thumb {
        let thumb: Thumb = glib::Object::new();
        thumb.imp().size.set(size);
        thumb.imp().radius.set(radius);
        thumb
    }

    /// Draw the artwork as a circle (artists).
    pub fn set_round(&self, round: bool) {
        self.imp().round.set(round);
        self.queue_draw();
    }

    pub fn set_shadow(&self, shadow: bool) {
        self.imp().shadow.set(shadow);
        self.queue_draw();
    }

    pub fn set_size(&self, size: i32) {
        if self.imp().size.replace(size) != size {
            self.queue_resize();
        }
    }

    pub fn has_texture(&self) -> bool {
        self.imp().texture.borrow().is_some()
    }

    pub fn set_artwork(&self, artwork: Option<&Artwork>) {
        let imp = self.imp();
        if imp.artwork.borrow().as_ref() == artwork {
            return;
        }
        self.release();
        imp.artwork.replace(artwork.cloned());
        if self.is_mapped() {
            self.load();
        }
        self.queue_draw();
    }

    fn wanted(&self) -> Option<Key> {
        let side = self.width().min(self.height()).max(self.imp().size.get());
        let artwork = self.imp().artwork.borrow();
        if side <= 0 {
            return None;
        }
        Some(Key::new(artwork.as_ref()?, (side * self.scale_factor()) as u32))
    }

    fn load(&self) {
        let imp = self.imp();
        let wanted = self.wanted();
        if *imp.key.borrow() == wanted && (imp.texture.borrow().is_some() || wanted.is_none()) {
            return;
        }
        self.release();
        let Some(key) = wanted else { return };
        let weak = self.downgrade();
        let found = art::loader().request(
            &key,
            self.as_ptr() as usize,
            Box::new(move |ready, texture| {
                if let Some(thumb) = weak.upgrade()
                    && thumb.imp().key.borrow().as_ref() == Some(ready)
                {
                    thumb.imp().texture.replace(Some(texture.clone()));
                    thumb.queue_draw();
                }
            }),
        );
        imp.key.replace(Some(key));
        if found.is_some() {
            imp.texture.replace(found);
            self.queue_draw();
        }
    }

    /// Drop the texture (the loader keeps recent ones) and a request that has not started.
    fn release(&self) {
        let imp = self.imp();
        if let Some(key) = imp.key.take()
            && imp.texture.borrow().is_none()
        {
            art::loader().cancel(&key, self.as_ptr() as usize);
        }
        imp.texture.replace(None);
    }
}
