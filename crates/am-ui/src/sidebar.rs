//! The sidebar: search, the sections of Apple Music, the library, the playlists.

use adw::prelude::*;

use crate::app::ctx;
use crate::nav::Dest;
use crate::widgets::playlist_list::{self, PlaylistList};

pub struct Sidebar {
    pub widget: adw::ToolbarView,
    pub list: gtk::ListBox,
    pub search: gtk::SearchEntry,
    playlists: PlaylistList,
}

fn row(dest: &Dest) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.append(&gtk::Image::from_icon_name(dest.icon()));
    let label = gtk::Label::new(Some(dest.title()));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&label);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row.set_widget_name(dest.name());
    row
}

pub fn build(on_select: impl Fn(Dest) + 'static) -> Sidebar {
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    for dest in Dest::ROOTS {
        list.append(&row(&dest));
    }
    // "Library" above the first library row, as a section header
    list.set_header_func(|row, before| {
        let starts_library = row.widget_name() == Dest::Recent.name() && before.is_some();
        if starts_library {
            let header = gtk::Label::new(Some("Library"));
            header.set_xalign(0.0);
            header.add_css_class("sidebar-heading");
            row.set_header(Some(&header));
        } else {
            row.set_header(None::<&gtk::Widget>);
        }
    });
    // a click or Enter opens a section; the selection only shows where the window is
    list.connect_row_activated(move |_, row| {
        if let Some(dest) = Dest::parse(&row.widget_name()) {
            on_select(dest);
        }
    });

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search"));
    search.add_css_class("sidebar-search");

    // the sections, then the library's playlists, scrolling together
    let playlists = playlist_list::build();
    let lists = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lists.append(&list);
    lists.append(&playlists.widget);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&search);
    let scroller =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&lists).build();
    content.append(&scroller);

    let menu = gtk::gio::Menu::new();
    menu.append(Some("Sign In…"), Some("app.sign-in"));
    menu.append(Some("Equalizer"), Some("app.equalizer"));
    menu.append(Some("Preferences"), Some("app.preferences"));
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Music"), Some("app.about"));
    let menu_button =
        gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Main Menu").build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Music", "")));
    header.pack_end(&menu_button);

    let widget = adw::ToolbarView::new();
    widget.add_top_bar(&header);
    widget.set_content(Some(&content));
    Sidebar { widget, list, search, playlists }
}

impl Sidebar {
    /// Mark `dest` as where the window is: one of the sections, one of the playlists, or neither.
    pub fn show(&self, dest: &Dest) {
        self.playlists.show(dest);
        let mut index = 0;
        while let Some(row) = self.list.row_at_index(index) {
            if row.widget_name() == dest.name() {
                if !row.is_selected() {
                    self.list.select_row(Some(&row));
                }
                return;
            }
            index += 1;
        }
        self.list.select_row(None::<&gtk::ListBoxRow>);
    }

    /// The playlists arrive after the window: mark the current one once it is among them.
    pub fn follow_playlists(self: &std::rc::Rc<Self>) {
        let weak = std::rc::Rc::downgrade(self);
        ctx().library.playlists.connect_items_changed(move |_, _, _, _| {
            if let (Some(sidebar), Some(dest)) = (weak.upgrade(), ctx().nav.current()) {
                sidebar.playlists.show(&dest);
            }
        });
    }
}
