//! The feeder thread: it composes the 16-channel blocks, turns them into stereo with the
//! chain, gives them to the consumer, and, for a consumer that can be paused, rewritten and
//! flushed, fades, rewrites and corks so that pause, resume, seeks and new sources act at once
//! (see the module comment of `output`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::consumer::{Consumer, OUT_CHANNELS};
use super::state::{Piece, QueuedEvent, State};
use super::{
    BLOCK_FRAMES, EventKind, FEEDER_GAP_WARN_MS, IDLE_CORK_FRAMES, INTERRUPT_MIN_FRAMES, Inner, LOW_WATER_FRAMES,
    SENT_RING_FRAMES, source_label,
};
use crate::config::CHANNELS;
use crate::dsp;
use crate::loudness::lock;

/// How long a wait for room lasts before the feeder looks at its state again.
const WAIT: Duration = Duration::from_millis(100);
/// While a pause fades out: how often the feeder looks whether the fade has been heard.
const FADING_WAIT: Duration = Duration::from_millis(5);
/// A corked feeder looks at its state at least this often (commands wake it at once).
const IDLE_WAIT: Duration = Duration::from_millis(500);
/// Stopping waits at most this long for the last fade to be played.
const STOP_DRAIN: Duration = Duration::from_millis(500);

/// What the feeder does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    /// Nothing to do until a command arrives.
    Wait,
    Exit,
}

/// Why the stream is corked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Corked {
    Paused,
    /// No source for [`IDLE_CORK_FRAMES`].
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Playing,
    /// Paused by a rewrite: the fade of what was about to be played ends at consumer frame
    /// `until`; silence standing for composed frame `ours` follows until the fade has been
    /// heard, then the stream corks.
    Fading {
        until: i64,
        ours: i64,
    },
    /// Paused without a rewrite (the consumer held too little): the composition fades out and
    /// writes silence, and the stream corks once consumer frame `until`, the last one that was
    /// not silent, has been heard.
    Settling {
        until: i64,
    },
    Corked(Corked),
}

enum Decision {
    Compose,
    Wait,
    Pause,
    Pad,
    Replay,
    Cork(Corked),
    Uncork,
}

/// The composition buffers and the chain.
pub(super) struct Composer {
    /// One block of 16 channels.
    pub(super) block: Vec<f32>,
    /// The block in stereo.
    pub(super) stereo: Vec<f32>,
    pub(super) chain: dsp::Chain,
}

impl Composer {
    pub(super) fn new(settings: dsp::Settings) -> Composer {
        Composer {
            block: vec![0.0; BLOCK_FRAMES * CHANNELS],
            stereo: vec![0.0; BLOCK_FRAMES * OUT_CHANNELS],
            chain: dsp::Chain::new(settings, crate::config::SAMPLE_RATE),
        }
    }

    /// Compose the next block (lock held): new settings for the chain first, the 16 channels
    /// (`last`: the fade that ends the stream), the chain, the recent-audio ring.
    pub(super) fn compose(&mut self, state: &mut State, last: bool) {
        if let Some(settings) = state.dsp_pending.take() {
            self.chain.set(settings);
        }
        state.fill_block(&mut self.block, last);
        self.chain.process(&self.block, &mut self.stereo);
        state.ring_write(&self.stereo);
    }
}

/// The frames given to the consumer, by consumer frame: what a fade starts from and what a
/// pause takes back.
struct SentRing {
    frames: Vec<f32>,
}

impl SentRing {
    fn capacity(&self) -> i64 {
        (self.frames.len() / OUT_CHANNELS) as i64
    }

    fn slot(&self, frame: i64) -> usize {
        frame.rem_euclid(self.capacity()) as usize * OUT_CHANNELS
    }

    fn write(&mut self, at: i64, stereo: &[f32]) {
        for (index, frame) in stereo.as_chunks::<OUT_CHANNELS>().0.iter().enumerate() {
            let slot = self.slot(at + index as i64);
            self.frames[slot..slot + OUT_CHANNELS].copy_from_slice(frame);
        }
    }

    fn read_into(&self, at: i64, count: usize, out: &mut Vec<f32>) {
        for index in 0..count {
            let slot = self.slot(at + index as i64);
            out.extend_from_slice(&self.frames[slot..slot + OUT_CHANNELS]);
        }
    }
}

/// What a pause took back from the consumer, given again on resume.
#[derive(Default)]
struct Replay {
    frames: Vec<f32>,
    /// Frames of it given again so far.
    sent: usize,
    /// The pieces of those frames, the first one where they begin.
    pieces: Vec<Piece>,
}

impl Replay {
    fn remaining(&self) -> usize {
        self.frames.len() / OUT_CHANNELS - self.sent
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.sent = 0;
        self.pieces.clear();
    }
}

/// The feeder's view of its own lateness, one window per second.
struct Monitor {
    last_write_end: Option<Instant>,
    /// Time spent waiting for room since the last write (the consumer's, not ours).
    waited: Duration,
    window_start: Instant,
    /// Largest gap between two writes in the window, seconds.
    window_max: f64,
    window_fill_min: Option<i64>,
    low_logged: Option<Instant>,
}

/// Where a rewrite cuts the consumer's stream (consumer frames).
struct Cut {
    /// Where the fade begins: the consumer plays what it held up to here.
    cut: i64,
    /// After the fade that follows.
    faded: i64,
    /// What it had been given before.
    before: i64,
}

/// A block written: what the monitor looks at.
struct Written {
    start: Instant,
    end: Instant,
    /// Frames the consumer held before the write.
    fill: i64,
}

/// The feeder of one run.
pub(super) struct Feeder {
    inner: Arc<Inner>,
    run: u64,
    consumer: Box<dyn Consumer>,
    can_pause: bool,
    pub(super) composer: Composer,
    sent: SentRing,
    replay: Replay,
    /// Frames of the fade-in after a resume given so far (`None`: none running).
    fade_in: Option<usize>,
    pub(super) mode: Mode,
    /// Silent composed frames without a source, in a row.
    idle_frames: i64,
    monitor: Monitor,
    scratch: Vec<f32>,
    silence: Vec<f32>,
}

/// Frames given to the consumer and not taken by it yet.
fn queued(state: &State, now: Instant) -> i64 {
    state.transport.sent - state.clock.played_at(state.transport.sent, now)
}

impl Feeder {
    pub(super) fn new(inner: Arc<Inner>, run: u64, consumer: Box<dyn Consumer>, settings: dsp::Settings) -> Feeder {
        let can_pause = consumer.can_pause();
        Feeder {
            inner,
            run,
            consumer,
            can_pause,
            composer: Composer::new(settings),
            sent: SentRing { frames: vec![0.0; SENT_RING_FRAMES * OUT_CHANNELS] },
            replay: Replay::default(),
            fade_in: None,
            mode: Mode::Playing,
            idle_frames: 0,
            monitor: Monitor {
                last_write_end: None,
                waited: Duration::ZERO,
                window_start: Instant::now(),
                window_max: 0.0,
                window_fill_min: None,
                low_logged: None,
            },
            scratch: Vec::with_capacity(SENT_RING_FRAMES * OUT_CHANNELS),
            silence: vec![0.0; BLOCK_FRAMES * OUT_CHANNELS],
        }
    }

    /// Feed until the run ends; then the last fade, and the consumer is let go.
    pub(super) fn run(mut self) -> Result<(), String> {
        let mut steps = 0u64;
        let outcome = loop {
            match self.step() {
                Ok(Flow::Continue) => {}
                Ok(Flow::Wait) => {
                    if let Err(error) = self.consumer.idle(&self.inner.waker, IDLE_WAIT) {
                        break Err(error);
                    }
                }
                Ok(Flow::Exit) => break Ok(()),
                Err(error) => break Err(error),
            }
            self.report();
            steps += 1;
            if steps.is_multiple_of(10)
                && let Some(gone) = self.inner.consumer_exited()
            {
                break Err(gone);
            }
        };
        if outcome.is_ok() {
            self.finish();
        }
        self.report();
        self.consumer.stop();
        outcome
    }

    /// One thing: a block, a fade, a cork... `Wait`: nothing until a command.
    pub(super) fn step(&mut self) -> Result<Flow, String> {
        let inner = self.inner.clone();
        let decision = {
            let mut state = lock(&inner.state);
            if state.run != self.run || self.inner.stop.load(Ordering::SeqCst) {
                return Ok(Flow::Exit);
            }
            self.decide(&mut state, Instant::now())
        };
        match decision {
            Decision::Wait => Ok(Flow::Wait),
            Decision::Compose => self.compose(),
            Decision::Pause => self.pause_by_rewriting(),
            Decision::Pad => self.pad(),
            Decision::Replay => self.send_replay(),
            Decision::Cork(why) => self.cork(why),
            Decision::Uncork => self.uncork(),
        }
    }

    fn decide(&mut self, state: &mut State, now: Instant) -> Decision {
        if !self.can_pause {
            return Decision::Compose;
        }
        match self.mode {
            Mode::Corked(Corked::Paused) if state.paused => Decision::Wait,
            Mode::Corked(Corked::Idle) if state.paused || state.idle() => Decision::Wait,
            Mode::Corked(_) => Decision::Uncork,
            Mode::Fading { until, .. } if state.heard(now) < until => Decision::Pad,
            // corked even when resumed during the fade: a corked stream's position is exact
            Mode::Fading { .. } => Decision::Cork(Corked::Paused),
            Mode::Settling { .. } if !state.paused => {
                self.mode = Mode::Playing;
                Decision::Compose
            }
            Mode::Settling { until } if state.gain == 0.0 && state.heard(now) >= until => {
                Decision::Cork(Corked::Paused)
            }
            Mode::Settling { .. } => Decision::Compose,
            Mode::Playing if state.paused => {
                if queued(state, now) >= INTERRUPT_MIN_FRAMES {
                    Decision::Pause // which settles when the consumer holds too little after all
                } else {
                    self.mode = Mode::Settling { until: state.transport.sent };
                    Decision::Compose
                }
            }
            Mode::Playing if self.replay.remaining() > 0 => Decision::Replay,
            Mode::Playing if self.idle_frames >= IDLE_CORK_FRAMES && state.idle() => Decision::Cork(Corked::Idle),
            Mode::Playing => Decision::Compose,
        }
    }

    /// Compose a block and give it to the consumer. A jump (seek, new source) rewrites a
    /// consumer that can be rewritten and holds enough: what it was about to play is replaced by
    /// a fade of it, and the new audio follows at once (see [`jump`](Self::jump)).
    fn compose(&mut self) -> Result<Flow, String> {
        let jumping = |state: &State| {
            self.can_pause && state.jump_pending() && queued(state, Instant::now()) >= INTERRUPT_MIN_FRAMES
        };
        if !jumping(&lock(&self.inner.state)) {
            let begun = Instant::now();
            let room = self.consumer.wait(BLOCK_FRAMES, &self.inner.waker, WAIT)?;
            self.monitor.waited += begun.elapsed();
            if !room {
                self.publish(); // what it played meanwhile
                return Ok(Flow::Continue); // woken: look again
            }
        }
        let inner = self.inner.clone();
        let (gain_before, fill) = {
            let mut state = lock(&inner.state);
            if state.run != self.run {
                return Ok(Flow::Exit);
            }
            if self.can_pause && state.paused && self.mode == Mode::Playing {
                return Ok(Flow::Continue); // a pause arrived: it is taken from here
            }
            if jumping(&state) {
                drop(state);
                self.jump()?;
                return Ok(Flow::Continue);
            }
            let gain_before = state.gain;
            self.composer.compose(&mut state, false);
            self.count_idle(&state, gain_before);
            (gain_before, queued(&state, Instant::now()))
        };
        if let Some(done) = self.fade_in {
            self.fade_in = fade_in(&mut self.composer.stereo, done);
        }
        let start = Instant::now();
        self.consumer.write(&self.composer.stereo)?;
        let end = Instant::now();
        let at = lock(&self.inner.state).transport.sent;
        self.sent.write(at, &self.composer.stereo);
        let audible = {
            let mut state = lock(&self.inner.state);
            if state.run != self.run {
                return Ok(Flow::Exit);
            }
            state.advance(true);
            if let Mode::Settling { until } = &mut self.mode
                && (gain_before > 0.0 || state.gain > 0.0)
            {
                *until = state.transport.sent;
            }
            state.source.is_some() && state.gain > 0.0
        };
        if !self.publish() {
            return Ok(Flow::Exit);
        }
        self.monitor(Written { start, end, fill }, audible);
        Ok(Flow::Continue)
    }

    fn count_idle(&mut self, state: &State, gain_before: f64) {
        if state.idle() && gain_before == 0.0 && state.gain == 0.0 {
            self.idle_frames += BLOCK_FRAMES as i64;
        } else {
            self.idle_frames = 0;
        }
    }

    /// Where a rewrite of what the consumer holds can begin, with the fade that starts it in
    /// `scratch` (a block of what the consumer was about to play there, from full gain down);
    /// `None` when less than that block is left to replace (it plays out as it is).
    fn rewrite_point(&mut self) -> Result<Option<Cut>, String> {
        let before = lock(&self.inner.state).transport.sent;
        let earliest = before - self.sent.capacity() + BLOCK_FRAMES as i64;
        let cut = self.consumer.rewrite_from()?.max(earliest);
        if before - cut < BLOCK_FRAMES as i64 {
            return Ok(None);
        }
        self.scratch.clear();
        self.sent.read_into(cut, BLOCK_FRAMES, &mut self.scratch);
        for (index, frame) in self.scratch.as_chunks_mut::<OUT_CHANNELS>().0.iter_mut().enumerate() {
            let gain = (BLOCK_FRAMES - index) as f32 / BLOCK_FRAMES as f32; // from 1 down to 1/480
            frame.iter_mut().for_each(|sample| *sample *= gain);
        }
        Ok(Some(Cut { cut, faded: cut + BLOCK_FRAMES as i64, before }))
    }

    /// Give `scratch` (the fade and what follows it) to the consumer in place of what it held
    /// from the cut on.
    fn give(&mut self, cut: &Cut) -> Result<(), String> {
        self.consumer.rewrite(cut.cut, &self.scratch)?;
        self.sent.write(cut.cut, &self.scratch);
        Ok(())
    }

    /// A jump that the consumer hears at once: the block that fades the old position out (when
    /// it is audible) and the new audio are composed first; then what the consumer holds from
    /// the rewrite point on is replaced, in one write, by a fade of what it held there and the
    /// new audio. The composed fade-out block is never given then (the composition stays the
    /// Python one). When too little is left to replace, both follow what the consumer holds.
    fn jump(&mut self) -> Result<(), String> {
        let inner = self.inner.clone();
        let (first, frames, skip, continued) = {
            let mut state = lock(&inner.state);
            let first = state.frames_written;
            // the source the jump goes on with, before the composition below takes the jump
            let continued = state.jump_source().cloned();
            // the new audio for as long as the consumer holds the old (it replaces all of it)
            let blocks = (queued(&state, Instant::now()).max(0) as usize).div_ceil(BLOCK_FRAMES) + 1;
            let skip = usize::from(state.gain > 0.0); // the fade-out block comes first
            let mut frames = Vec::with_capacity((blocks + skip) * BLOCK_FRAMES * OUT_CHANNELS);
            for _ in 0..blocks + skip {
                let gain_before = state.gain;
                self.composer.compose(&mut state, false);
                self.count_idle(&state, gain_before);
                state.advance(false);
                frames.extend_from_slice(&self.composer.stereo);
            }
            (first, frames, skip * BLOCK_FRAMES, continued)
        };
        let count = |samples: &[f32]| (samples.len() / OUT_CHANNELS) as i64;
        match self.rewrite_point()? {
            None => {
                let at = lock(&self.inner.state).transport.sent;
                self.consumer.write(&frames)?;
                self.sent.write(at, &frames);
                lock(&self.inner.state).transport.send(first, count(&frames));
            }
            Some(cut) => {
                let new = &frames[skip * OUT_CHANNELS..];
                self.scratch.extend_from_slice(new);
                self.give(&cut)?;
                let mut state = lock(&self.inner.state);
                // the composed frames after the fade up to the new audio are never heard
                let unheard = state.transport.ours(cut.faded)..first + skip as i64;
                state.transport.cut(cut.faded);
                state.drop_unheard(continued.as_ref(), unheard);
                state.transport.send(first + skip as i64, count(new));
                state.pending_log.push(format!(
                    "output: jump: {} frames the audio server held are replaced by a fade of {} and the new audio",
                    cut.before - cut.cut,
                    BLOCK_FRAMES
                ));
                if let Mode::Settling { until } = &mut self.mode {
                    *until = (*until).min(cut.faded); // what was replaced will not be heard
                }
            }
        }
        // what a pause took back belongs to where playback was: the jump replaces it
        self.replay.clear();
        self.fade_in = None;
        self.publish();
        Ok(())
    }

    /// Pause at once: what the consumer holds from the rewrite point on is replaced by a fade
    /// of it and silence (the stream corks once the fade has been heard); the audio replaced is
    /// kept for the resume. When too little is left to replace, the composition fades out.
    fn pause_by_rewriting(&mut self) -> Result<Flow, String> {
        let Some(cut) = self.rewrite_point()? else {
            self.mode = Mode::Settling { until: lock(&self.inner.state).transport.sent };
            return Ok(Flow::Continue);
        };
        // the frames replaced after the fade, followed by what a replay had not given yet
        let mut frames = Vec::with_capacity(SENT_RING_FRAMES * OUT_CHANNELS);
        let replaced = (cut.before - cut.faded).max(0) as usize;
        self.sent.read_into(cut.faded, replaced, &mut frames);
        let unsent = (self.replay.sent * OUT_CHANNELS).min(self.replay.frames.len());
        frames.extend_from_slice(&self.replay.frames[unsent..]);
        // silence over all that was replaced (a server may keep what lies beyond a rewrite),
        // and for a quantum at least, until the fade has been heard
        let padding = replaced.max(LOW_WATER_FRAMES as usize).next_multiple_of(BLOCK_FRAMES);
        self.scratch.resize(self.scratch.len() + padding * OUT_CHANNELS, 0.0);
        self.give(&cut)?;
        let label = {
            let mut state = lock(&self.inner.state);
            let first = state.transport.ours(cut.faded);
            let mut pieces = vec![Piece { at: cut.faded, ours: first, held: false }];
            pieces.extend(state.transport.cut(cut.faded));
            state.transport.send_held(first, padding as i64);
            self.replay = Replay { frames, sent: 0, pieces };
            self.mode = Mode::Fading { until: cut.faded, ours: first };
            source_label(state.active_source().map(|source| &**source))
        };
        self.fade_in = None;
        self.log(&format!(
            "output: pause {label}: the {} frames replaced after the fade are kept for the resume",
            self.replay.remaining()
        ));
        self.publish();
        Ok(Flow::Continue)
    }

    /// While a pause fades out: keep the consumer playing silence until the fade has been heard.
    fn pad(&mut self) -> Result<Flow, String> {
        let Mode::Fading { ours, .. } = self.mode else { return Ok(Flow::Continue) };
        if !self.consumer.wait(BLOCK_FRAMES, &self.inner.waker, FADING_WAIT)? {
            self.publish();
            return Ok(Flow::Continue);
        }
        self.consumer.write(&self.silence)?;
        let at = {
            let mut state = lock(&self.inner.state);
            let at = state.transport.sent;
            state.transport.send_held(ours, BLOCK_FRAMES as i64);
            at
        };
        self.sent.write(at, &self.silence);
        self.publish();
        Ok(Flow::Continue)
    }

    /// After a resume: what the pause took back, fading in.
    fn send_replay(&mut self) -> Result<Flow, String> {
        if !self.consumer.wait(BLOCK_FRAMES, &self.inner.waker, WAIT)? {
            self.publish();
            return Ok(Flow::Continue);
        }
        let count = self.replay.remaining().min(BLOCK_FRAMES);
        let from = self.replay.sent * OUT_CHANNELS;
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.replay.frames[from..from + count * OUT_CHANNELS]);
        if let Some(done) = self.fade_in {
            self.fade_in = fade_in(&mut self.scratch, done);
        }
        self.consumer.write(&self.scratch)?;
        self.replay.sent += count;
        let at = {
            let mut state = lock(&self.inner.state);
            let at = state.transport.sent;
            state.transport.advance(count as i64);
            at
        };
        self.sent.write(at, &self.scratch);
        if self.replay.remaining() == 0 {
            self.replay.clear();
        }
        self.publish();
        Ok(Flow::Continue)
    }

    fn cork(&mut self, why: Corked) -> Result<Flow, String> {
        self.consumer.pause()?;
        self.mode = Mode::Corked(why);
        lock(&self.inner.state).corked = true;
        self.log(match why {
            Corked::Paused => "output: corked (paused)",
            Corked::Idle => "output: corked (nothing to play)",
        });
        self.publish();
        Ok(Flow::Continue)
    }

    /// Play again: what the corked consumer holds is silence (after a fade, or the silence of an
    /// idle output), flushed so that the audio is heard at once (a corked stream's position
    /// stands still, so the flush says exactly where the next frame lands); then what the pause
    /// took back.
    fn uncork(&mut self) -> Result<Flow, String> {
        let corked = matches!(self.mode, Mode::Corked(_));
        let cut = self.consumer.flush()?;
        if corked {
            self.consumer.resume()?;
        }
        {
            let mut state = lock(&self.inner.state);
            state.transport.cut(cut);
            state.corked = false;
            if self.replay.remaining() > 0 {
                if state.jump_pending() {
                    // it belongs to where playback was: the jump replaces it, after the silence
                    self.replay.clear();
                    state.gain = 0.0;
                } else {
                    let pieces = std::mem::take(&mut self.replay.pieces);
                    state.transport.place(&pieces, cut);
                    self.fade_in = Some(0);
                }
            }
        }
        self.mode = Mode::Playing;
        self.idle_frames = 0;
        self.log("output: uncorked");
        self.publish();
        Ok(Flow::Continue)
    }

    /// The end of the run (`stop()`): a last fade so that stopping never clicks, played.
    pub(super) fn finish(&mut self) {
        let (mine, gain, queued_frames) = {
            let state = lock(&self.inner.state);
            (state.run == self.run, state.gain, queued(&state, Instant::now()))
        };
        if !mine {
            return;
        }
        let result = match self.mode {
            Mode::Corked(_) => return, // silent already
            // the pause's fade is on its way and silence follows; the composition's gain is
            // the one it had before the pause
            Mode::Fading { .. } => Ok(()),
            _ if gain > 0.0 => {
                let point = if self.can_pause && queued_frames >= INTERRUPT_MIN_FRAMES {
                    self.rewrite_point()
                } else {
                    Ok(None)
                };
                match point {
                    Ok(Some(cut)) => {
                        // a fade of what was about to be heard, silence over the rest
                        let replaced = (cut.before - cut.faded).max(0) as usize;
                        let padding = replaced.max(LOW_WATER_FRAMES as usize).next_multiple_of(BLOCK_FRAMES);
                        self.scratch.resize(self.scratch.len() + padding * OUT_CHANNELS, 0.0);
                        self.give(&cut)
                    }
                    Ok(None) => self.last_block(),
                    Err(error) => Err(error),
                }
            }
            _ => Ok(()),
        };
        if result.is_ok() && (gain > 0.0 || matches!(self.mode, Mode::Fading { .. })) {
            self.consumer.drain(STOP_DRAIN);
        }
    }

    /// The block that fades the stream out at its end, given after what the consumer holds.
    fn last_block(&mut self) -> Result<(), String> {
        {
            let mut state = lock(&self.inner.state);
            self.composer.compose(&mut state, true);
        }
        let at = lock(&self.inner.state).transport.sent;
        self.consumer.write(&self.composer.stereo)?;
        self.sent.write(at, &self.composer.stereo);
        let mut state = lock(&self.inner.state);
        if state.run == self.run {
            state.advance(true);
        }
        Ok(())
    }

    /// Tell the state what the consumer says, write the lines composed under the lock and run
    /// the callbacks that are due. False when the run is over.
    pub(super) fn publish(&mut self) -> bool {
        let (due, lines) = {
            let mut state = lock(&self.inner.state);
            if state.run != self.run {
                return false;
            }
            state.clock = self.consumer.clock(state.transport.sent);
            let audible = state.audible(Instant::now());
            let due = state.due_events(audible);
            (due, std::mem::take(&mut state.pending_log))
        };
        for line in lines {
            self.inner.log.write(&line);
        }
        self.run_callbacks(due);
        true
    }

    fn run_callbacks(&self, due: Vec<QueuedEvent>) {
        for event in due {
            let callback = {
                let callbacks = lock(&self.inner.callbacks);
                match event.kind {
                    EventKind::Started => callbacks.source_started.clone(),
                    EventKind::Ended => callbacks.source_ended.clone(),
                }
            };
            if let Some(callback) = callback {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event.source)));
            }
        }
    }

    fn log(&self, line: &str) {
        self.inner.consumer_log.write(line);
        self.inner.log.write(line);
    }

    /// What the consumer said about itself, to the logs; its state for `stream_state()`.
    fn report(&mut self) {
        for (important, line) in self.consumer.take_log() {
            self.inner.consumer_log.write(&line);
            if important {
                self.inner.log.write(&line);
            }
        }
        let (state, index) = self.consumer.status();
        let mut status = lock(&self.inner.status);
        if state.is_some() {
            status.0 = state;
        }
        if index.is_some() {
            status.1 = index;
        }
    }

    /// Feeder jitter: the time between finishing one write and starting the next, without the
    /// waits for room, is ours (composition, lock waits, the feeder held off the processor).
    fn monitor(&mut self, written: Written, audible: bool) {
        let monitor = &mut self.monitor;
        let gap = monitor
            .last_write_end
            .map(|last| (written.start.duration_since(last).saturating_sub(monitor.waited)).as_secs_f64());
        monitor.last_write_end = Some(written.end);
        monitor.waited = Duration::ZERO;
        if let Some(gap) = gap
            && gap > monitor.window_max
        {
            monitor.window_max = gap;
        }
        if monitor.window_fill_min.is_none_or(|least| written.fill < least) {
            monitor.window_fill_min = Some(written.fill);
        }
        let frames_written = lock(&self.inner.state).frames_written;
        if audible && written.fill < LOW_WATER_FRAMES && frames_written > 4 * LOW_WATER_FRAMES {
            let count = self.inner.pipe_low_count.fetch_add(1, Ordering::Relaxed) + 1;
            if monitor.low_logged.is_none_or(|logged| written.end.duration_since(logged).as_secs_f64() >= 1.0) {
                monitor.low_logged = Some(written.end);
                let (label, cursor) = {
                    let state = lock(&self.inner.state);
                    (source_label(state.source.as_deref()), state.cursor)
                };
                self.inner.log.write(&format!(
                    "feeder: buffer low, {} frames left before the write (gap {:.1} ms, source {label}, cursor {cursor}, \
                     buffer-low count {count})",
                    written.fill,
                    gap.unwrap_or(0.0) * 1000.0
                ));
            }
        }
        if written.end.duration_since(monitor.window_start).as_secs_f64() >= 1.0 {
            let gap_ms = monitor.window_max * 1000.0;
            self.inner.feeder_gap_ms.store(gap_ms.to_bits(), Ordering::Relaxed);
            if gap_ms > FEEDER_GAP_WARN_MS {
                let label = source_label(lock(&self.inner.state).source.as_deref());
                self.inner.log.write(&format!(
                    "feeder: max gap {gap_ms:.1} ms between writes in the last second (buffer fill min {} frames, \
                     source {label})",
                    monitor.window_fill_min.unwrap_or(written.fill)
                ));
            }
            monitor.window_start = written.end;
            monitor.window_max = 0.0;
            monitor.window_fill_min = None;
        }
    }
}

/// The fade-in after a resume, `done` frames of it given already: the frames of `stereo` times
/// a linear ramp from 0 to 1 over a block. Returns how far it got, `None` once complete.
fn fade_in(stereo: &mut [f32], done: usize) -> Option<usize> {
    let mut done = done;
    for frame in stereo.as_chunks_mut::<OUT_CHANNELS>().0 {
        if done >= BLOCK_FRAMES {
            return None;
        }
        let gain = done as f32 / BLOCK_FRAMES as f32;
        frame.iter_mut().for_each(|sample| *sample *= gain);
        done += 1;
    }
    (done < BLOCK_FRAMES).then_some(done)
}
