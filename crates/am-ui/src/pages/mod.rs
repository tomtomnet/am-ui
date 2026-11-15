//! The pages of the window and what they share.

pub mod artist;
pub mod collection;
pub mod detail;
pub mod grid;
pub mod search;
pub mod shelves;
pub mod songs;
pub mod view_options;

use adw::prelude::*;

use crate::actions;
use crate::app::ctx;
use crate::data::Item;
use crate::library::Kind;
use crate::nav::Dest;
use crate::scroll::{SmoothScroll, Tuning};
use crate::widgets::card::{Card, CardAction};

/// A page with its header bar.
pub struct Shell {
    pub page: adw::NavigationPage,
    pub header: adw::HeaderBar,
}

/// Back and forward through the window's history.
fn history_buttons() -> gtk::Box {
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Back"));
    back.connect_clicked(|_| ctx().nav.back());
    let forward = gtk::Button::from_icon_name("go-next-symbolic");
    forward.set_tooltip_text(Some("Forward"));
    forward.connect_clicked(|_| ctx().nav.forward());
    buttons.append(&back);
    buttons.append(&forward);
    let update = |buttons: &gtk::Box| {
        let nav = ctx().nav.clone();
        if let Some(back) = buttons.first_child() {
            back.set_sensitive(nav.can_go_back());
        }
        if let Some(forward) = buttons.last_child() {
            forward.set_sensitive(nav.can_go_forward());
        }
    };
    update(&buttons);
    ctx().nav.changed.connect(&buttons, move |buttons, _| update(buttons));
    buttons
}

pub fn shell(title: &str, content: &impl IsA<gtk::Widget>) -> Shell {
    let header = adw::HeaderBar::new();
    header.set_show_back_button(false);
    header.pack_start(&history_buttons());
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(content));
    let page = adw::NavigationPage::builder().title(title).child(&view).build();
    Shell { page, header }
}

/// A scrolled window whose content moves in step with the display (see `scroll`).
pub fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(child)
        .vexpand(true)
        .build();
    SmoothScroll::attach(&scroller, Tuning::default());
    scroller
}

/// What a card does: open its page, play it, or show its menu.
pub fn card_action(item: &Item, action: CardAction, card: &Card) {
    match action {
        CardAction::Open => actions::open(item),
        CardAction::Play => actions::play(item),
        CardAction::Menu(x, y) => crate::menus::item_menu(item, card, x, y),
    }
}

/// The page of a destination.
pub fn build(dest: &Dest) -> adw::NavigationPage {
    match dest {
        Dest::Search => search::page(dest),
        Dest::Albums => view_options::page(dest, Kind::Albums),
        Dest::Artists => view_options::page(dest, Kind::Artists),
        Dest::Playlists => view_options::page(dest, Kind::Playlists),
        Dest::Recent => grid::library_page(dest, Kind::Recent),
        Dest::Songs => view_options::page(dest, Kind::Songs),
        Dest::Home | Dest::New | Dest::Radio => shelves::page(dest),
        Dest::Album(target) => collection::page(dest, collection::Kind::Album, target),
        Dest::Playlist(target) => collection::page(dest, collection::Kind::Playlist, target),
        Dest::Artist(target) => artist::page(dest, target),
    }
}
