//! The glow behind Now Playing: soft blobs of the artwork's own colours, drifting slowly over a
//! dark base.
//!
//! Nothing is blurred and no gradient is drawn. The glow is a texture of 160 x 90 pixels computed
//! on the CPU, 8 bits per channel with an ordered dither, and drawn scaled up to the view: a glow
//! has no detail that a small image cannot carry, and this form costs next to nothing per frame
//! (measured while the toolkit was chosen). The texture is computed again about 30
//! times a second while the blobs drift or the colours change to those of another artwork, and
//! not at all while the view is hidden, nothing plays or the desktop asks for reduced motion.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::widgets::activity;

pub type Colour = [f32; 3];

/// The colours a glow is made of: one for each blob.
pub const COLOURS: usize = 5;
/// The side of the square an artwork is scaled to before its colours are counted.
pub const SAMPLE: u32 = 48;
const WIDTH: usize = 160;
const HEIGHT: usize = 90;
/// The glow is computed again this often while it changes.
const STEP: Duration = Duration::from_millis(33);
/// Seconds a change to the colours of another artwork takes.
const FADE: f32 = 0.6;
/// White text has to stay readable on the glow: no colour of it is brighter than this.
const BRIGHTEST: f32 = 0.5;
/// Two colours closer than this (squared distance) count as the same colour of an artwork.
const DISTINCT: f32 = 0.03;

// centre (as fractions of the width and height), radius (as a fraction of the diagonal) and the
// periods in seconds of the drift along x and along y
const BLOBS: [(f32, f32, f32, f32, f32); COLOURS] = [
    (0.22, 0.28, 0.55, 23.0, 19.0),
    (0.78, 0.24, 0.50, 29.0, 17.0),
    (0.30, 0.80, 0.52, 19.0, 31.0),
    (0.74, 0.76, 0.58, 37.0, 23.0),
    (0.50, 0.50, 0.45, 27.0, 33.0),
];

/// The glow of a song without artwork.
pub fn neutral() -> Vec<Colour> {
    vec![[0.30, 0.30, 0.34], [0.24, 0.25, 0.30], [0.34, 0.32, 0.36], [0.22, 0.24, 0.27], [0.28, 0.28, 0.33]]
}

fn brightness(colour: Colour) -> f32 {
    0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2]
}

/// The colour itself, or a darker one of the same hue when it is too bright for white text.
pub fn tame(colour: Colour) -> Colour {
    let bright = brightness(colour);
    if bright <= BRIGHTEST { colour } else { colour.map(|channel| channel * BRIGHTEST / bright) }
}

/// `count` colours of an image (RGBA, four bytes per pixel) for its glow: what there is most of
/// and what is most colourful first, greys, near-black and near-white only when nothing else is
/// left. An image with fewer distinct colours gives them in turn, one without pixels gives greys.
pub fn palette(rgba: &[u8], count: usize) -> Vec<Colour> {
    // the pixels counted in a coarse histogram of four bits per channel
    let mut buckets = vec![(0u32, [0u32; 3]); 4096];
    for [red, green, blue, _] in rgba.as_chunks::<4>().0 {
        let coarse = |channel: &u8| *channel as usize >> 4;
        let bucket = &mut buckets[(coarse(red) << 8) | (coarse(green) << 4) | coarse(blue)];
        bucket.0 += 1;
        for (sum, value) in bucket.1.iter_mut().zip([red, green, blue]) {
            *sum += *value as u32;
        }
    }
    let mut weighted: Vec<(f32, Colour)> = buckets
        .iter()
        .filter(|(pixels, _)| *pixels > 0)
        .map(|(pixels, sums)| {
            let colour = sums.map(|sum| sum as f32 / *pixels as f32 / 255.0);
            let most = colour[0].max(colour[1]).max(colour[2]);
            let least = colour[0].min(colour[1]).min(colour[2]);
            let saturation = if most > 0.0 { (most - least) / most } else { 0.0 };
            let dull = most < 0.12 || least > 0.92;
            (*pixels as f32 * (0.25 + saturation) * if dull { 0.05 } else { 1.0 }, colour)
        })
        .collect();
    weighted.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut chosen: Vec<Colour> = Vec::new();
    for (_, colour) in weighted {
        let apart = |other: &Colour| (0..3).map(|i| (other[i] - colour[i]).powi(2)).sum::<f32>() > DISTINCT;
        if chosen.iter().all(apart) {
            chosen.push(colour);
            if chosen.len() == count {
                break;
            }
        }
    }
    if chosen.is_empty() {
        return neutral().into_iter().cycle().take(count).collect();
    }
    (0..count).map(|i| tame(chosen[i % chosen.len()])).collect()
}

/// The colours `part` of the way (0 to 1) from those of one glow to those of the next.
pub fn blend(from: &[Colour], to: &[Colour], part: f32) -> Vec<Colour> {
    let part = part.clamp(0.0, 1.0);
    to.iter()
        .enumerate()
        .map(|(i, target)| {
            let start = from.get(i).unwrap_or(target);
            [0, 1, 2].map(|channel| start[channel] + (target[channel] - start[channel]) * part)
        })
        .collect()
}

/// The glow as 160 x 90 opaque pixels (RGBA) for a view `aspect` times as wide as it is high,
/// `seconds` into the drift of its blobs.
pub fn render(colours: &[Colour], seconds: f32, aspect: f32) -> Vec<u8> {
    let base = colours.first().copied().unwrap_or([0.2, 0.2, 0.25]).map(|channel| channel * 0.22);
    let diagonal = (aspect * aspect + 1.0).sqrt();
    // the blobs in the coordinates of the pixels below: x from 0 to `aspect`, y from 0 to 1
    let blobs: Vec<(f32, f32, f32, Colour)> = BLOBS
        .iter()
        .enumerate()
        .map(|(i, (x, y, radius, period_x, period_y))| {
            let colour = colours.get(i % colours.len().max(1)).copied().unwrap_or(base);
            let turn = std::f32::consts::TAU * seconds;
            let x = (x + 0.10 * (turn / period_x + i as f32).sin()) * aspect;
            let y = y + 0.10 * (turn / period_y + 2.0 * i as f32).cos();
            (x, y, 1.0 / (radius * diagonal), colour)
        })
        .collect();
    // 8 bits per channel are cheap for every renderer; a fixed 4 x 4 pattern spreads what the
    // rounding loses, so that slow gradients show no steps
    const PATTERN: [f32; 16] = [0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0];
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for row in 0..HEIGHT {
        let v = (row as f32 + 0.5) / HEIGHT as f32;
        for column in 0..WIDTH {
            let u = (column as f32 + 0.5) / WIDTH as f32 * aspect;
            let mut colour = base;
            for (x, y, per_radius, blob) in &blobs {
                let distance = (((u - x) * (u - x) + (v - y) * (v - y)).sqrt() * per_radius).min(1.0);
                // 0.6 in the centre of a blob, falling smoothly to nothing at its radius
                let rest = 1.0 - distance;
                let part = 0.60 * rest * rest * (1.0 + 2.0 * distance) * (1.0 - 0.45 * distance);
                colour[0] += (blob[0] - colour[0]) * part;
                colour[1] += (blob[1] - colour[1]) * part;
                colour[2] += (blob[2] - colour[2]) * part;
            }
            let dither = (PATTERN[(row % 4) * 4 + column % 4] + 0.5) / 16.0 - 0.5;
            for channel in colour {
                pixels.push((channel * 0.72 * 255.0 + dither).round().clamp(0.0, 255.0) as u8);
            }
            pixels.push(255);
        }
    }
    pixels
}

mod imp {
    use super::*;

    pub struct Glow {
        /// The colours the glow is changing from and to.
        pub from: RefCell<Vec<Colour>>,
        pub to: RefCell<Vec<Colour>>,
        /// How far that change has come, 0 to 1.
        pub fade: Cell<f32>,
        /// Seconds the blobs have drifted.
        pub drift: Cell<f32>,
        pub drifting: Cell<bool>,
        pub aspect: Cell<f32>,
        pub texture: RefCell<Option<gdk::Texture>>,
        /// When the texture was last computed (monotonic microseconds).
        pub stepped: Cell<i64>,
        pub timer: RefCell<Option<glib::SourceId>>,
    }

    impl Default for Glow {
        fn default() -> Self {
            Glow {
                from: RefCell::new(neutral()),
                to: RefCell::new(neutral()),
                fade: Cell::new(1.0),
                drift: Cell::new(0.0),
                drifting: Cell::new(false),
                aspect: Cell::new(16.0 / 9.0),
                texture: RefCell::default(),
                stepped: Cell::new(0),
                timer: RefCell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Glow {
        const NAME: &'static str = "AmGlow";
        type Type = super::Glow;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("glow");
            class.set_accessible_role(gtk::AccessibleRole::Presentation);
        }
    }

    impl ObjectImpl for Glow {}

    impl WidgetImpl for Glow {
        fn map(&self) {
            self.parent_map();
            if self.texture.borrow().is_none() {
                self.obj().compute();
            }
            self.obj().run();
        }

        fn unmap(&self) {
            self.obj().stop();
            self.parent_unmap();
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            // the blobs are round in a view of any shape: the texture is made for the view's
            let aspect = width.max(1) as f32 / height.max(1) as f32;
            if (aspect - self.aspect.get()).abs() > 0.02 * aspect {
                self.aspect.set(aspect);
                self.obj().compute();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let bounds = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
            if let Some(texture) = self.texture.borrow().as_ref() {
                snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Linear, &bounds);
            }
        }
    }
}

glib::wrapper! {
    pub struct Glow(ObjectSubclass<imp::Glow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Glow {
    fn default() -> Self {
        Glow::new()
    }
}

impl Glow {
    pub fn new() -> Glow {
        glib::Object::new()
    }

    /// Change to the colours of another artwork: `smoothly` over a moment (while the glow is on
    /// screen and the desktop does not ask for reduced motion), otherwise at once.
    pub fn set_colours(&self, colours: Vec<Colour>, smoothly: bool) {
        let imp = self.imp();
        if *imp.to.borrow() == colours {
            return;
        }
        // the change starts from what is shown now, also in the middle of an earlier change
        let shown = blend(&imp.from.borrow(), &imp.to.borrow(), eased(imp.fade.get()));
        imp.from.replace(shown);
        imp.to.replace(colours);
        if smoothly && self.is_mapped() && adw::is_animations_enabled(self) {
            imp.fade.set(0.0);
            self.run();
        } else {
            imp.fade.set(1.0);
            self.refresh();
        }
    }

    /// Whether the blobs drift (something plays).
    pub fn set_drifting(&self, drifting: bool) {
        if self.imp().drifting.replace(drifting) != drifting {
            self.run();
        }
    }

    /// True while the glow changes by itself.
    fn moving(&self) -> bool {
        let imp = self.imp();
        self.is_mapped() && adw::is_animations_enabled(self) && (imp.drifting.get() || imp.fade.get() < 1.0)
    }

    /// The texture for what the glow is now: computed at once while it is on screen, when it is
    /// shown again otherwise.
    fn refresh(&self) {
        if self.is_mapped() {
            self.compute();
        } else {
            self.imp().texture.replace(None);
        }
    }

    fn compute(&self) {
        let imp = self.imp();
        let colours = blend(&imp.from.borrow(), &imp.to.borrow(), eased(imp.fade.get()));
        let pixels = render(&colours, imp.drift.get(), imp.aspect.get());
        let texture = gdk::MemoryTexture::new(
            WIDTH as i32,
            HEIGHT as i32,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &glib::Bytes::from_owned(pixels),
            WIDTH * 4,
        );
        imp.texture.replace(Some(texture.upcast()));
        self.queue_draw();
    }

    /// Start the timer that moves the glow, when it has to move and none runs.
    fn run(&self) {
        let imp = self.imp();
        if imp.timer.borrow().is_some() || !self.moving() {
            return;
        }
        imp.stepped.set(glib::monotonic_time());
        let weak = self.downgrade();
        let timer = glib::timeout_add_local(STEP, move || {
            activity::note(activity::Source::Glow);
            match weak.upgrade() {
                Some(glow) => glow.step(),
                None => glib::ControlFlow::Break,
            }
        });
        imp.timer.replace(Some(timer));
    }

    fn step(&self) -> glib::ControlFlow {
        let imp = self.imp();
        let now = glib::monotonic_time();
        let passed = ((now - imp.stepped.replace(now)) as f32 / 1e6).clamp(0.0, 0.1);
        if imp.drifting.get() {
            imp.drift.set(imp.drift.get() + passed);
        }
        if imp.fade.get() < 1.0 {
            imp.fade.set((imp.fade.get() + passed / FADE).min(1.0));
        }
        if self.moving() {
            self.compute();
            return glib::ControlFlow::Continue;
        }
        // nothing moves any more (the timer ends by itself): a change of colours that was under
        // way is completed
        imp.timer.replace(None);
        imp.fade.set(1.0);
        self.refresh();
        glib::ControlFlow::Break
    }

    /// The glow is hidden: nothing moves, and a change of colours is complete when it shows again.
    fn stop(&self) {
        let imp = self.imp();
        if let Some(timer) = imp.timer.take() {
            timer.remove();
        }
        if imp.fade.get() < 1.0 {
            imp.fade.set(1.0);
            imp.texture.replace(None);
        }
    }
}

/// Slow at both ends.
fn eased(part: f32) -> f32 {
    let part = part.clamp(0.0, 1.0);
    part * part * (3.0 - 2.0 * part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(colours: &[([u8; 3], usize)]) -> Vec<u8> {
        colours
            .iter()
            .flat_map(|(colour, pixels)| std::iter::repeat_n([colour[0], colour[1], colour[2], 255], *pixels).flatten())
            .collect()
    }

    fn near(a: Colour, b: Colour) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 0.02)
    }

    #[test]
    fn palette_prefers_what_is_frequent_and_colourful() {
        // much dark grey, less red, a little blue: the colours lead, the grey comes last
        let colours = palette(&image(&[([40, 40, 40], 1200), ([200, 30, 30], 700), ([30, 60, 200], 400)]), 3);
        assert!(near(colours[0], [200.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0]), "{colours:?}");
        assert!(near(colours[1], [30.0 / 255.0, 60.0 / 255.0, 200.0 / 255.0]), "{colours:?}");
        assert!(near(colours[2], [40.0 / 255.0, 40.0 / 255.0, 40.0 / 255.0]), "{colours:?}");
    }

    #[test]
    fn palette_skips_black_and_white_while_there_is_colour() {
        let colours = palette(&image(&[([0, 0, 0], 3000), ([255, 255, 255], 3000), ([20, 140, 90], 60)]), 1);
        assert!(near(colours[0], [20.0 / 255.0, 140.0 / 255.0, 90.0 / 255.0]), "{colours:?}");
    }

    #[test]
    fn palette_takes_near_colours_as_one_and_repeats_what_it_has() {
        // two reds a shade apart and one green: two colours, used in turn for five blobs
        let colours = palette(&image(&[([176, 20, 20], 500), ([200, 36, 20], 400), ([20, 150, 40], 300)]), COLOURS);
        assert_eq!(colours.len(), COLOURS);
        assert!(colours[0][0] > 0.6 && colours[1][1] > 0.5, "{colours:?}");
        assert_eq!(colours[2], colours[0]);
        assert_eq!(colours[3], colours[1]);
    }

    #[test]
    fn palette_of_nothing_is_grey() {
        assert_eq!(palette(&[], COLOURS), neutral());
        // a black cover still gives a glow: its own darkness
        let black = palette(&image(&[([0, 0, 0], 100)]), COLOURS);
        assert_eq!(black, vec![[0.0, 0.0, 0.0]; COLOURS]);
    }

    #[test]
    fn bright_colours_are_darkened_for_white_text() {
        let yellow = tame([1.0, 0.92, 0.2]);
        assert!((brightness(yellow) - BRIGHTEST).abs() < 1e-4);
        // the hue stays: the channels keep their proportions
        assert!((yellow[1] / yellow[0] - 0.92).abs() < 1e-4);
        let navy = [0.1, 0.15, 0.5];
        assert_eq!(tame(navy), navy);
        assert!(palette(&image(&[([255, 255, 120], 500)]), 1).iter().all(|c| brightness(*c) <= BRIGHTEST + 1e-4));
    }

    #[test]
    fn blend_goes_from_one_glow_to_the_next() {
        let (from, to) = (vec![[0.0, 0.2, 1.0]; 2], vec![[1.0, 0.2, 0.0]; 2]);
        assert_eq!(blend(&from, &to, 0.0), from);
        assert_eq!(blend(&from, &to, 1.0), to);
        assert_eq!(blend(&from, &to, 0.5), vec![[0.5, 0.2, 0.5]; 2]);
        // a glow that had fewer colours starts the others where they end
        assert_eq!(blend(&from[..1], &to, 0.0), vec![[0.0, 0.2, 1.0], [1.0, 0.2, 0.0]]);
    }

    #[test]
    fn the_texture_is_opaque_and_dark_enough_for_white_text() {
        let colours = palette(&image(&[([255, 240, 60], 900), ([250, 250, 250], 900), ([255, 120, 0], 300)]), COLOURS);
        let pixels = render(&colours, 12.0, 16.0 / 10.0);
        assert_eq!(pixels.len(), WIDTH * HEIGHT * 4);
        for [red, green, blue, alpha] in pixels.as_chunks::<4>().0 {
            assert_eq!(*alpha, 255);
            let bright = brightness([red, green, blue].map(|channel| *channel as f32 / 255.0));
            assert!(bright < 0.4, "a pixel of brightness {bright}");
        }
    }

    #[test]
    fn the_blobs_drift() {
        let colours = vec![[0.8, 0.1, 0.1], [0.1, 0.1, 0.8], [0.1, 0.7, 0.2], [0.6, 0.5, 0.1], [0.5, 0.1, 0.6]];
        let (first, later) = (render(&colours, 0.0, 1.6), render(&colours, 4.0, 1.6));
        assert_ne!(first, later);
        // the same moment gives the same picture
        assert_eq!(first, render(&colours, 0.0, 1.6));
    }
}
