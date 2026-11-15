//! The Apple Music account as the window sees it: whether this machine is signed in, and the
//! sign-in itself. The engine's `session` serves it in the real application, a simulation in the
//! demonstration.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;

use crate::signal::Signal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    /// Signed in, and Apple accepted the sign-in when it was last checked.
    Connected,
    /// A sign-in exists but Apple rejected it: sign in again.
    Reconnect,
    /// No sign-in on this machine.
    SignedOut,
    /// A sign-in exists and has not been checked yet.
    Unknown,
}

/// Where a running sign-in is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInStep {
    /// Working; the text says on what ("Signing in…").
    Busy(String),
    /// Apple sent a verification code to the user's devices: ask for it. A notice says why it is
    /// asked again.
    Code { notice: Option<String> },
    /// Signed in.
    Done,
    /// The attempt ended without a sign-in; the message says why.
    Failed(String),
}

pub type OnStep = Box<dyn Fn(SignInStep)>;

/// A sign-in that is running.
pub trait SignIn {
    /// Send the six-digit verification code.
    fn submit_code(&self, code: &str);
    /// Give up; an earlier sign-in on this machine is kept.
    fn cancel(&self);
}

pub trait AccountService {
    /// What is known without asking Apple.
    fn state(&self) -> AccountState;
    /// Whether this machine can sign in at all, and if not, why.
    fn can_sign_in(&self) -> Result<(), String>;
    /// Ask Apple whether the sign-in is accepted; `done` gets the answer on the UI thread.
    fn check(&self, done: Box<dyn FnOnce(AccountState)>);
    /// Start a sign-in with these credentials. `on_step` is called on the UI thread. The
    /// password is handed on and not kept.
    fn sign_in(&self, apple_id: &str, password: &str, on_step: OnStep) -> Result<Box<dyn SignIn>, String>;
}

/// The account's state with a signal for the widgets that show it.
pub struct Account {
    pub service: Rc<dyn AccountService>,
    state: Cell<AccountState>,
    /// The state changed.
    pub changed: Signal<AccountState>,
    /// A sign-in just succeeded: load the library again.
    pub signed_in: Signal<()>,
}

impl Account {
    pub fn new(service: Rc<dyn AccountService>) -> Rc<Account> {
        let state = service.state();
        Rc::new(Account { service, state: Cell::new(state), changed: Signal::new(), signed_in: Signal::new() })
    }

    pub fn state(&self) -> AccountState {
        self.state.get()
    }

    pub fn set_state(&self, state: AccountState) {
        if self.state.replace(state) != state {
            self.changed.emit(&state);
        }
    }

    /// Ask Apple whether the sign-in still holds.
    pub fn check(self: &Rc<Self>) {
        let account = self.clone();
        self.service.check(Box::new(move |state| account.set_state(state)));
    }
}

// --- the demonstration ---------------------------------------------------------------

/// A sign-in that needs no account: any Apple ID with a password of four characters or more
/// is accepted after a verification code; the code 000000 is refused once.
pub struct DemoAccount {
    state: Cell<AccountState>,
}

impl DemoAccount {
    pub fn new(signed_in: bool) -> DemoAccount {
        DemoAccount { state: Cell::new(if signed_in { AccountState::Connected } else { AccountState::SignedOut }) }
    }
}

struct DemoSignIn {
    on_step: Rc<OnStep>,
    cancelled: Rc<Cell<bool>>,
    refused_once: RefCell<bool>,
}

fn later(ms: u64, cancelled: &Rc<Cell<bool>>, run: impl FnOnce() + 'static) {
    let cancelled = cancelled.clone();
    glib::timeout_add_local_once(Duration::from_millis(ms), move || {
        if !cancelled.get() {
            run();
        }
    });
}

impl SignIn for DemoSignIn {
    fn submit_code(&self, code: &str) {
        let on_step = self.on_step.clone();
        on_step(SignInStep::Busy("Verifying…".into()));
        let refuse = code == "000000" && !self.refused_once.replace(true);
        later(700, &self.cancelled, move || {
            if refuse {
                on_step(SignInStep::Code { notice: Some("That code was not accepted. Enter the new code.".into()) });
            } else {
                on_step(SignInStep::Busy("Finishing…".into()));
                let on_step = on_step.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || on_step(SignInStep::Done));
            }
        });
    }

    fn cancel(&self) {
        self.cancelled.set(true);
        (self.on_step)(SignInStep::Failed("The sign-in was cancelled".into()));
    }
}

impl AccountService for DemoAccount {
    fn state(&self) -> AccountState {
        self.state.get()
    }

    fn can_sign_in(&self) -> Result<(), String> {
        Ok(())
    }

    fn check(&self, done: Box<dyn FnOnce(AccountState)>) {
        let state = self.state.get();
        glib::timeout_add_local_once(Duration::from_millis(300), move || done(state));
    }

    fn sign_in(&self, apple_id: &str, password: &str, on_step: OnStep) -> Result<Box<dyn SignIn>, String> {
        if !apple_id.contains('@') {
            return Err("Enter the e-mail address of your Apple Account".into());
        }
        let good_password = password.chars().count() >= 4;
        let on_step = Rc::new(on_step);
        let cancelled = Rc::new(Cell::new(false));
        on_step(SignInStep::Busy("Preparing…".into()));
        let step = on_step.clone();
        later(500, &cancelled, move || step(SignInStep::Busy("Signing in…".into())));
        let step = on_step.clone();
        later(1300, &cancelled, move || {
            if good_password {
                step(SignInStep::Code { notice: None });
            } else {
                step(SignInStep::Failed("Your Apple Account or password was incorrect".into()));
            }
        });
        Ok(Box::new(DemoSignIn { on_step, cancelled, refused_once: RefCell::new(false) }))
    }
}
