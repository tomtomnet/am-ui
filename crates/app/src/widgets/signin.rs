//! The sign-in dialog: Apple Account and password, then the verification code Apple sends to
//! the user's devices.
//!
//! The password lives in its field until "Sign In" is pressed: the field is emptied before the
//! password is handed to the account service, and nothing here keeps or shows it.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::account::{AccountState, SignIn, SignInStep};
use crate::app::ctx;

pub fn show(parent: &impl IsA<gtk::Widget>) {
    build(parent, None);
}

/// The dialog as it looks at a step of the sign-in, without a sign-in running (screenshots).
pub fn show_step(parent: &impl IsA<gtk::Widget>, step: SignInStep) {
    build(parent, Some(step));
}

fn build(parent: &impl IsA<gtk::Widget>, initial: Option<SignInStep>) {
    let account = ctx().account.clone();
    if let Err(reason) = account.service.can_sign_in() {
        let dialog = adw::AlertDialog::new(Some("Sign-In Is Not Available"), Some(&reason));
        dialog.add_response("close", "Close");
        dialog.present(Some(parent));
        return;
    }

    // --- the form
    let apple_id = adw::EntryRow::new();
    apple_id.set_title("Apple Account (e-mail)");
    apple_id.set_input_purpose(gtk::InputPurpose::Email);
    let password = adw::PasswordEntryRow::new();
    password.set_title("Password");
    let fields = adw::PreferencesGroup::new();
    fields.add(&apple_id);
    fields.add(&password);
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);
    let sign_in = gtk::Button::with_label("Sign In");
    sign_in.add_css_class("suggested-action");
    sign_in.add_css_class("pill");
    sign_in.set_halign(gtk::Align::Center);
    sign_in.set_sensitive(false);
    let hint = gtk::Label::new(Some(
        "Your password goes to Apple's own sign-in component on this computer and is not stored.",
    ));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    hint.set_wrap(true);
    hint.set_justify(gtk::Justification::Center);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 14);
    form.append(&fields);
    form.append(&error);
    form.append(&sign_in);
    form.append(&hint);

    // --- working
    let spinner = adw::Spinner::new();
    spinner.set_size_request(36, 36);
    spinner.set_halign(gtk::Align::Center);
    let busy_text = gtk::Label::new(Some("Signing in…"));
    busy_text.add_css_class("title-4");
    let busy = gtk::Box::new(gtk::Orientation::Vertical, 14);
    busy.set_valign(gtk::Align::Center);
    busy.append(&spinner);
    busy.append(&busy_text);

    // --- the verification code
    let code_title = gtk::Label::new(Some("Enter the verification code sent to your devices"));
    code_title.set_wrap(true);
    code_title.set_justify(gtk::Justification::Center);
    let code_notice = gtk::Label::new(None);
    code_notice.add_css_class("error");
    code_notice.set_wrap(true);
    code_notice.set_justify(gtk::Justification::Center);
    code_notice.set_visible(false);
    let code = gtk::Entry::new();
    code.set_max_length(6);
    code.set_input_purpose(gtk::InputPurpose::Digits);
    code.set_placeholder_text(Some("000000"));
    gtk::prelude::EntryExt::set_alignment(&code, 0.5);
    code.set_width_chars(8);
    code.set_max_width_chars(8);
    code.set_halign(gtk::Align::Center);
    code.add_css_class("signin-code");
    let verify = gtk::Button::with_label("Continue");
    verify.add_css_class("suggested-action");
    verify.add_css_class("pill");
    verify.set_halign(gtk::Align::Center);
    verify.set_sensitive(false);
    let code_page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    code_page.set_valign(gtk::Align::Center);
    code_page.append(&code_title);
    code_page.append(&code_notice);
    code_page.append(&code);
    code_page.append(&verify);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(150);
    stack.add_named(&form, Some("form"));
    stack.add_named(&busy, Some("busy"));
    stack.add_named(&code_page, Some("code"));
    stack.set_margin_top(6);
    stack.set_margin_bottom(24);
    stack.set_margin_start(24);
    stack.set_margin_end(24);

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&stack));
    let dialog = adw::Dialog::new();
    dialog.set_title("Sign In to Apple Music");
    dialog.set_content_width(420);
    dialog.set_child(Some(&view));

    let running: Rc<RefCell<Option<Box<dyn SignIn>>>> = Rc::new(RefCell::new(None));

    // "Sign In" is offered once both fields hold something
    {
        let update = {
            let (apple_id, password, sign_in) = (apple_id.clone(), password.clone(), sign_in.clone());
            move || sign_in.set_sensitive(!apple_id.text().trim().is_empty() && !password.text().is_empty())
        };
        let on_id = update.clone();
        apple_id.connect_changed(move |_| on_id());
        password.connect_changed(move |_| update());
    }
    {
        let verify = verify.clone();
        code.connect_changed(move |code| {
            let text = code.text();
            verify.set_sensitive(text.len() == 6 && text.chars().all(|c| c.is_ascii_digit()));
        });
    }

    // what each step of a sign-in shows
    let apply: Rc<dyn Fn(SignInStep)> = {
        let (stack, busy_text, code, code_notice, error, dialog, running, password) = (
            stack.clone(),
            busy_text.clone(),
            code.clone(),
            code_notice.clone(),
            error.clone(),
            dialog.clone(),
            running.clone(),
            password.clone(),
        );
        Rc::new(move |step| match step {
            SignInStep::Busy(text) => {
                busy_text.set_text(&text);
                stack.set_visible_child_name("busy");
            }
            SignInStep::Code { notice } => {
                code.set_text("");
                code_notice.set_text(notice.as_deref().unwrap_or(""));
                code_notice.set_visible(notice.is_some());
                stack.set_visible_child_name("code");
                code.grab_focus();
            }
            SignInStep::Done => {
                running.replace(None);
                let ctx = ctx();
                ctx.account.set_state(AccountState::Connected);
                ctx.account.signed_in.emit(&());
                ctx.toast.emit(&"Signed in to Apple Music".to_string());
                dialog.close();
            }
            SignInStep::Failed(message) => {
                running.replace(None);
                error.set_text(&message);
                error.set_visible(true);
                stack.set_visible_child_name("form");
                password.grab_focus();
            }
        })
    };

    let submit = {
        let (apple_id, password, error, running, apply) =
            (apple_id.clone(), password.clone(), error.clone(), running.clone(), apply.clone());
        move || {
            let id = apple_id.text().trim().to_string();
            let secret = password.text().to_string();
            // the field forgets the password before anything else happens
            password.set_text("");
            if id.is_empty() || secret.is_empty() {
                return;
            }
            error.set_visible(false);
            apply(SignInStep::Busy("Preparing…".into()));
            let on_step = apply.clone();
            match ctx().account.service.sign_in(&id, &secret, Box::new(move |step| on_step(step))) {
                Ok(handle) => {
                    running.replace(Some(handle));
                }
                Err(message) => apply(SignInStep::Failed(message)),
            }
        }
    };
    {
        let on_click = submit.clone();
        sign_in.connect_clicked(move |_| on_click());
        let on_enter = submit.clone();
        password.connect_entry_activated(move |_| on_enter());
    }

    let send_code = {
        let (code, stack, busy_text, running) = (code.clone(), stack.clone(), busy_text.clone(), running.clone());
        move || {
            let text = code.text().to_string();
            if text.len() != 6 || !text.chars().all(|c| c.is_ascii_digit()) {
                return;
            }
            code.set_text("");
            busy_text.set_text("Verifying…");
            stack.set_visible_child_name("busy");
            if let Some(handle) = running.borrow().as_ref() {
                handle.submit_code(&text);
            }
        }
    };
    {
        let on_click = send_code.clone();
        verify.connect_clicked(move |_| on_click());
        code.connect_activate(move |_| send_code());
    }

    // closing the dialog gives up a sign-in that is still running
    {
        let running = running.clone();
        dialog.connect_closed(move |_| {
            if let Some(handle) = running.take() {
                handle.cancel();
            }
        });
    }
    dialog.present(Some(parent));
    apple_id.grab_focus();
    if let Some(step) = initial {
        apply(step);
    }
}
