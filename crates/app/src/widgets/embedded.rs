//! A list or grid view as one part of a page that scrolls as a whole.
//!
//! A `gtk::ListView` makes rows only for what its own adjustment shows. That keeps a long list
//! cheap as long as the list is the scrolled window's child, and then nothing can scroll with it:
//! no header above the rows, no footer below them, no second list. `Embedded` is the way around
//! that. In a page that scrolls through a `gtk::Viewport` it takes up the height of the whole
//! list, gives the list only the part of itself that is in view, and keeps the list's own
//! adjustment at that part's offset. The rows are then exactly where a fully laid out list would
//! have them, the page scrolls like any other (`pages::scrolled`), and the list still holds a
//! couple of hundred row widgets however long it is.
//!
//! When the list moves by itself (the keyboard focus goes to a row that is out of view, rows turn
//! out taller than it had estimated), the page follows by the same amount. The viewport must
//! leave that to the list and not scroll to the focused widget on its own
//! (`set_scroll_to_focus(false)`): it would go by where a row was when it was last in view.
//! `pages::detail::Frame` is a page set up that way.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Embedded {
        pub child: RefCell<Option<gtk::Widget>>,
        /// The list's own adjustment, and the handler on it.
        pub inner: RefCell<Option<(gtk::Adjustment, glib::SignalHandlerId)>>,
        /// The page's adjustment, and the handlers on it.
        pub outer: RefCell<Option<(gtk::Adjustment, [glib::SignalHandlerId; 2])>>,
        /// Where this widget begins in the scrolled content.
        pub top: Cell<i32>,
        /// The height the list was last given.
        pub given: Cell<Option<i32>>,
        /// The position the list was last told to show.
        pub expected: Cell<f64>,
        /// The list's adjustment is being set from here.
        pub setting: Cell<bool>,
        /// How far the list moved by itself, until the page has followed.
        pub follow: Cell<Option<f64>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Embedded {
        const NAME: &'static str = "AmEmbedded";
        type Type = super::Embedded;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Embedded {
        fn dispose(&self) {
            self.obj().detach();
            if let Some((adjustment, handler)) = self.inner.take() {
                adjustment.disconnect(handler);
            }
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Embedded {
        fn unroot(&self) {
            self.obj().detach();
            self.parent_unroot();
        }

        fn request_mode(&self) -> gtk::SizeRequestMode {
            match self.child.borrow().as_ref() {
                Some(child) => child.request_mode(),
                None => gtk::SizeRequestMode::ConstantSize,
            }
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let child = self.child.borrow();
            let Some(child) = child.as_ref() else { return (0, 0, -1, -1) };
            let (minimum, natural, _, _) = child.measure(orientation, for_size);
            if orientation == gtk::Orientation::Vertical {
                // the height of every row, never less
                (natural, natural, -1, -1)
            } else {
                (minimum, natural, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let obj = self.obj();
            let Some(child) = self.child.borrow().clone() else { return };
            obj.attach();
            let Some((first, last)) = obj.part(height) else {
                // not in a scrolling page: the list as it is
                child.set_child_visible(true);
                child.allocate(width, height, baseline, None);
                return;
            };
            if last <= first {
                // out of view: the list lets go of its rows' artwork
                child.set_child_visible(false);
                self.given.set(None);
                return;
            }
            child.set_child_visible(true);
            let same_size = self.given.get() == Some(last - first);
            let accepted = obj.show(first);
            obj.allocate_child(&child, width, first, last);
            self.given.set(Some(last - first));
            let Some(now) = obj.position() else { return };
            if (now - first as f64).abs() < 1.0 {
                return;
            }
            if accepted && same_size {
                // the list went somewhere by itself: its rows stay where it put them
                obj.follow(now - first as f64);
            } else {
                // the list placed itself by the size it had before: once more
                obj.show(first);
                obj.allocate_child(&child, width, first, last);
            }
        }
    }
}

glib::wrapper! {
    pub struct Embedded(ObjectSubclass<imp::Embedded>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Embedded {
    /// `list`: a `gtk::ListView` or a `gtk::GridView` (it must not be in a scrolled window).
    pub fn new<W: IsA<gtk::Widget> + IsA<gtk::Scrollable>>(list: &W) -> Embedded {
        let embedded: Embedded = glib::Object::new();
        let adjustment = list.vadjustment().unwrap_or_else(|| {
            let adjustment = gtk::Adjustment::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            list.set_vadjustment(Some(&adjustment));
            adjustment
        });
        let weak = embedded.downgrade();
        let handler = adjustment.connect_value_changed(move |adjustment| {
            let Some(embedded) = weak.upgrade() else { return };
            let imp = embedded.imp();
            if imp.setting.get() || imp.outer.borrow().is_none() {
                return;
            }
            // the list moved outside a layout of this widget (a row got the keyboard focus)
            let moved = adjustment.value() - imp.expected.get();
            if moved.abs() >= 1.0 {
                embedded.follow(moved);
            }
        });
        embedded.imp().inner.replace(Some((adjustment, handler)));
        list.set_parent(&embedded);
        embedded.imp().child.replace(Some(list.clone().upcast()));
        embedded
    }

    /// Listen to the adjustment of the page this widget is in.
    fn attach(&self) {
        let imp = self.imp();
        let viewport = self.ancestor(gtk::Viewport::static_type()).and_downcast::<gtk::Viewport>();
        let Some(adjustment) = viewport.and_then(|viewport| viewport.vadjustment()) else {
            return self.detach();
        };
        if imp.outer.borrow().as_ref().is_some_and(|(current, _)| *current == adjustment) {
            return;
        }
        // a scrolled window gives its viewport another adjustment when it takes it in
        self.detach();
        let weak = self.downgrade();
        let moved = adjustment.connect_value_changed(move |_| {
            if let Some(embedded) = weak.upgrade() {
                embedded.sync();
            }
        });
        let weak = self.downgrade();
        let resized = adjustment.connect_changed(move |_| {
            if let Some(embedded) = weak.upgrade() {
                embedded.sync();
            }
        });
        imp.outer.replace(Some((adjustment, [moved, resized])));
    }

    fn detach(&self) {
        if let Some((adjustment, handlers)) = self.imp().outer.take() {
            for handler in handlers {
                adjustment.disconnect(handler);
            }
        }
        self.imp().follow.set(None);
    }

    /// The part of this widget that is in view, as offsets from its top (`None` outside a page).
    fn part(&self, height: i32) -> Option<(i32, i32)> {
        let imp = self.imp();
        let outer = imp.outer.borrow();
        let (adjustment, _) = outer.as_ref()?;
        let content = self.ancestor(gtk::Viewport::static_type()).and_downcast::<gtk::Viewport>()?.child()?;
        if let Some(origin) = self.compute_point(&content, &graphene::Point::zero()) {
            imp.top.set(origin.y().round() as i32);
        }
        Some(visible_part(adjustment.value(), adjustment.page_size(), imp.top.get(), height))
    }

    /// The list's own position.
    fn position(&self) -> Option<f64> {
        self.imp().inner.borrow().as_ref().map(|(adjustment, _)| adjustment.value())
    }

    /// Tell the list to show its rows from `offset` on. False when it cannot (its bounds are
    /// those of its last layout).
    fn show(&self, offset: i32) -> bool {
        let imp = self.imp();
        let inner = imp.inner.borrow();
        let Some((adjustment, _)) = inner.as_ref() else { return false };
        imp.expected.set(offset as f64);
        imp.setting.set(true);
        adjustment.set_value(offset as f64);
        imp.setting.set(false);
        (adjustment.value() - offset as f64).abs() < 0.5
    }

    fn allocate_child(&self, child: &gtk::Widget, width: i32, first: i32, last: i32) {
        let imp = self.imp();
        imp.setting.set(true);
        child.size_allocate(&gtk::Allocation::new(0, first, width, last - first), -1);
        imp.setting.set(false);
    }

    /// The page moved or changed its size: the list shows what is in view now.
    fn sync(&self) {
        let imp = self.imp();
        imp.follow.set(None);
        if let Some((first, last)) = self.part(self.height())
            && last > first
        {
            self.show(first);
        }
        self.queue_allocate();
    }

    /// The list moved by `distance` on its own: the page does the same once this layout is over
    /// (a layout that is under way would not notice).
    fn follow(&self, distance: f64) {
        if self.imp().follow.replace(Some(distance)).is_some() {
            return;
        }
        let weak = self.downgrade();
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(embedded) = weak.upgrade()
                && let Some(distance) = embedded.imp().follow.take()
            {
                let page = embedded.imp().outer.borrow().as_ref().map(|(adjustment, _)| adjustment.clone());
                if let Some(page) = page {
                    let at_start = embedded.position().is_some_and(|position| position < 1.0);
                    let top = embedded.imp().top.get();
                    page.set_value(followed(page.value(), distance, at_start, top, page.page_size()));
                }
                embedded.sync();
            }
            glib::ControlFlow::Break
        });
    }
}

/// Where a page scrolled to `value` goes when the list that begins at `top` of it has moved by
/// `distance` on its own. A list that went back to its beginning (`at_start`) brings what is
/// above it into view as well: its heading, the header of the page.
fn followed(value: f64, distance: f64, at_start: bool, top: i32, page: f64) -> f64 {
    let value = value + distance;
    if distance < 0.0 && at_start { value.min((top as f64 - page / 2.0).max(0.0)) } else { value }
}

/// Which part of a widget of `height` that begins at `top` of the scrolled content is in view
/// when the page is scrolled to `value` and shows `page` pixels: offsets from the widget's top.
/// The viewport places its content at whole pixels (rounding down), and so does this.
fn visible_part(value: f64, page: f64, top: i32, height: i32) -> (i32, i32) {
    let value = value.floor() as i64;
    let page = page.ceil() as i64;
    let first = (value - top as i64).clamp(0, height as i64);
    let last = (value + page - top as i64).clamp(0, height as i64);
    (first as i32, last as i32)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::widgets::tests::Stage;

    #[test]
    fn the_part_in_view() {
        // a list of 29000 px that begins 350 px down the page, in a window that shows 661 px
        assert_eq!(visible_part(0.0, 661.0, 350, 29000), (0, 311));
        assert_eq!(visible_part(100.6, 661.0, 350, 29000), (0, 411));
        assert_eq!(visible_part(350.0, 661.0, 350, 29000), (0, 661));
        assert_eq!(visible_part(4476.3, 661.0, 350, 29000), (4126, 4787));
        // the end of the list, with the footer below it coming into view
        assert_eq!(visible_part(28769.0, 661.0, 350, 29000), (28419, 29000));
        // above and below what is in view: nothing to show
        assert_eq!(visible_part(0.0, 661.0, 700, 400), (0, 0));
        assert_eq!(visible_part(30000.0, 661.0, 350, 29000), (29000, 29000));
    }

    #[test]
    fn the_page_follows_its_list() {
        // the focus went to a row further down, or further up: the page moves by as much
        assert_eq!(followed(5000.0, 216.0, false, 350, 661.0), 5216.0);
        assert_eq!(followed(5000.0, -216.0, false, 350, 661.0), 4784.0);
        // back at the first row, the header above the list is in view again
        assert_eq!(followed(5000.0, -4650.0, true, 350, 661.0), 19.5);
        assert_eq!(followed(5000.0, -4650.0, true, 300, 661.0), 0.0);
        // a list far down the page: half a page of what is above it
        assert_eq!(followed(3000.0, -1000.0, true, 2000, 600.0), 1700.0);
    }

    const HEADER: i32 = 350;
    const FOOTER: i32 = 80;

    /// A page as the album and playlist pages build it: a header, an embedded list, a footer.
    struct Scene {
        stage: Stage,
        scroller: gtk::ScrolledWindow,
        column: gtk::Box,
        list: gtk::ListView,
        model: gtk::StringList,
    }

    impl Scene {
        fn new(rows: u32) -> Scene {
            let model = gtk::StringList::new(&[]);
            for n in 0..rows {
                model.append(&n.to_string());
            }
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(|_, object| {
                let label = gtk::Label::new(None);
                label.set_size_request(-1, 40);
                object.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&label));
            });
            factory.connect_bind(|_, object| {
                let cell = object.downcast_ref::<gtk::ListItem>().unwrap();
                let text = cell.item().and_downcast::<gtk::StringObject>().unwrap().string();
                cell.child().and_downcast::<gtk::Label>().unwrap().set_text(&text);
            });
            let list = gtk::ListView::new(Some(gtk::NoSelection::new(Some(model.clone()))), Some(factory));
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let block = |height: i32| {
                let block = gtk::Box::new(gtk::Orientation::Vertical, 0);
                block.set_size_request(-1, height);
                block
            };
            column.append(&block(HEADER));
            column.append(&Embedded::new(&list));
            column.append(&block(FOOTER));
            let viewport = gtk::Viewport::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
            viewport.set_scroll_to_focus(false);
            viewport.set_child(Some(&column));
            let scroller = gtk::ScrolledWindow::new();
            scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroller.set_child(Some(&viewport));
            let stage = Stage::new(&scroller, 700, 600);
            Scene { stage, scroller, column, list, model }
        }

        /// Let the window draw `count` frames.
        fn frames(&self, count: u32) {
            self.stage.frames(count);
        }

        /// The rows on screen: (number, where its top is in the column, its height).
        fn rows(&self) -> Vec<(i32, f32, f32)> {
            let mut rows = Vec::new();
            let mut next = self.list.first_child();
            while let Some(row) = next {
                next = row.next_sibling();
                let label = row.first_child().and_downcast::<gtk::Label>();
                if let (true, Some(label), Some(bounds)) = (row.is_mapped(), label, row.compute_bounds(&self.column))
                    && let Ok(number) = label.text().parse()
                {
                    rows.push((number, bounds.y(), bounds.height()));
                }
            }
            rows
        }

        /// Every row on screen is where a list laid out in full below the header would have
        /// it, and the rows on screen are the ones that belong there.
        fn check(&self, what: &str) {
            let adjustment = self.scroller.vadjustment();
            let (value, page) = (adjustment.value().floor() as f32, adjustment.page_size() as f32);
            let rows = self.rows();
            let height = rows.first().map(|row| row.2).unwrap_or(0.0);
            for (number, y, _) in &rows {
                let expected = HEADER as f32 + *number as f32 * height;
                assert!(
                    (y - expected).abs() < 0.01,
                    "{what}: row {number} is at {y}, not at {expected} (scrolled to {value})"
                );
            }
            // from the first row that reaches into view to the last one; the list also keeps
            // rows that are within a few pixels of its edges
            let count = self.model.n_items() as i32;
            let rows_between = |from: f32, to: f32| {
                let first = (((from - HEADER as f32) / height).floor() as i32).clamp(0, count);
                let last = (((to - HEADER as f32) / height).ceil() as i32).clamp(0, count);
                first..last
            };
            let (needed, allowed) =
                (rows_between(value, value + page), rows_between(value - 12.0, value + page + 12.0));
            let mut shown: Vec<i32> = rows.iter().map(|row| row.0).collect();
            shown.sort();
            let complete = needed.clone().all(|number| shown.contains(&number));
            let no_more = shown.iter().all(|number| allowed.contains(number));
            assert!(
                complete && no_more,
                "{what}: rows {shown:?} are on screen, {needed:?} belong there (scrolled to {value})"
            );
        }

        /// Scroll to `target` in steps, as a fling does, and check every frame.
        fn scroll(&self, target: f64, what: &str) {
            let adjustment = self.scroller.vadjustment();
            loop {
                let value = adjustment.value();
                let end = (adjustment.upper() - adjustment.page_size()).max(0.0);
                let goal = target.clamp(0.0, end);
                if (goal - value).abs() < 0.5 {
                    break;
                }
                adjustment.set_value(if goal > value { (value + 137.3).min(goal) } else { (value - 137.3).max(goal) });
                self.frames(1);
                self.check(what);
            }
        }
    }

    /// The embedded list of a page, scrolled, resized, added to and moved by the keyboard
    /// (part of `widgets::tests::widgets_on_a_display`).
    pub(crate) fn on_a_display() {
        let scene = Scene::new(500);
        let adjustment = scene.scroller.vadjustment();
        let row = scene.rows()[0].2 as f64;
        // the page is as tall as header, rows and footer
        assert_eq!(adjustment.upper(), HEADER as f64 + 500.0 * row + FOOTER as f64);
        scene.check("at the top");
        // far fewer row widgets than rows
        let widgets = scene.list.observe_children().n_items();
        assert!(widgets < 260, "{widgets} row widgets for 500 rows");

        scene.scroll(9000.0, "scrolling down");
        // rows that arrive later (the next pages of a long playlist) make the page longer
        for n in 500..600 {
            scene.model.append(&n.to_string());
        }
        scene.frames(2);
        assert_eq!(adjustment.upper(), HEADER as f64 + 600.0 * row + FOOTER as f64);
        scene.check("after more rows arrived");
        // a smaller and a larger window
        scene.stage.window.set_default_size(700, 380);
        scene.frames(6);
        scene.check("in a smaller window");
        scene.scroll(f64::MAX, "scrolling to the end");
        assert!(scene.rows().iter().any(|row| row.0 == 599), "the last row is on screen at the end");
        scene.stage.window.set_default_size(820, 700);
        scene.frames(6);
        scene.check("in a larger window");
        scene.scroll(0.0, "scrolling back up");
        assert_eq!(scene.rows()[0].0, 0);

        // the keyboard goes to a row far down: the list moves to it and the page follows
        scene.list.scroll_to(300, gtk::ListScrollFlags::FOCUS, None);
        scene.frames(4);
        scene.check("after the list moved to row 300");
        let in_view = |number: i32| {
            let (value, page) = (adjustment.value(), adjustment.page_size());
            let top = HEADER as f64 + number as f64 * row;
            top >= value && top + row <= value + page
        };
        assert!(in_view(300), "row 300 is in view (scrolled to {})", adjustment.value());
        // and back up to one that is behind the header's place
        scene.list.scroll_to(2, gtk::ListScrollFlags::FOCUS, None);
        scene.frames(4);
        scene.check("after the list moved to row 2");
        assert!(in_view(2), "row 2 is in view (scrolled to {})", adjustment.value());
        // from far down back to the first row: the header comes into view with it
        scene.scroll(9000.0, "scrolling down again");
        scene.list.scroll_to(0, gtk::ListScrollFlags::FOCUS, None);
        scene.frames(4);
        scene.check("after the list moved to its first row");
        assert_eq!(adjustment.value(), (HEADER as f64 - adjustment.page_size() / 2.0).max(0.0));
        assert!(scene.list.observe_children().n_items() < 260);
    }
}
