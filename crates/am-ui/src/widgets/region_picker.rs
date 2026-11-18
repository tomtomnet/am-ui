//! The dialog that chooses the catalog's region: a search field over the list of Apple Music's
//! regions. A region is found by its two-letter code or its name, the best match first
//! (`regions::search`). The account's own region comes first, as "Account (…)".

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;

use crate::data::Region;
use crate::pages::scrolled;
use crate::regions;
use crate::widgets::scaled;

/// The dialog's size at the usual text size.
const SIZE: (i32, i32) = (420, 560);
/// Space kept around a row the keyboard moves to.
const ROW_MARGIN: f64 = 6.0;

/// What a row of the list stands for: a region's code, or `None` for the account's own.
type Entry = Option<&'static str>;

/// How the chosen region is called ("Account (United States)" or the region's name).
pub fn label(region: &Region) -> String {
    match &region.chosen {
        Some(code) => regions::name(code),
        None => account_label(region),
    }
}

fn account_label(region: &Region) -> String {
    format!("Account ({})", regions::name(&region.account))
}

/// A row of the list.
fn row(name: &str, code: Option<&str>, chosen: bool) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_wrap(true);
    content.append(&label);
    if let Some(code) = code {
        let code = gtk::Label::new(Some(&code.to_uppercase()));
        code.add_css_class("dim-label");
        code.add_css_class("numeric");
        content.append(&code);
    }
    let check = gtk::Image::from_icon_name("am-object-select-symbolic");
    check.set_opacity(if chosen { 1.0 } else { 0.0 });
    content.append(&check);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row
}

/// Scroll `scroller` so far that `row` of `list` is in view.
fn bring_into_view(scroller: &gtk::ScrolledWindow, list: &gtk::ListBox, row: &gtk::ListBoxRow) {
    let Some(bounds) = row.compute_bounds(list) else { return };
    let adjustment = scroller.vadjustment();
    let (top, bottom) = (bounds.y() as f64 - ROW_MARGIN, (bounds.y() + bounds.height()) as f64 + ROW_MARGIN);
    if top < adjustment.value() {
        adjustment.set_value(top);
    } else if bottom > adjustment.value() + adjustment.page_size() {
        adjustment.set_value(bottom - adjustment.page_size());
    }
}

/// Offer the regions; `chosen(code)` runs with the region chosen (`None`: the account's own).
pub fn show(parent: &impl IsA<gtk::Widget>, region: &Region, chosen: impl Fn(Option<String>) + 'static) {
    let dialog = adw::Dialog::new();
    dialog.set_title("Catalog Region");
    dialog.set_content_width(scaled(parent, SIZE.0));
    dialog.set_content_height(scaled(parent, SIZE.1));

    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Search by name or code"));
    entry.set_search_delay(60);
    entry.set_margin_start(12);
    entry.set_margin_end(12);
    entry.set_margin_top(4);
    entry.set_margin_bottom(8);

    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let nothing = adw::StatusPage::new();
    nothing.set_icon_name(Some("am-system-search-symbolic"));
    nothing.set_title("No Regions Found");
    nothing.add_css_class("compact");
    list.set_placeholder(Some(&nothing));
    let scroller = scrolled(&list);

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&entry);
    view.set_content(Some(&scroller));
    dialog.set_child(Some(&view));

    // the rows that are shown, in the order of the list
    let entries: Rc<RefCell<Vec<Entry>>> = Rc::default();
    let fill = {
        let (list, entries, region) = (list.clone(), entries.clone(), region.clone());
        move |term: &str| {
            let mut shown = entries.borrow_mut();
            shown.clear();
            while let Some(row) = list.row_at_index(0) {
                list.remove(&row);
            }
            if regions::means_account(term) {
                list.append(&row(&account_label(&region), None, region.chosen.is_none()));
                shown.push(None);
            }
            for (code, name) in regions::search(term) {
                list.append(&row(name, Some(code), region.chosen.as_deref() == Some(code)));
                shown.push(Some(code));
            }
            list.select_row(list.row_at_index(0).as_ref());
        }
    };
    fill("");
    {
        let fill = fill.clone();
        entry.connect_search_changed(move |entry| fill(&entry.text()));
    }

    let choose: Rc<dyn Fn(&gtk::ListBoxRow)> = {
        let (dialog, entries) = (dialog.clone(), entries.clone());
        Rc::new(move |row| {
            let entry = entries.borrow().get(row.index() as usize).copied();
            if let Some(entry) = entry {
                chosen(entry.map(str::to_string));
                dialog.close();
            }
        })
    };
    {
        let choose = choose.clone();
        list.connect_row_activated(move |_, row| choose(row));
    }
    // the keyboard stays in the field: Enter takes the row that is selected, the arrows move it
    {
        let list = list.clone();
        entry.connect_activate(move |_| {
            if let Some(row) = list.selected_row() {
                choose(&row);
            }
        });
    }
    {
        let dialog = dialog.clone();
        entry.connect_stop_search(move |_| {
            dialog.close();
        });
    }
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (list, scroller) = (list.clone(), scroller.clone());
        keys.connect_key_pressed(move |_, key, _, _| {
            let step = match key {
                gdk::Key::Down => 1,
                gdk::Key::Up => -1,
                _ => return gtk::glib::Propagation::Proceed,
            };
            let at = list.selected_row().map_or(0, |row| row.index());
            if let Some(row) = list.row_at_index((at + step).max(0)) {
                list.select_row(Some(&row));
                bring_into_view(&scroller, &list, &row);
            }
            gtk::glib::Propagation::Stop
        });
    }
    entry.add_controller(keys);

    dialog.present(Some(parent));
    entry.grab_focus();
}
