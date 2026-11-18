//! A short list of rows of one height that fills two columns when the page is wide enough for
//! them, top to bottom in each (an artist's top songs). How wide a column must be grows with the
//! desktop's text size.

use std::cell::Cell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::widgets::scaled;

const GAP: i32 = 12;

/// How many columns a list of `width` has when a column needs `column_minimum`.
pub fn columns(width: i32, column_minimum: i32) -> i32 {
    if width >= 2 * column_minimum + GAP { 2 } else { 1 }
}

/// Where row `index` of `count` goes: (column, row in it).
pub fn cell(index: i32, count: i32, columns: i32) -> (i32, i32) {
    let per_column = (count + columns - 1) / columns.max(1);
    if per_column == 0 { (0, 0) } else { (index / per_column, index % per_column) }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Columns {
        pub column_minimum: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Columns {
        const NAME: &'static str = "AmColumns";
        type Type = super::Columns;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("columns");
        }
    }

    impl ObjectImpl for Columns {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl Columns {
        fn children(&self) -> Vec<gtk::Widget> {
            let mut children = Vec::new();
            let mut next = self.obj().first_child();
            while let Some(child) = next {
                next = child.next_sibling();
                children.push(child);
            }
            children
        }

        /// The height of one row (the tallest, should they differ).
        fn row_height(&self, children: &[gtk::Widget], width: i32) -> i32 {
            children.iter().map(|child| child.measure(gtk::Orientation::Vertical, width).1).max().unwrap_or(0)
        }

        /// How wide a column must be, at the desktop's text size.
        fn column_minimum(&self) -> i32 {
            scaled(&*self.obj(), self.column_minimum.get())
        }
    }

    impl WidgetImpl for Columns {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let children = self.children();
            if orientation == gtk::Orientation::Horizontal {
                let widths = children.iter().map(|child| child.measure(orientation, -1));
                let (minimum, natural) =
                    widths.fold((0, 0), |(minimum, natural), (min, nat, _, _)| (minimum.max(min), natural.max(nat)));
                return (minimum, natural.max(minimum), -1, -1);
            }
            let count = children.len() as i32;
            let columns = if for_size > 0 { columns(for_size, self.column_minimum()) } else { 1 };
            let column_width = if for_size > 0 { (for_size - (columns - 1) * GAP) / columns } else { -1 };
            let rows = (count + columns - 1) / columns;
            let height = rows * self.row_height(&children, column_width);
            (height, height, -1, -1)
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let children = self.children();
            let count = children.len() as i32;
            let columns = columns(width, self.column_minimum());
            let column_width = (width - (columns - 1) * GAP) / columns;
            let row_height = self.row_height(&children, column_width);
            for (index, child) in children.iter().enumerate() {
                let (column, row) = cell(index as i32, count, columns);
                let _ = child.measure(gtk::Orientation::Horizontal, -1);
                let x = column * (column_width + GAP);
                child.size_allocate(&gtk::Allocation::new(x, row * row_height, column_width, row_height), -1);
            }
        }
    }
}

glib::wrapper! {
    pub struct Columns(ObjectSubclass<imp::Columns>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Columns {
    /// Two columns once each can be `column_minimum` wide (at the usual text size).
    pub fn new(column_minimum: i32) -> Columns {
        let columns: Columns = glib::Object::new();
        columns.imp().column_minimum.set(column_minimum);
        columns
    }

    pub fn append(&self, row: &impl IsA<gtk::Widget>) {
        row.set_parent(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_fill_one_column_after_the_other() {
        assert_eq!(columns(700, 420), 1);
        assert_eq!(columns(852, 420), 2);
        // ten rows in two columns: five and five
        assert_eq!(cell(0, 10, 2), (0, 0));
        assert_eq!(cell(4, 10, 2), (0, 4));
        assert_eq!(cell(5, 10, 2), (1, 0));
        assert_eq!(cell(9, 10, 2), (1, 4));
        // seven rows: four and three
        assert_eq!(cell(3, 7, 2), (0, 3));
        assert_eq!(cell(4, 7, 2), (1, 0));
        assert_eq!(cell(6, 7, 1), (0, 6));
        assert_eq!(cell(0, 0, 2), (0, 0));
    }
}
