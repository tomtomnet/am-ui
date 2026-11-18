//! "Refresh" (F5, Ctrl+R, the button in Preferences): everything is asked of the service again,
//! and the user sees that it happens and how it ended.
//!
//! The answers the engine has cached are dropped, the library's lists are fetched again (the
//! sidebar's playlists among them) and the pages on screen ask for what they show (the others do
//! when they are next seen). A refresh is over when all of that is. Whoever takes part holds a
//! `Task` for as long as it works: however that ends (an answer, a problem, a page closed
//! meanwhile), the task goes with it, so a refresh cannot stay open.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::glib;

use crate::account::AccountState;
use crate::app::ctx;
use crate::data::Problem;
use crate::signal::{Signal, background};

/// A refresh is given up after this long (a list of the library in pages takes its time).
const PATIENCE: Duration = Duration::from_secs(180);

/// What happens when a refresh is over, with what went wrong.
type End = Box<dyn FnOnce(Option<Problem>)>;

/// One refresh while it runs.
struct Run {
    /// The tasks that are not over.
    open: Cell<usize>,
    /// The first thing that went wrong.
    problem: RefCell<Option<Problem>>,
    /// Taken when the end has come.
    end: RefCell<Option<End>>,
}

impl Run {
    fn new(end: impl FnOnce(Option<Problem>) + 'static) -> Rc<Run> {
        Rc::new(Run { open: Cell::new(0), problem: RefCell::default(), end: RefCell::new(Some(Box::new(end))) })
    }

    fn close(&self, problem: Option<Problem>) {
        if let Some(end) = self.end.take() {
            end(problem);
        }
    }
}

/// What takes part in a refresh holds this while it works. `failed` says what went wrong; letting
/// go of it says that it is over.
pub struct Task {
    run: Option<Rc<Run>>,
}

impl Task {
    pub fn failed(&self, problem: &Problem) {
        if let Some(run) = &self.run {
            run.problem.borrow_mut().get_or_insert_with(|| problem.clone());
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(run) = self.run.take() {
            run.open.set(run.open.get() - 1);
            if run.open.get() == 0 {
                let problem = run.problem.take();
                run.close(problem);
            }
        }
    }
}

/// Fresh data is wanted: what `Ctx::refreshed` tells the pages. A page that goes to fetch takes
/// a task from it.
pub struct Refreshing {
    run: Option<Rc<Run>>,
}

impl Refreshing {
    /// Nobody waits for the pages (after a sign-in, with another catalog region).
    pub fn unobserved() -> Refreshing {
        Refreshing { run: None }
    }

    pub fn task(&self) -> Task {
        if let Some(run) = &self.run {
            run.open.set(run.open.get() + 1);
        }
        Task { run: self.run.clone() }
    }
}

/// The words for how a refresh ended.
fn outcome(problem: Option<&Problem>) -> String {
    match problem {
        None => "Library updated".to_string(),
        Some(problem) => format!("Couldn't update your library: {}", problem.message),
    }
}

/// The refresh of the application: at most one runs.
#[derive(Default)]
pub struct Refresh {
    current: RefCell<Option<Rc<Run>>>,
    /// A refresh started (true) or ended.
    pub running: Signal<bool>,
    /// A refresh ended: what to tell the user, and whether it went well.
    pub finished: Signal<(String, bool)>,
}

impl Refresh {
    pub fn is_running(&self) -> bool {
        self.current.borrow().is_some()
    }

    /// Fetch everything again. While a refresh runs this does nothing: it is what was asked for.
    pub fn start(self: &Rc<Self>) {
        if self.is_running() {
            return;
        }
        let ctx = ctx();
        if ctx.account.state() == AccountState::SignedOut {
            ctx.toast.emit(&"Sign in to update your library".to_string());
            return;
        }
        let weak = Rc::downgrade(self);
        let run = Run::new(move |problem| {
            if let Some(refresh) = weak.upgrade() {
                refresh.ended(problem);
            }
        });
        self.current.replace(Some(run.clone()));
        self.running.emit(&true);
        give_up_after(&run, PATIENCE);
        let refreshing = Refreshing { run: Some(run) };
        // the first task: until everything that follows has been started
        let starting = refreshing.task();
        let music = ctx.music.clone();
        background(
            move || music.refresh(),
            move |()| {
                let ctx = crate::app::ctx();
                ctx.favorites.refresh(&ctx.music);
                let lists = refreshing.task();
                ctx.library.reload_then(&ctx.music, move |problem| {
                    if let Some(problem) = &problem {
                        lists.failed(problem);
                    }
                });
                ctx.refreshed.emit(&refreshing);
                drop(starting);
            },
        );
    }

    fn ended(&self, problem: Option<Problem>) {
        self.current.replace(None);
        self.running.emit(&false);
        self.finished.emit(&(outcome(problem.as_ref()), problem.is_none()));
    }
}

/// End `run` after `patience` if it has not ended by then; what is still on its way goes on.
fn give_up_after(run: &Rc<Run>, patience: Duration) {
    let run: Weak<Run> = Rc::downgrade(run);
    glib::timeout_add_local_once(patience, move || {
        if let Some(run) = run.upgrade() {
            run.close(Some(Problem::new("Apple Music is taking too long to answer")));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the end of a run was told; `None` while it has not ended.
    type Told = Rc<RefCell<Option<Option<Problem>>>>;

    fn watched() -> (Rc<Run>, Told) {
        let told = Rc::new(RefCell::new(None));
        let seen = told.clone();
        let run = Run::new(move |problem| {
            assert!(seen.replace(Some(problem)).is_none(), "a run ends once");
        });
        (run, told)
    }

    #[test]
    fn a_refresh_ends_with_its_last_task() {
        let (run, told) = watched();
        let refreshing = Refreshing { run: Some(run) };
        let (one, two) = (refreshing.task(), refreshing.task());
        drop(one);
        assert!(told.borrow().is_none());
        let three = refreshing.task();
        drop(two);
        assert!(told.borrow().is_none(), "one more is at work");
        drop(three);
        assert_eq!(*told.borrow(), Some(None));
    }

    #[test]
    fn what_went_wrong_first_is_what_is_told() {
        let (run, told) = watched();
        let refreshing = Refreshing { run: Some(run) };
        let (one, two) = (refreshing.task(), refreshing.task());
        two.failed(&Problem::new("The network is down"));
        one.failed(&Problem::new("Something else"));
        drop(two);
        drop(one);
        assert_eq!(*told.borrow(), Some(Some(Problem::new("The network is down"))));
        assert_eq!(
            outcome(told.borrow().as_ref().unwrap().as_ref()),
            "Couldn't update your library: The network is down"
        );
        assert_eq!(outcome(None), "Library updated");
    }

    #[test]
    fn giving_up_ends_a_refresh_and_what_follows_changes_nothing() {
        let (run, told) = watched();
        let refreshing = Refreshing { run: Some(run.clone()) };
        let task = refreshing.task();
        run.close(Some(Problem::new("Too long")));
        assert_eq!(*told.borrow(), Some(Some(Problem::new("Too long"))));
        // (the end would panic if it were told again)
        drop(task);
    }

    #[test]
    fn a_task_nobody_waits_for_does_nothing() {
        let refreshing = Refreshing::unobserved();
        let task = refreshing.task();
        task.failed(&Problem::new("Nobody hears this"));
        drop(task);
    }
}
