//! A grid of cards (the library's albums, artists and playlists), what a page of the library
//! shows while its list loads or is empty, and the page of what was added recently.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::app::ctx;
use crate::library::{Kind, item_of};
use crate::nav::Dest;
use crate::pages::{card_action, scrolled, shell};
use crate::widgets::card::{Card, CardHandler};

/// A grid view of the items of `model` (rows carrying `data::Item`).
pub fn card_grid(model: &impl IsA<gio::ListModel>, handler: CardHandler) -> gtk::GridView {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        let card = Card::new();
        card.set_handler(handler.clone());
        cell.set_child(Some(&card));
        cell.set_activatable(true);
        cell.set_selectable(false);
    });
    factory.connect_bind(|_, object| {
        let Some(cell) = object.downcast_ref::<gtk::ListItem>() else { return };
        if let Some(card) = cell.child().and_downcast::<Card>() {
            card.bind(cell.item().as_ref().and_then(item_of).as_ref());
        }
    });
    factory.connect_unbind(|_, object| {
        if let Some(card) = object.downcast_ref::<gtk::ListItem>().and_then(|cell| cell.child()).and_downcast::<Card>()
        {
            card.bind(None);
        }
    });
    let view = gtk::GridView::new(Some(gtk::NoSelection::new(Some(model.clone()))), Some(factory));
    view.add_css_class("cards");
    view.set_min_columns(2);
    view.set_max_columns(14);
    // Enter on a focused card opens it
    view.connect_activate(|view, position| {
        if let Some(item) = view.model().and_then(|model| model.item(position)).as_ref().and_then(item_of) {
            crate::actions::open(&item);
        }
    });
    view
}

/// Content, a spinner while a list loads for the first time, or what went wrong.
pub fn with_states(kind: Kind, content: &impl IsA<gtk::Widget>, empty_title: &str, empty_text: &str) -> gtk::Stack {
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.add_named(content, Some("content"));
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    stack.add_named(&spinner, Some("loading"));
    let empty = adw::StatusPage::new();
    empty.set_title(empty_title);
    empty.set_description(Some(empty_text));
    stack.add_named(&empty, Some("empty"));
    let failed = adw::StatusPage::new();
    failed.set_icon_name(Some("am-network-error-symbolic"));
    failed.set_title("Couldn't Load Your Library");
    let again = gtk::Button::with_label("Try Again");
    again.add_css_class("pill");
    again.set_halign(gtk::Align::Center);
    again.connect_clicked(|_| {
        let ctx = ctx();
        ctx.library.reload(&ctx.music);
    });
    failed.set_child(Some(&again));
    stack.add_named(&failed, Some("failed"));
    let signed_out = adw::StatusPage::new();
    signed_out.set_icon_name(Some("am-avatar-default-symbolic"));
    signed_out.set_title("Sign In to Apple Music");
    signed_out.set_description(Some("Your library appears here once you are signed in."));
    let sign_in = gtk::Button::with_label("Sign In");
    sign_in.add_css_class("pill");
    sign_in.add_css_class("suggested-action");
    sign_in.set_halign(gtk::Align::Center);
    sign_in.connect_clicked(crate::widgets::signin::show);
    signed_out.set_child(Some(&sign_in));
    stack.add_named(&signed_out, Some("signed-out"));

    let update = move |stack: &gtk::Stack| {
        let library = ctx().library.clone();
        let count = library.store(kind).n_items();
        let problem = library.problem.borrow().clone();
        let name = if count > 0 {
            "content"
        } else if ctx().account.state() == crate::account::AccountState::SignedOut {
            "signed-out"
        } else if library.is_loading(kind) || !library.is_loaded(kind) && problem.is_none() {
            "loading"
        } else if let Some(problem) = problem {
            if let Some(page) = stack.child_by_name("failed").and_downcast::<adw::StatusPage>() {
                page.set_description(Some(&problem.message));
            }
            "failed"
        } else {
            "empty"
        };
        stack.set_visible_child_name(name);
    };
    update(&stack);
    ctx().library.changed.connect(&stack, move |stack, changed| {
        if *changed == kind {
            update(stack);
        }
    });
    ctx().account.changed.connect(&stack, move |stack, _| update(stack));
    let weak = stack.downgrade();
    ctx().library.store(kind).connect_items_changed(move |store, _, _, _| {
        // the first rows arrive while the list is still loading
        if let Some(stack) = weak.upgrade()
            && store.n_items() > 0
        {
            stack.set_visible_child_name("content");
        }
    });
    let _ = glib::MainContext::default();
    stack
}

/// What a page of the library says while its list is empty: a title and a line under it.
pub fn empty_texts(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Albums => ("No Albums", "Albums you add to your library appear here."),
        Kind::Artists => ("No Artists", "Artists of the music in your library appear here."),
        Kind::Playlists => ("No Playlists", "Playlists you create or add appear here."),
        Kind::Songs => ("No Songs", "Songs you add to your library appear here."),
        Kind::Recent => ("Nothing Added Yet", "Music you add to your library appears here."),
    }
}

/// The cards of a list of the library as it is ordered (what was added recently).
pub fn library_page(dest: &Dest, kind: Kind) -> adw::NavigationPage {
    let handler: CardHandler = Rc::new(card_action);
    let view = card_grid(ctx().library.store(kind), handler);
    let (title, text) = empty_texts(kind);
    let content = with_states(kind, &scrolled(&view), title, text);
    shell(dest.title(), &content).page
}
