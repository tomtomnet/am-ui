//! The account on the engine: the sign-in state from the engine's status files and its live
//! check, the sign-in through Apple's native sign-in component.
//!
//! The password is handed to the engine in the call that starts the sign-in and is not kept
//! here; the engine writes it into the pipe of the sign-in helper and nowhere else.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use amui::session::{self, AndroidSignIn, Phase, SignInEvent, State};
use amui::web::Web;
use gtk::glib;

use crate::account::{AccountService, AccountState, OnStep, SignIn, SignInStep};
use crate::signal::background;

pub struct EngineAccount {
    web: Arc<Web>,
}

impl EngineAccount {
    pub fn new(web: Arc<Web>) -> EngineAccount {
        EngineAccount { web }
    }
}

fn state_of(state: State) -> AccountState {
    match state {
        State::Connected => AccountState::Connected,
        State::ReconnectRequired => AccountState::Reconnect,
        State::NoSession => AccountState::SignedOut,
        State::Unknown => AccountState::Unknown,
    }
}

fn busy(phase: Phase) -> &'static str {
    match phase {
        Phase::Preparing => "Preparing…",
        Phase::SigningIn | Phase::CodeRequired => "Signing in…",
        Phase::Verifying => "Verifying…",
        Phase::Finishing => "Finishing…",
    }
}

struct EngineSignIn {
    signin: Arc<AndroidSignIn>,
    on_step: Rc<OnStep>,
    /// Whether the dialog is asking for the code (a notice then belongs to the code).
    at_code: Rc<Cell<bool>>,
}

impl SignIn for EngineSignIn {
    fn submit_code(&self, code: &str) {
        if !self.signin.submit_code(code) {
            (self.on_step)(SignInStep::Failed("The sign-in is no longer running".to_string()));
        }
    }

    fn cancel(&self) {
        // does not block; the result follows once the helper has ended
        self.signin.cancel();
        self.at_code.set(false);
    }
}

impl AccountService for EngineAccount {
    fn state(&self) -> AccountState {
        state_of(session::status(&self.web).state)
    }

    fn can_sign_in(&self) -> Result<(), String> {
        if session::android_signin_available(self.web.config()) {
            Ok(())
        } else {
            Err(session::SessionError::NotBuilt.to_string())
        }
    }

    fn check(&self, done: Box<dyn FnOnce(AccountState)>) {
        let web = self.web.clone();
        background(
            move || match session::check(&web) {
                Ok(status) => status.state,
                // the check could not be made (no network): what the files say stands
                Err(_) => session::status(&web).state,
            },
            move |state| done(state_of(state)),
        );
    }

    fn sign_in(&self, apple_id: &str, password: &str, on_step: OnStep) -> Result<Box<dyn SignIn>, String> {
        // the engine reports on its reader thread; the steps are shown on this one
        let (sender, receiver) = async_channel::unbounded::<SignInEvent>();
        let listener = Box::new(move |event: SignInEvent| {
            let _ = sender.send_blocking(event);
        });
        let signin = session::android_signin(self.web.clone(), apple_id, password, Some(listener))
            .map_err(|error| error.to_string())?;
        let signin = Arc::new(signin);
        let on_step = Rc::new(on_step);
        let at_code = Rc::new(Cell::new(false));
        {
            let (on_step, at_code) = (on_step.clone(), at_code.clone());
            glib::spawn_future_local(async move {
                while let Ok(event) = receiver.recv().await {
                    match event {
                        SignInEvent::Phase(Phase::CodeRequired) => {
                            at_code.set(true);
                            on_step(SignInStep::Code { notice: None });
                        }
                        SignInEvent::Phase(phase) => {
                            at_code.set(false);
                            on_step(SignInStep::Busy(busy(phase).to_string()));
                        }
                        // the helper did not take the line it was sent: ask for the code again
                        SignInEvent::Notice { error } => {
                            at_code.set(true);
                            on_step(SignInStep::Code { notice: Some(error) });
                        }
                        SignInEvent::Result(result) => {
                            at_code.set(false);
                            if result.ok {
                                on_step(SignInStep::Done);
                            } else if result.kind.as_deref() == Some("cancelled") {
                                on_step(SignInStep::Failed("The sign-in was cancelled".to_string()));
                            } else {
                                let reason = result.error.filter(|error| !error.is_empty());
                                on_step(SignInStep::Failed(
                                    reason.unwrap_or_else(|| "The sign-in did not complete".to_string()),
                                ));
                            }
                            break;
                        }
                    }
                }
            });
        }
        Ok(Box::new(EngineSignIn { signin, on_step, at_code }))
    }
}
