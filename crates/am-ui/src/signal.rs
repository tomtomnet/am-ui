//! A list of listeners for one kind of event, for the user interface thread.
//!
//! The application state announces changes through these; widgets subscribe with a weak
//! reference to themselves and are dropped from the list once they are gone.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::glib::prelude::*;

type Handler<T> = Rc<dyn Fn(&T) -> bool>;

pub struct Signal<T> {
    handlers: RefCell<Vec<Handler<T>>>,
}

impl<T> Default for Signal<T> {
    fn default() -> Self {
        Signal { handlers: RefCell::new(Vec::new()) }
    }
}

impl<T: 'static> Signal<T> {
    pub fn new() -> Signal<T> {
        Signal::default()
    }

    /// Call `handler` for every event from now on, for as long as `owner` lives.
    pub fn connect<O: ObjectType>(&self, owner: &O, handler: impl Fn(&O, &T) + 'static) {
        let weak = owner.downgrade();
        self.handlers.borrow_mut().push(Rc::new(move |value| match weak.upgrade() {
            Some(owner) => {
                handler(&owner, value);
                true
            }
            None => false,
        }));
    }

    /// Call `handler` for every event for the rest of the program.
    pub fn connect_forever(&self, handler: impl Fn(&T) + 'static) {
        self.handlers.borrow_mut().push(Rc::new(move |value| {
            handler(value);
            true
        }));
    }

    pub fn emit(&self, value: &T) {
        // handlers may subscribe or emit again: work on a copy of the list
        let handlers: Vec<Handler<T>> = self.handlers.borrow().clone();
        let mut gone = Vec::new();
        for handler in &handlers {
            if !handler(value) {
                gone.push(Rc::as_ptr(handler) as *const ());
            }
        }
        if !gone.is_empty() {
            self.handlers.borrow_mut().retain(|handler| !gone.contains(&(Rc::as_ptr(handler) as *const ())));
        }
    }
}

/// Run `work` on a worker thread and `done` with its result on the user interface thread.
pub fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    glib::spawn_future_local(async move {
        if let Ok(result) = gtk::gio::spawn_blocking(work).await {
            done(result);
        }
    });
}
