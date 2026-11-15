//! One Cavern process at a time, in the order current > next > previous.
//!
//! The render of the track the user wants to hear now (a load, switch or rerender job, or the
//! session `set_current()` names) runs at once and freezes whatever else renders (`pause()` of
//! the session: SIGSTOP). A prefetch render waits while the *current* track still renders and
//! otherwise freezes a stale render (a previous track's) to take its place. `poll()` and
//! `release()` resume the most recently frozen render once the running one has finished or was
//! stopped.
//!
//! A job that was given its turn keeps it until its render has started (or the job gave up):
//! nothing is resumed and nobody else is let through in between, so the render that was frozen
//! for it stays frozen and the new one is the gate's running render from its first moment.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::jobs::Job;
use super::seams::{Source, same_source};
use super::{PlayerError, RENDER_POLL_S};
use crate::loudness::lock;
use crate::output::{PlaybackLog, source_label};
use crate::render::PcmSource;
use crate::util::Event;

#[derive(Default)]
struct GateState {
    running: Option<Arc<dyn Source>>,
    /// Frozen renders, the most recently frozen one last.
    paused: Vec<Arc<dyn Source>>,
    /// The source the player currently plays (any kind).
    current: Option<Arc<dyn Source>>,
    /// A job has its turn and is about to start its render.
    starting: bool,
}

pub(super) struct RenderGate {
    state: Mutex<GateState>,
    changed: Condvar,
    log: Arc<PlaybackLog>,
}

/// A job's turn to start its render; given back when it is dropped without `started()`.
pub(super) struct Turn<'a> {
    gate: &'a RenderGate,
    used: bool,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.used {
            let mut state = lock(&self.gate.state);
            state.starting = false;
            self.gate.reap(&mut state);
            self.gate.changed.notify_all();
        }
    }
}

/// The name of a source in log lines.
pub(super) fn label(source: &Arc<dyn Source>) -> String {
    let source: &dyn PcmSource = &**source;
    source_label(Some(source))
}

fn finished(session: &Arc<dyn Source>) -> bool {
    session.done() || session.error().is_some() || session.running() == Some(false)
}

/// Whether the job's render is for the track the user wants to hear now.
fn priority(job: Option<&Arc<Job>>) -> bool {
    job.is_some_and(|job| job.purpose().priority())
}

impl RenderGate {
    pub fn new(log: Arc<PlaybackLog>) -> RenderGate {
        RenderGate { state: Mutex::default(), changed: Condvar::new(), log }
    }

    fn reap(&self, state: &mut GateState) {
        if state.running.as_ref().is_some_and(finished) {
            state.running = None;
        }
        state.paused.retain(|session| !finished(session));
        if state.running.is_none()
            && !state.starting
            && let Some(session) = state.paused.pop()
        {
            session.resume();
            self.log.write(&format!("render resume {}", label(&session)));
            state.running = Some(session);
            self.changed.notify_all();
        }
    }

    fn freeze(&self, state: &mut GateState, session: Arc<dyn Source>) {
        session.pause();
        self.log.write(&format!("render pause {} (another render takes the CPU)", label(&session)));
        state.paused.push(session);
    }

    fn freeze_running(&self, state: &mut GateState) {
        if let Some(session) = state.running.take() {
            self.freeze(state, session);
        }
    }

    fn wait<'a>(&self, state: MutexGuard<'a, GateState>) -> MutexGuard<'a, GateState> {
        let poll = Duration::from_secs_f64(RENDER_POLL_S);
        self.changed.wait_timeout(state, poll).unwrap_or_else(|poisoned| poisoned.into_inner()).0
    }

    /// Block until this job may start its render; [`PlayerError::Cancelled`] when `cancel` is
    /// set first. The turn is the job's until it calls `started()` or drops it.
    pub fn wait_turn(&self, job: Option<&Arc<Job>>, cancel: &Event) -> Result<Turn<'_>, PlayerError> {
        let mut state = lock(&self.state);
        loop {
            if cancel.is_set() {
                return Err(PlayerError::Cancelled);
            }
            self.reap(&mut state);
            if state.starting {
                state = self.wait(state); // another job is about to start its render
                continue;
            }
            let yields = match &state.running {
                None => true,
                // the user wants this track now
                Some(_) if priority(job) => true,
                // a previous track's render yields to the next track's
                Some(running) => !state.current.as_ref().is_some_and(|current| same_source(running, current)),
            };
            if yields {
                self.freeze_running(&mut state);
                state.starting = true;
                return Ok(Turn { gate: self, used: false });
            }
            state = self.wait(state);
        }
    }

    /// The job's render runs. A render that came to run during the job's turn (the one of the
    /// track that became current meanwhile) is frozen for it, or keeps the processor when this
    /// job only prefetches.
    pub fn started(&self, mut turn: Turn<'_>, job: Option<&Arc<Job>>, session: &Arc<dyn Source>) {
        turn.used = true;
        let mut state = lock(&self.state);
        state.starting = false;
        state.paused.retain(|frozen| !same_source(frozen, session));
        let other = state.running.take().filter(|running| !same_source(running, session) && !finished(running));
        let other_is_current = match (&other, &state.current) {
            (Some(other), Some(current)) => same_source(other, current),
            _ => false,
        };
        match other {
            Some(other) if other_is_current && !priority(job) => {
                state.running = Some(other);
                self.freeze(&mut state, session.clone());
            }
            other => {
                if let Some(other) = other {
                    self.freeze(&mut state, other);
                }
                state.running = Some(session.clone());
            }
        }
        if priority(job) {
            state.current = Some(session.clone());
        }
        self.changed.notify_all();
    }

    /// The player now plays `source` (any source): a frozen render of it resumes at once.
    pub fn set_current(&self, source: &Arc<dyn Source>) {
        let mut state = lock(&self.state);
        state.current = Some(source.clone());
        self.reap(&mut state);
        if state.running.as_ref().is_some_and(|running| same_source(running, source)) {
            return;
        }
        let Some(position) = state.paused.iter().position(|frozen| same_source(frozen, source)) else { return };
        self.freeze_running(&mut state);
        let session = state.paused.remove(position);
        session.resume();
        self.log.write(&format!("render resume {} (now the current track)", label(&session)));
        state.running = Some(session);
        self.changed.notify_all();
    }

    pub fn release(&self, session: &Arc<dyn Source>) {
        let mut state = lock(&self.state);
        if state.running.as_ref().is_some_and(|running| same_source(running, session)) {
            state.running = None;
        }
        state.paused.retain(|frozen| !same_source(frozen, session));
        self.reap(&mut state);
        self.changed.notify_all();
    }

    pub fn poll(&self) {
        self.reap(&mut lock(&self.state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::jobs::Purpose;
    use crate::player::jobs::testing::{Dummy, idle_job};
    use std::sync::atomic::Ordering;
    use std::thread;

    fn gate() -> (tempfile::TempDir, Arc<RenderGate>) {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(PlaybackLog::new(dir.path().join("playback.log")));
        (dir, Arc::new(RenderGate::new(log)))
    }

    fn session(name: &str) -> (Arc<Dummy>, Arc<dyn Source>) {
        let dummy = Dummy::new(name);
        (dummy.clone(), dummy)
    }

    fn paused(dummy: &Dummy) -> bool {
        dummy.paused.load(Ordering::SeqCst)
    }

    /// Start a render the way a job does.
    fn start(gate: &RenderGate, job: &Arc<Job>, session: &Arc<dyn Source>) {
        let turn = gate.wait_turn(Some(job), &Event::new()).unwrap();
        gate.started(turn, Some(job), session);
    }

    #[test]
    fn a_frozen_render_stays_frozen_until_the_new_one_has_started() {
        let (_dir, gate) = gate();
        let (prefetch, load) = (idle_job(Purpose::Prefetch), idle_job(Purpose::Load));
        let (next, next_session) = session("next");
        start(&gate, &prefetch, &next_session);
        // the current track's render takes its turn: the prefetch render is frozen for it
        let turn = gate.wait_turn(Some(&load), &Event::new()).unwrap();
        assert!(paused(&next));
        // the worker looks at the gate before the new render has started: nothing is resumed
        gate.poll();
        assert!(paused(&next), "the turn keeps the frozen render frozen");
        let (current, current_session) = session("current");
        gate.started(turn, Some(&load), &current_session);
        gate.poll();
        assert!(paused(&next) && !paused(&current));
        // once the current track's render is done, the frozen one goes on
        current.finished.store(true, Ordering::SeqCst);
        gate.poll();
        assert!(!paused(&next));
    }

    #[test]
    fn a_prefetch_waits_for_the_current_render_and_replaces_a_previous_one() {
        let (_dir, gate) = gate();
        let (prefetch, load) = (idle_job(Purpose::Prefetch), idle_job(Purpose::Load));
        let (current, current_session) = session("current");
        start(&gate, &load, &current_session);
        let (theirs, job) = (gate.clone(), prefetch.clone());
        let waiting = thread::spawn(move || {
            let turn = theirs.wait_turn(Some(&job), &Event::new()).unwrap();
            let (_, next_session) = session("next");
            theirs.started(turn, Some(&job), &next_session);
        });
        thread::sleep(Duration::from_millis(300));
        assert!(!waiting.is_finished(), "the prefetch render waits while the current track still renders");
        assert!(!paused(&current));
        // the player moves on to a source that is no render: the old render is a previous track's
        let (_, decoded) = session("decoded");
        gate.set_current(&decoded);
        waiting.join().unwrap();
        assert!(paused(&current), "a previous track's render yields to the next track's");
    }

    #[test]
    fn a_turn_that_is_given_back_lets_the_frozen_render_go_on() {
        let (_dir, gate) = gate();
        let (prefetch, load) = (idle_job(Purpose::Prefetch), idle_job(Purpose::Load));
        let (next, next_session) = session("next");
        start(&gate, &prefetch, &next_session);
        let turn = gate.wait_turn(Some(&load), &Event::new()).unwrap();
        assert!(paused(&next));
        drop(turn); // the job failed before its render started
        assert!(!paused(&next));
        // and a cancelled job gets no turn
        let cancelled = Event::new();
        cancelled.set();
        assert!(matches!(gate.wait_turn(Some(&load), &cancelled), Err(PlayerError::Cancelled)));
    }

    #[test]
    fn one_job_at_a_time_is_between_its_turn_and_its_start() {
        let (_dir, gate) = gate();
        let load = idle_job(Purpose::Load);
        let turn = gate.wait_turn(Some(&load), &Event::new()).unwrap();
        let (theirs, job) = (gate.clone(), idle_job(Purpose::Switch));
        let other = thread::spawn(move || {
            let turn = theirs.wait_turn(Some(&job), &Event::new()).unwrap();
            let (_, second) = session("second");
            theirs.started(turn, Some(&job), &second);
        });
        thread::sleep(Duration::from_millis(300));
        assert!(!other.is_finished(), "the second job waits for the first one's render to start");
        let (first, first_session) = session("first");
        gate.started(turn, Some(&load), &first_session);
        other.join().unwrap();
        assert!(paused(&first), "the later job is for the track the user wants now: it freezes the first");
    }

    #[test]
    fn a_render_that_became_current_during_a_turn_is_sorted_out_at_the_start() {
        let (_dir, gate) = gate();
        let (prefetch, load) = (idle_job(Purpose::Prefetch), idle_job(Purpose::Load));
        // the render of a track that follows is at work, then frozen for another prefetch (it is
        // nobody's current track) ...
        let (early, early_session) = session("early");
        start(&gate, &prefetch, &early_session);
        let turn = gate.wait_turn(Some(&prefetch), &Event::new()).unwrap();
        assert!(paused(&early));
        // ... and becomes the current track before the other render has started
        gate.set_current(&early_session);
        assert!(!paused(&early), "the current track's frozen render resumes at once");
        let (late, late_session) = session("late");
        gate.started(turn, Some(&prefetch), &late_session);
        assert!(!paused(&early) && paused(&late), "a prefetch does not take the processor from the current track");
        // a render for the track the user wants now does
        let (wanted, wanted_session) = session("wanted");
        start(&gate, &load, &wanted_session);
        assert!(paused(&early) && paused(&late) && !paused(&wanted));
        // stopping the running render lets the most recently frozen one go on
        gate.release(&wanted_session);
        assert!(!paused(&early) && paused(&late));
    }
}
