//! Display-synchronised scrolling for a GtkScrolledWindow.
//!
//! GTK moves the content once per input event. Input events and display refreshes are not in
//! step, so at 240 Hz some refreshes get two events and some none, and the eye sees the motion
//! stutter; wheel clicks jump a whole step at once. This controller takes the scroll events
//! before the scrolled window does and moves the content from a tick callback instead: every
//! refresh the shown position moves toward the position the input asked for.
//!
//! * touchpad: the target follows the fingers 1:1 (GTK's own 2.5 px per unit), the shown position
//!   follows the target with a short time constant;
//! * lifting the fingers while moving starts a fling: the velocity of the last 70 ms decays
//!   exponentially (GTK's friction by default);
//! * a wheel click moves the target by one step and the shown position eases toward it.
//!
//! The tick callback runs only while something moves or fingers rest on the touchpad.
//!
//! A row that scrolls sideways inside a page (a shelf) gets a controller of its own
//! (`attach_sideways`). The page's controller sees every event first. A touchpad sequence that
//! begins over such a row belongs to the page or to the row, whichever way it sets out, until the
//! fingers are lifted: the page keeps the ones that move it and leaves the others to the row.
//! A wheel moves the page, and the row when it is turned sideways or with Shift held.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// GtkScrolledWindow's factor for touchpad units, so the travel matches every other GTK app.
const TOUCHPAD_FACTOR: f64 = 2.5;
const FLING_WINDOW_MS: u32 = 70;
const FLING_MIN_SPEED: f64 = 200.0;
/// A change of the adjustment by more than this while the content moves is a deliberate jump.
const JUMP: f64 = 200.0;
/// The travel (in touchpad units) after which a sequence that began over a row that scrolls
/// sideways is known to be the page's or the row's.
const LOCK_DISTANCE: f64 = 3.0;
/// Rate (1/s) of the spring behind `glide_to`: the content arrives in about a third of a second.
const GLIDE_RATE: f64 = 18.0;

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Time constant (s) with which the shown position follows the fingers; 0 = no smoothing.
    pub follow_tau: f64,
    /// Time constant (s) of the wheel-step animation.
    pub wheel_tau: f64,
    /// Logical px per wheel click.
    pub wheel_step: f64,
    /// Exponential decay rate of a fling's velocity (1/s).
    pub friction: f64,
    /// A fling ends below this speed (px/s).
    pub stop_speed: f64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { follow_tau: 0.010, wheel_tau: 0.045, wheel_step: 140.0, friction: 4.0, stop_speed: 30.0 }
    }
}

/// Which way a controller moves its scrolled window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Vertical,
    Sideways,
}

/// Whose a touchpad sequence is, from its first event until the fingers are lifted.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sequence {
    /// No fingers move on the touchpad.
    None,
    /// It began over a row that scrolls sideways and has not gone far enough to tell whose it
    /// is: the travel so far.
    Open(f64, f64),
    /// This controller moves its content with it.
    Ours,
    /// It was left to the row under the pointer.
    Theirs,
}

impl Sequence {
    /// An open sequence that has travelled (`x`, `y`): the row's when it goes sideways more than
    /// up or down, the page's otherwise.
    fn travelled(x: f64, y: f64) -> Sequence {
        if x.abs() + y.abs() < LOCK_DISTANCE {
            Sequence::Open(x, y)
        } else if x.abs() > y.abs() {
            Sequence::Theirs
        } else {
            Sequence::Ours
        }
    }
}

struct State {
    target: f64,
    pos: f64,
    velocity: f64,
    tau: f64,
    sequence: Sequence,
    /// `glide_to` is moving the content: `speed` is the spring's velocity (px/s).
    glide: bool,
    speed: f64,
    last_frame: Option<i64>,
    samples: VecDeque<(u32, f64)>,
    setting: bool,
}

impl State {
    /// Whatever was moving stops where it is.
    fn halt(&mut self) {
        self.velocity = 0.0;
        self.target = self.pos;
        self.glide = false;
        self.speed = 0.0;
    }
}

struct Inner {
    sw: glib::WeakRef<gtk::ScrolledWindow>,
    axis: Axis,
    tuning: Tuning,
    state: RefCell<State>,
    ticking: Cell<bool>,
}

impl Inner {
    fn adjustment(&self, sw: &gtk::ScrolledWindow) -> gtk::Adjustment {
        match self.axis {
            Axis::Vertical => sw.vadjustment(),
            Axis::Sideways => sw.hadjustment(),
        }
    }
}

#[derive(Clone)]
pub struct SmoothScroll {
    inner: Rc<Inner>,
}

impl SmoothScroll {
    /// Scroll the content of `sw` up and down.
    pub fn attach(sw: &gtk::ScrolledWindow, tuning: Tuning) -> SmoothScroll {
        SmoothScroll::attach_axis(sw, sw.upcast_ref(), tuning, Axis::Vertical)
    }

    /// Scroll the content of `sw` sideways: for a row inside a page (a shelf). `around` is the
    /// overlay that holds the row and what lies over it (the arrow buttons of a shelf), or `sw`
    /// itself: the row moves with every scroll event that reaches any of it. The page's
    /// controller keeps what moves the page.
    pub fn attach_sideways(sw: &gtk::ScrolledWindow, around: &impl IsA<gtk::Widget>, tuning: Tuning) -> SmoothScroll {
        SmoothScroll::attach_axis(sw, around.upcast_ref(), tuning, Axis::Sideways)
    }

    fn attach_axis(sw: &gtk::ScrolledWindow, around: &gtk::Widget, tuning: Tuning, axis: Axis) -> SmoothScroll {
        let adj = match axis {
            Axis::Vertical => sw.vadjustment(),
            Axis::Sideways => sw.hadjustment(),
        };
        let inner = Rc::new(Inner {
            sw: sw.downgrade(),
            axis,
            tuning,
            state: RefCell::new(State {
                target: adj.value(),
                pos: adj.value(),
                velocity: 0.0,
                tau: tuning.follow_tau,
                sequence: Sequence::None,
                glide: false,
                speed: 0.0,
                last_frame: None,
                samples: VecDeque::with_capacity(64),
                setting: false,
            }),
            ticking: Cell::new(false),
        });

        listen(&inner, sw.upcast_ref(), true);
        if around != sw.upcast_ref::<gtk::Widget>() {
            // what lies over the row moves it too; a click there is the arrow button's own
            listen(&inner, around, false);
        }

        if axis == Axis::Vertical {
            // What the page left to a row and nothing inside took comes back up here (the pointer
            // slid off the row during the sequence): the scrolled window must not scroll with it
            // either. Controllers added later run first, so this one is ahead of its own.
            let unclaimed = gtk::EventControllerLegacy::new();
            unclaimed.set_propagation_phase(gtk::PropagationPhase::Bubble);
            unclaimed.connect_event(|_, event| {
                let zoom = event.modifier_state().contains(gdk::ModifierType::CONTROL_MASK);
                if event.event_type() == gdk::EventType::Scroll && !zoom {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            sw.add_controller(unclaimed);
        }

        // Anything else that moves the adjustment (keys, the scrollbar, scroll_to) takes over.
        {
            let inner = inner.clone();
            adj.connect_value_changed(move |adj| {
                let mut st = inner.state.borrow_mut();
                if st.setting {
                    return;
                }
                let shift = adj.value() - st.pos;
                if shift.abs() <= 1.0 {
                    // GTK's list widgets write the value back rounded to whole pixels: that is
                    // still this controller's own position
                } else if inner.ticking.get() && shift.abs() < JUMP {
                    // the list moved its own origin (a row turned out taller or shorter than
                    // it had estimated): the motion goes on from the corrected position
                    st.pos += shift;
                    st.target += shift;
                } else {
                    // keys, the scrollbar, scroll_to: somebody else took over
                    st.pos = adj.value();
                    st.halt();
                }
            });
        }
        SmoothScroll { inner }
    }

    /// True while a fling or an animation is moving the content.
    pub fn moving(&self) -> bool {
        self.inner.ticking.get()
    }

    /// Where the content is heading: its position once it rests.
    pub fn target(&self) -> f64 {
        self.inner.state.borrow().target
    }

    /// Move to `value` with the wheel's easing (keyboard paging, "scroll to").
    pub fn animate_to(&self, value: f64) {
        let Some(sw) = self.inner.sw.upgrade() else { return };
        let (lower, max) = bounds(&self.inner.adjustment(&sw));
        {
            let mut st = self.inner.state.borrow_mut();
            st.halt();
            st.target = value.clamp(lower, max);
            st.tau = self.inner.tuning.wheel_tau;
        }
        start(&self.inner);
    }

    /// Move to `value` over a longer way (a page of a shelf): gently off, gently to rest, and
    /// a new destination given on the way is taken up without a jolt.
    pub fn glide_to(&self, value: f64) {
        let Some(sw) = self.inner.sw.upgrade() else { return };
        let (lower, max) = bounds(&self.inner.adjustment(&sw));
        {
            let mut st = self.inner.state.borrow_mut();
            st.velocity = 0.0;
            st.target = value.clamp(lower, max);
            st.glide = true;
        }
        start(&self.inner);
    }
}

/// Take the scroll events that reach `widget`; with `presses`, a click on it stops a fling.
fn listen(inner: &Rc<Inner>, widget: &gtk::Widget, presses: bool) {
    // Every scroll event is taken here, in the capture phase, as a raw event: the scrolled
    // window must not see any of them, not even the one that says the fingers were lifted
    // (it would start its own kinetic scrolling and fight this one over the adjustment).
    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let inner = inner.clone();
    controller.connect_event(move |_, event| match event.event_type() {
        gdk::EventType::Scroll => match event.downcast_ref::<gdk::ScrollEvent>() {
            Some(scroll) => on_scroll(&inner, scroll),
            None => glib::Propagation::Proceed,
        },
        gdk::EventType::TouchpadHold => {
            // fingers resting on the touchpad stop a fling, as everywhere in GTK
            inner.state.borrow_mut().halt();
            glib::Propagation::Proceed
        }
        // a click stops a fling too
        gdk::EventType::ButtonPress if presses => {
            inner.state.borrow_mut().halt();
            glib::Propagation::Proceed
        }
        _ => glib::Propagation::Proceed,
    });
    widget.add_controller(controller);
}

/// Whether the pointer rests on a row inside `page` that can move sideways (a shelf): on its
/// scrolled window, or on what lies over that in an overlay (an arrow button). GTK marks the
/// widget under the pointer and all its ancestors as hovered.
fn sideways_row_under_pointer(page: &gtk::ScrolledWindow) -> bool {
    let is_row = |widget: &gtk::Widget| {
        widget.downcast_ref::<gtk::ScrolledWindow>().is_some_and(|row| {
            let adj = row.hadjustment();
            let moves = adj.upper() - adj.page_size() > adj.lower() + 0.5;
            moves && row.can_target() && row.hscrollbar_policy() != gtk::PolicyType::Never
        })
    };
    let mut next = page.first_child();
    while let Some(widget) = next {
        if !widget.state_flags().contains(gtk::StateFlags::PRELIGHT) {
            next = widget.next_sibling();
            continue;
        }
        if is_row(&widget) {
            return true;
        }
        if widget.is::<gtk::Overlay>() {
            let mut child = widget.first_child();
            while let Some(over) = child {
                if is_row(&over) {
                    return true;
                }
                child = over.next_sibling();
            }
        }
        next = widget.first_child();
    }
    false
}

fn on_scroll(inner: &Rc<Inner>, event: &gdk::ScrollEvent) -> glib::Propagation {
    let modifiers = event.modifier_state();
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        return glib::Propagation::Proceed;
    }
    let Some(sw) = inner.sw.upgrade() else { return glib::Propagation::Proceed };
    let smooth = event.direction() == gdk::ScrollDirection::Smooth;
    let (dx, dy) = match (event.direction(), event.deltas()) {
        (gdk::ScrollDirection::Smooth, deltas) => deltas,
        // a wheel says how far it turned (parts of a click for one with fine steps; and for a
        // wheel turned sideways the direction GDK names is not the one it turned in)
        (_, (x, y)) if x != 0.0 || y != 0.0 => (x, y),
        (gdk::ScrollDirection::Up, _) => (0.0, -1.0),
        (gdk::ScrollDirection::Down, _) => (0.0, 1.0),
        (gdk::ScrollDirection::Left, _) => (-1.0, 0.0),
        (gdk::ScrollDirection::Right, _) => (1.0, 0.0),
        _ => (0.0, 0.0),
    };
    let wheel = !smooth || event.unit() == gdk::ScrollUnit::Wheel;
    let time = event.time();
    let mut st = inner.state.borrow_mut();
    if !wheel && st.sequence == Sequence::None {
        // first event of a touchpad sequence: whatever was moving stops under the fingers
        st.halt();
        st.samples.clear();
        st.sequence = match inner.axis {
            Axis::Vertical if sideways_row_under_pointer(&sw) => Sequence::Open(0.0, 0.0),
            _ => Sequence::Ours,
        };
    }
    // the part of the movement that is this controller's
    let delta = match (inner.axis, wheel) {
        // a row moves with whatever reaches it: a wheel turned sideways, or turned with Shift held
        (Axis::Sideways, true) => {
            if dx != 0.0 {
                dx
            } else {
                dy
            }
        }
        (Axis::Sideways, false) => dx,
        (Axis::Vertical, true) => {
            let sideways = dx.abs() > dy.abs() || modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            if sideways && st.sequence != Sequence::Ours && sideways_row_under_pointer(&sw) {
                return glib::Propagation::Proceed;
            }
            dy
        }
        (Axis::Vertical, false) => {
            let mut dy = dy;
            if let Sequence::Open(x, y) = st.sequence {
                st.sequence = Sequence::travelled(x + dx, y + dy);
                // nothing of the way so far is lost when it turns out to be the page's
                dy += y;
            }
            match st.sequence {
                Sequence::Ours => dy,
                undecided_or_theirs => {
                    if event.is_stop() {
                        st.sequence = Sequence::None;
                    }
                    return match undecided_or_theirs {
                        Sequence::Theirs => glib::Propagation::Proceed,
                        _ => glib::Propagation::Stop,
                    };
                }
            }
        }
    };
    if delta != 0.0 {
        st.velocity = 0.0;
        st.glide = false;
        st.speed = 0.0;
        if wheel {
            st.target += delta * inner.tuning.wheel_step;
            st.tau = inner.tuning.wheel_tau;
        } else {
            let px = delta * TOUCHPAD_FACTOR;
            st.target += px;
            st.tau = inner.tuning.follow_tau;
            if st.samples.len() == 64 {
                st.samples.pop_front();
            }
            st.samples.push_back((time, px));
        }
        let (lower, max) = bounds(&inner.adjustment(&sw));
        st.target = st.target.clamp(lower, max);
    }
    if !wheel && event.is_stop() {
        st.sequence = Sequence::None;
        let v = fling_velocity(&st.samples, time);
        st.samples.clear();
        if v.abs() > FLING_MIN_SPEED {
            st.velocity = v;
        }
    }
    drop(st);
    start(inner);
    glib::Propagation::Stop
}

fn bounds(adj: &gtk::Adjustment) -> (f64, f64) {
    let lower = adj.lower();
    (lower, (adj.upper() - adj.page_size()).max(lower))
}

/// px/s over the samples of the last FLING_WINDOW_MS before `t_end` (0 with fewer than three).
fn fling_velocity(samples: &VecDeque<(u32, f64)>, t_end: u32) -> f64 {
    let (mut sum, mut n) = (0.0, 0u32);
    let (mut t_first, mut t_last) = (0u32, 0u32);
    for &(t, d) in samples.iter().rev() {
        if t_end.wrapping_sub(t) > FLING_WINDOW_MS {
            break;
        }
        if n == 0 {
            t_last = t;
        }
        t_first = t;
        sum += d;
        n += 1;
    }
    if n < 3 || t_last <= t_first {
        return 0.0;
    }
    // the oldest sample's delta covers the interval before it: count one average interval for it
    let span_s = (t_last - t_first) as f64 * n as f64 / (n - 1) as f64 / 1000.0;
    sum / span_s
}

fn start(inner: &Rc<Inner>) {
    if inner.ticking.replace(true) {
        return;
    }
    inner.state.borrow_mut().last_frame = None;
    let Some(sw) = inner.sw.upgrade() else {
        inner.ticking.set(false);
        return;
    };
    let weak = Rc::downgrade(inner);
    sw.add_tick_callback(move |sw, clock| match weak.upgrade() {
        Some(inner) => tick(&inner, sw, clock),
        None => glib::ControlFlow::Break,
    });
}

/// One step of the spring behind `glide_to`: the distance to the destination and the speed after
/// `dt` seconds. Critically damped, so it starts gently and comes to rest without swinging past.
fn glide_step(distance: f64, speed: f64, dt: f64) -> (f64, f64) {
    let pull = speed + GLIDE_RATE * distance;
    let decay = (-GLIDE_RATE * dt).exp();
    ((distance + pull * dt) * decay, (speed - GLIDE_RATE * pull * dt) * decay)
}

fn tick(inner: &Rc<Inner>, sw: &gtk::ScrolledWindow, clock: &gdk::FrameClock) -> glib::ControlFlow {
    let now = clock.frame_time();
    let adj = inner.adjustment(sw);
    let (lower, max) = bounds(&adj);
    let mut st = inner.state.borrow_mut();
    // the first tick after a start stands for one refresh
    let dt = match st.last_frame {
        Some(prev) => ((now - prev) as f64 / 1e6).clamp(0.0, 0.05),
        None => 1.0 / 240.0,
    };
    st.last_frame = Some(now);
    if st.velocity != 0.0 {
        let k = inner.tuning.friction;
        let decay = (-k * dt).exp();
        st.target += st.velocity * (1.0 - decay) / k;
        st.velocity *= decay;
        if st.velocity.abs() < inner.tuning.stop_speed {
            st.velocity = 0.0;
        }
    }
    if st.target <= lower || st.target >= max {
        st.target = st.target.clamp(lower, max);
        st.velocity = 0.0;
    }
    let settled = if st.glide {
        let (distance, speed) = glide_step(st.pos - st.target, st.speed, dt);
        st.pos = st.target + distance;
        st.speed = speed;
        distance.abs() < 0.25 && speed.abs() < 10.0
    } else {
        let alpha = if st.tau <= 0.0 { 1.0 } else { 1.0 - (-dt / st.tau).exp() };
        st.pos += (st.target - st.pos) * alpha;
        st.velocity == 0.0 && (st.target - st.pos).abs() < 0.25
    };
    if settled {
        st.pos = st.target;
        st.glide = false;
        st.speed = 0.0;
    }
    let (value, gesture) = (st.pos, st.sequence == Sequence::Ours);
    st.setting = true;
    drop(st);
    adj.set_value(value);
    let mut st = inner.state.borrow_mut();
    st.setting = false;
    if settled && !gesture {
        st.last_frame = None;
        inner.ticking.set(false);
        return glib::ControlFlow::Break;
    }
    glib::ControlFlow::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_over_a_row_goes_the_way_it_sets_out() {
        // too short to tell
        assert_eq!(Sequence::travelled(0.4, -0.9), Sequence::Open(0.4, -0.9));
        // sideways: the row's, in either direction
        assert_eq!(Sequence::travelled(3.5, 0.5), Sequence::Theirs);
        assert_eq!(Sequence::travelled(-6.0, 2.0), Sequence::Theirs);
        // up or down: the page's, and so is a diagonal
        assert_eq!(Sequence::travelled(0.5, -3.0), Sequence::Ours);
        assert_eq!(Sequence::travelled(2.0, 2.0), Sequence::Ours);
    }

    #[test]
    fn a_glide_arrives_without_swinging_past() {
        let dt = 1.0 / 240.0;
        let (mut distance, mut speed) = (-1000.0, 0.0);
        let mut fastest: f64 = 0.0;
        for frame in 1..=240 {
            let before = distance;
            (distance, speed) = glide_step(distance, speed, dt);
            // always toward the destination, never beyond it
            assert!(distance >= before && distance <= 0.0, "frame {frame}: {before} -> {distance}");
            fastest = fastest.max(speed);
            if frame == 1 {
                // a gentle start: the first frame moves a few pixels of the thousand
                assert!(distance - before < 3.0, "first step {}", distance - before);
            }
            if frame == 96 {
                // 0.4 s: all but there
                assert!(distance.abs() < 10.0, "after 0.4 s: {distance}");
            }
        }
        assert!(distance.abs() < 0.01 && speed.abs() < 0.1);
        assert!(fastest > 4000.0 && fastest < 9000.0, "peak speed {fastest}");
    }

    #[test]
    fn a_glide_takes_a_new_destination_in_its_stride() {
        // the same motion whether it is computed in one step or in many
        let (mut distance, mut speed) = (-600.0, 0.0);
        for _ in 0..24 {
            (distance, speed) = glide_step(distance, speed, 1.0 / 240.0);
        }
        let (once, once_speed) = glide_step(-600.0, 0.0, 0.1);
        assert!((distance - once).abs() < 1e-6 && (speed - once_speed).abs() < 1e-6);
        // a destination moved 500 px further on keeps the speed: no jolt
        let (further, further_speed) = glide_step(distance - 500.0, speed, 1.0 / 240.0);
        assert!((further_speed - speed).abs() < speed * 0.2 + 50.0, "{speed} -> {further_speed}");
        assert!(further > distance - 500.0);
    }

    #[test]
    fn fling_speed_comes_from_the_last_moments() {
        let mut samples = VecDeque::new();
        // 10 px every 8 ms: 1250 px/s
        for n in 0..12u32 {
            samples.push_back((1000 + n * 8, 10.0));
        }
        let v = fling_velocity(&samples, 1000 + 11 * 8);
        assert!((v - 1250.0).abs() < 1.0, "{v}");
        // fingers that rested before lifting leave nothing to fling with
        assert_eq!(fling_velocity(&samples, 1000 + 11 * 8 + 200), 0.0);
        assert_eq!(fling_velocity(&VecDeque::new(), 5), 0.0);
    }
}
