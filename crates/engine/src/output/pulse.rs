//! The PulseAudio stream: an ordinary 48 kHz float32 stereo playback stream of the desktop's
//! audio server (pipewire-pulse on a PipeWire desktop), driven from the feeder thread with a
//! standard main loop of its own, so that every PulseAudio object lives and dies on that thread
//! and the server's requests are answered at the feeder's priority.
//!
//! How pipewire-pulse treats such a stream (measured on a private server, see
//! `tools/private-audio.sh`): it keeps the stream's buffer (`tlength`) in a ring that its graph
//! takes one quantum from per cycle. A cork stops at the read position and keeps the ring. A
//! stream that runs dry is reported (underflow), keeps what it had and starts again with it, so
//! that no frame written is ever skipped. A flush drops the ring and sets the write position
//! back to the read position, and the stream plays again as soon as a quantum is there; but on
//! a stream that plays, the read position it reports can be a quantum behind what was taken
//! already, and a quantum taken from the ring can be dropped unheard, so that the audio given
//! after a flush can repeat or skip a quantum, or follow a quantum of silence. Hence the
//! feeder flushes only a corked stream (nothing moves), and replaces audio while the stream
//! plays by writing at an absolute position instead ([`Consumer::rewrite`]): pipewire-pulse
//! drops what lies beyond the end of such a write, plays the old audio up to its start and the
//! new from there, in one piece. The start must be ahead of what the server has taken when the
//! write arrives: the read position of a fresh timing reply plus two quanta (the reply can be a
//! quantum old, and one more can be taken before the write arrives); the quantum is the
//! greatest common divisor of the steps of the read position between replies.
//!
//! The read position the server reports moves in whole quanta and libpulse's interpolation of
//! it drifts by tens of milliseconds between its sparse updates, so this consumer asks for the
//! timing every [`TIMING_INTERVAL_MS`] and takes, of the replies of the last two seconds of
//! uninterrupted playback, the extrapolation that is lowest: each reply's read position is at
//! or ahead of the frame being played, and the lowest one is closest to it.

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use libpulse_binding as pa;
use pa::channelmap::Map;
use pa::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pa::def::BufferAttr;
use pa::mainloop::api::Mainloop as _;
use pa::mainloop::events::io::{FlagSet as IoFlags, IoEvent};
use pa::mainloop::standard::Mainloop;
use pa::operation::{Operation, State as OperationState};
use pa::proplist::Proplist;
use pa::sample::{Format, Spec};
use pa::stream::{FlagSet as StreamFlags, SeekMode, State as StreamState, Stream};
use pa::time::MicroSeconds;

use super::consumer::{Clock, Consumer, OUT_CHANNELS, OUT_FRAME_BYTES, Waker};
use crate::config::{self, SAMPLE_RATE};

/// How often the stream's timing is asked for while it plays.
pub const TIMING_INTERVAL_MS: u64 = 50;
/// The stream asks to be given audio this many frames at a time (`minreq`, 21 ms), which is
/// also the quantum it asks the graph for.
pub const REQUEST_FRAMES: u32 = 1024;
/// Connecting to the audio server and opening the stream may take this long.
pub const CONNECT_TIMEOUT_S: f64 = 5.0;
/// A cork, flush or timing request the server has not answered within this long has failed.
pub const OPERATION_TIMEOUT_S: f64 = 1.0;
/// The timing replies the read position is estimated from (see the module comment).
const ESTIMATE_WINDOW: Duration = Duration::from_secs(2);
/// The stream's clock goes to the output's log once a minute.
const CLOCK_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// The quantum assumed until the replies tell (the largest of a PipeWire graph by default), and
/// the bounds of what they may tell.
const QUANTUM_FRAMES: (i64, i64, i64) = (2048, 256, 8192);
/// The steps of the read position the quantum is found from.
const QUANTUM_STEPS: usize = 8;
/// A cycle of the graph that took a quantum just before a cork is over after this long.
const CORK_SETTLE: Duration = Duration::from_millis(50);

type WakeEvent = IoEvent<<Mainloop as pa::mainloop::api::Mainloop>::MI>;

/// What the stream is made with.
pub(super) struct PulseSettings {
    /// The server; `None`: the one libpulse finds (`PULSE_SERVER`, the session's socket).
    pub server: Option<String>,
    /// The sink; `None`: the server's default, and the stream follows the user's choice.
    pub target: Option<String>,
    /// The buffer the server is asked to keep (`tlength`).
    pub buffer_ms: u32,
    pub application_id: String,
    pub icon_name: String,
    pub stream_name: String,
    /// Shared with the output: the server's underflow notifications.
    pub underflows: Arc<AtomicU64>,
    /// The feeder's waker, watched in the main loop.
    pub wake_fd: Option<RawFd>,
}

/// What the callbacks saw, read on the feeder thread after each turn of the main loop.
#[derive(Default)]
struct Notes {
    woken: Cell<bool>,
    /// A timing reply arrived (when).
    timing: Cell<Option<Instant>>,
    underflows: Cell<u32>,
    started: Cell<u32>,
    moved: Cell<bool>,
    suspended: Cell<bool>,
    attr: Cell<bool>,
}

/// A timing reply: the server's read position (frames) and whether it was playing.
#[derive(Debug, Clone, Copy)]
struct Reply {
    at: Instant,
    read: i64,
    playing: bool,
    /// The device's latency after the read position, in frames.
    delay: i64,
}

/// The read position from the timing replies (see the module comment).
#[derive(Default)]
struct ReadEstimate {
    /// (arrival, read position) of the replies since playback was last interrupted, oldest first.
    samples: VecDeque<(Instant, i64)>,
    latest: Option<Reply>,
    /// The latest steps of the read position between two replies while it played.
    steps: VecDeque<i64>,
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl ReadEstimate {
    fn reply(&mut self, reply: Reply) {
        let backwards = self.samples.back().is_some_and(|(_, read)| reply.read < *read);
        if let Some((_, read)) = self.samples.back()
            && reply.playing
            && reply.read > *read
        {
            self.steps.push_back(reply.read - read);
            if self.steps.len() > QUANTUM_STEPS {
                self.steps.pop_front();
            }
        }
        if !reply.playing || backwards {
            self.samples.clear();
        }
        if reply.playing {
            self.samples.push_back((reply.at, reply.read));
            while self.samples.front().is_some_and(|(at, _)| reply.at.saturating_duration_since(*at) > ESTIMATE_WINDOW)
            {
                self.samples.pop_front();
            }
        }
        self.latest = Some(reply);
    }

    /// Playback stopped or moved (cork, flush, underflow, another sink): hold where it is now,
    /// never beyond what was written.
    fn stall(&mut self, now: Instant, written: i64) {
        let held = self.clock(now).played.min(written);
        self.samples.clear();
        if let Some(latest) = &mut self.latest {
            *latest = Reply { at: now, read: held, playing: false, delay: latest.delay };
        }
    }

    /// The quantum the server takes per cycle: what all the recent steps are multiples of.
    fn quantum(&self) -> i64 {
        let (assumed, least, most) = QUANTUM_FRAMES;
        if self.steps.len() < 2 {
            return assumed;
        }
        self.steps.iter().fold(0, |divisor, step| gcd(*step, divisor)).clamp(least, most)
    }

    fn clock(&self, now: Instant) -> Clock {
        let Some(latest) = self.latest else { return Clock { played: 0, at: Some(now), running: false, delay: 0 } };
        if !latest.playing || self.samples.is_empty() {
            return Clock { played: latest.read, at: Some(latest.at), running: false, delay: latest.delay };
        }
        let rate = f64::from(SAMPLE_RATE);
        let ahead = |at: &Instant| (now.saturating_duration_since(*at).as_secs_f64() * rate) as i64;
        let played = self.samples.iter().map(|(at, read)| read + ahead(at)).min().unwrap_or(latest.read);
        Clock { played, at: Some(now), running: true, delay: latest.delay }
    }
}

/// The connection; the fields drop in this order: the wake-up watch, the stream, the context,
/// the main loop.
struct Live {
    wake: Option<WakeEvent>,
    stream: Stream,
    context: Context,
    mainloop: Mainloop,
}

/// The PulseAudio consumer (see the module comment).
pub(super) struct PulseConsumer {
    settings: PulseSettings,
    live: Option<Live>,
    notes: Rc<Notes>,
    estimate: ReadEstimate,
    corked: bool,
    corked_at: Instant,
    /// Frames written, as the server counts them (moved back by a flush or a rewrite).
    written: i64,
    /// The buffer the server granted.
    attr: BufferAttr,
    next_timing: Instant,
    bytes: Vec<u8>,
    log: Vec<(bool, String)>,
    clock_logged: Option<Instant>,
}

fn frames_of(usec: MicroSeconds) -> i64 {
    (usec.0 as f64 * f64::from(SAMPLE_RATE) / 1e6).round() as i64
}

fn bytes_of_ms(ms: u32) -> u32 {
    let frames = u64::from(ms) * u64::from(SAMPLE_RATE) / 1000;
    u32::try_from(frames * OUT_FRAME_BYTES as u64).unwrap_or(u32::MAX - 1)
}

/// The samples as the stream's float32le bytes: on a little-endian machine their own bytes, so
/// that a write right after a flush goes out without a conversion.
fn sample_bytes<'a>(frames: &'a [f32], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    if cfg!(target_endian = "little") {
        // SAFETY: an f32 is four initialized bytes without padding and u8 needs no alignment;
        // the slice covers exactly the samples and borrows them.
        unsafe { std::slice::from_raw_parts(frames.as_ptr().cast::<u8>(), std::mem::size_of_val(frames)) }
    } else {
        scratch.clear();
        scratch.extend(frames.iter().flat_map(|sample| sample.to_le_bytes()));
        scratch
    }
}

/// One turn of the main loop, waiting at most `timeout` for something to happen.
fn turn(mainloop: &mut Mainloop, timeout: Duration) -> Result<(), String> {
    let micros = u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX).min(i32::MAX as u64);
    let failed = |error: pa::error::PAErr| format!("the PulseAudio main loop failed: {error}");
    mainloop.prepare(Some(MicroSeconds(micros))).map_err(failed)?;
    mainloop.poll().map_err(failed)?;
    mainloop.dispatch().map_err(failed)?;
    Ok(())
}

fn describe(attr: &BufferAttr) -> String {
    let frames = |bytes: u32| bytes as usize / OUT_FRAME_BYTES;
    format!(
        "buffer {} frames ({} ms), prebuf {} frames, minreq {} frames",
        frames(attr.tlength),
        frames(attr.tlength) * 1000 / SAMPLE_RATE as usize,
        frames(attr.prebuf),
        frames(attr.minreq)
    )
}

impl PulseConsumer {
    pub(super) fn new(settings: PulseSettings) -> PulseConsumer {
        PulseConsumer {
            settings,
            live: None,
            notes: Rc::default(),
            estimate: ReadEstimate::default(),
            corked: false,
            corked_at: Instant::now(),
            written: 0,
            attr: BufferAttr::default(),
            next_timing: Instant::now(),
            bytes: Vec::new(),
            log: Vec::new(),
            clock_logged: None,
        }
    }

    fn proplist(&self) -> Result<Proplist, String> {
        let mut props = Proplist::new().ok_or("PulseAudio: no property list")?;
        let entries = [
            ("application.name", config::PW_APP_NAME),
            ("application.id", self.settings.application_id.as_str()),
            ("application.icon_name", self.settings.icon_name.as_str()),
            ("media.role", "music"),
            ("media.name", self.settings.stream_name.as_str()),
        ];
        for (key, value) in entries {
            props.set_str(key, value).map_err(|()| format!("PulseAudio: cannot set {key}"))?;
        }
        Ok(props)
    }

    fn live(&mut self) -> Result<&mut Live, String> {
        self.live.as_mut().ok_or_else(|| "the stream is not open".to_string())
    }

    /// The context and the stream are still there.
    fn check(&mut self) -> Result<(), String> {
        let live = self.live()?;
        if matches!(live.context.get_state(), ContextState::Failed | ContextState::Terminated) {
            return Err(format!("the audio server went away: {}", live.context.errno()));
        }
        if matches!(live.stream.get_state(), StreamState::Failed | StreamState::Terminated) {
            return Err(format!("the audio server ended the stream: {}", live.context.errno()));
        }
        Ok(())
    }

    /// Act on what the callbacks saw.
    fn attend(&mut self) -> Result<(), String> {
        self.check()?;
        let now = Instant::now();
        if let Some(at) = self.notes.timing.take() {
            let corked = self.corked;
            let live = self.live()?;
            if let Some(info) = live.stream.get_timing_info().copied()
                && info.read_index_corrupt == 0
            {
                let read = info.read_index / OUT_FRAME_BYTES as i64;
                let reply = Reply { at, read, playing: info.playing != 0 && !corked, delay: frames_of(info.sink_usec) };
                self.estimate.reply(reply);
            }
        }
        let underflows = self.notes.underflows.replace(0);
        if underflows > 0 {
            self.estimate.stall(now, self.written);
            self.next_timing = now; // where it stopped
            let total = self.settings.underflows.load(Ordering::Relaxed);
            self.log.push((true, format!("output: the audio server ran out of audio (underflow {total})")));
        }
        if self.notes.started.replace(0) > 0 {
            self.estimate.stall(now, self.written);
            self.next_timing = now;
            self.log.push((false, "output: the audio server plays the stream".to_string()));
        }
        if self.notes.moved.replace(false) {
            self.estimate.stall(now, self.written);
            let device = self.live()?.stream.get_device_name().map(|name| name.into_owned()).unwrap_or_default();
            self.log.push((true, format!("output: the stream moved to {device}")));
        }
        if self.notes.suspended.replace(false) {
            let suspended = self.live()?.stream.is_suspended().unwrap_or(false);
            let what = if suspended { "suspended" } else { "resumed" };
            self.log.push((false, format!("output: the sink was {what}")));
        }
        if self.notes.attr.replace(false)
            && let Some(attr) = self.live()?.stream.get_buffer_attr().copied()
        {
            self.log.push((true, format!("output: the audio server changed the stream's {}", describe(&attr))));
            self.attr = attr;
        }
        Ok(())
    }

    /// Ask for the timing when it is due; returns when the next request is due.
    fn ask_timing(&mut self, now: Instant) -> Instant {
        if self.corked {
            return now + Duration::from_secs(3600);
        }
        if now >= self.next_timing {
            self.next_timing = now + Duration::from_millis(TIMING_INTERVAL_MS);
            if let Some(live) = self.live.as_mut() {
                drop(live.stream.update_timing_info(None)); // the reply is seen by the latency callback
            }
        }
        self.next_timing
    }

    /// Turn the main loop until the operations are done.
    fn complete(&mut self, operations: &[&Operation<dyn FnMut(bool)>]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs_f64(OPERATION_TIMEOUT_S);
        while operations.iter().any(|operation| operation.get_state() == OperationState::Running) {
            self.check()?;
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("the audio server did not answer within {OPERATION_TIMEOUT_S} s"));
            }
            turn(&mut self.live()?.mainloop, deadline - now)?;
        }
        if operations.iter().any(|operation| operation.get_state() != OperationState::Done) {
            return Err("the audio server cancelled a request".to_string());
        }
        self.attend()
    }

    fn set_corked(&mut self, corked: bool) -> Result<(), String> {
        let live = self.live()?;
        let operation = if corked { live.stream.cork(None) } else { live.stream.uncork(None) };
        let timing = live.stream.update_timing_info(None);
        self.corked = corked;
        self.corked_at = Instant::now();
        let now = Instant::now();
        self.estimate.stall(now, self.written);
        self.next_timing = now;
        self.complete(&[&operation, &timing])
    }
}

impl Consumer for PulseConsumer {
    fn start(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs_f64(CONNECT_TIMEOUT_S);
        let late = || format!("the audio server did not answer within {CONNECT_TIMEOUT_S} s");
        let mut mainloop = Mainloop::new().ok_or("PulseAudio: no main loop")?;
        let props = self.proplist()?;
        let mut context =
            Context::new_with_proplist(&mainloop, config::PW_APP_NAME, &props).ok_or("PulseAudio: no context")?;
        context
            .connect(self.settings.server.as_deref(), ContextFlags::NOAUTOSPAWN, None)
            .map_err(|error| format!("cannot reach the audio server: {error}"))?;
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err(format!("cannot reach the audio server: {}", context.errno()));
                }
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(late());
            }
            turn(&mut mainloop, deadline - now)?;
        }
        let spec = Spec { format: Format::F32le, rate: SAMPLE_RATE, channels: OUT_CHANNELS as u8 };
        let mut map = Map::default();
        map.init_stereo();
        let mut props = self.proplist()?;
        let mut stream =
            Stream::new_with_proplist(&mut context, &self.settings.stream_name, &spec, Some(&map), &mut props)
                .ok_or_else(|| format!("the audio server refused the stream: {}", context.errno()))?;
        let notes = self.notes.clone();
        let counter = self.settings.underflows.clone();
        stream.set_underflow_callback(Some(Box::new(move || {
            notes.underflows.set(notes.underflows.get() + 1);
            counter.fetch_add(1, Ordering::Relaxed);
        })));
        let notes = self.notes.clone();
        stream.set_started_callback(Some(Box::new(move || notes.started.set(notes.started.get() + 1))));
        let notes = self.notes.clone();
        stream.set_latency_update_callback(Some(Box::new(move || notes.timing.set(Some(Instant::now())))));
        let notes = self.notes.clone();
        stream.set_moved_callback(Some(Box::new(move || notes.moved.set(true))));
        let notes = self.notes.clone();
        stream.set_suspended_callback(Some(Box::new(move || notes.suspended.set(true))));
        let notes = self.notes.clone();
        stream.set_buffer_attr_callback(Some(Box::new(move || notes.attr.set(true))));
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: bytes_of_ms(self.settings.buffer_ms),
            prebuf: u32::MAX,
            minreq: REQUEST_FRAMES * OUT_FRAME_BYTES as u32,
            fragsize: u32::MAX,
        };
        // early requests: the server asks for `minreq` at a time, and pipewire-pulse asks its
        // graph for a quantum of `minreq` (instead of most of the buffer, which the graph caps at
        // 2048 frames): what a rewrite has to stay ahead of
        let flags = StreamFlags::AUTO_TIMING_UPDATE | StreamFlags::EARLY_REQUESTS;
        stream
            .connect_playback(self.settings.target.as_deref(), Some(&attr), flags, None, None)
            .map_err(|error| format!("the audio server refused the stream: {error}"))?;
        loop {
            match stream.get_state() {
                StreamState::Ready => break,
                StreamState::Failed | StreamState::Terminated => {
                    return Err(format!("the audio server refused the stream: {}", context.errno()));
                }
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(late());
            }
            turn(&mut mainloop, deadline - now)?;
        }
        let wake = self.settings.wake_fd.and_then(|fd| {
            let notes = self.notes.clone();
            mainloop.new_io_event(
                fd,
                IoFlags::INPUT,
                Box::new(move |_, fd, _| {
                    let mut count = 0u64;
                    // SAFETY: eight bytes into a valid u64; the descriptor does not block.
                    unsafe { libc::read(fd, (&raw mut count).cast(), 8) };
                    notes.woken.set(true);
                }),
            )
        });
        let device = stream.get_device_name().map(|name| name.into_owned()).unwrap_or_default();
        let server = context.get_server().unwrap_or_default();
        let granted = stream.get_buffer_attr().copied().unwrap_or(attr);
        self.log
            .push((true, format!("output: PulseAudio stream ready on {device} ({server}): {}", describe(&granted))));
        self.attr = granted;
        self.live = Some(Live { wake, stream, context, mainloop });
        self.next_timing = Instant::now();
        Ok(())
    }

    fn wait(&mut self, frames: usize, _waker: &Waker, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.attend()?;
            if self.notes.woken.replace(false) {
                return Ok(false);
            }
            let room = self.live()?.stream.writable_size().ok_or("the stream is gone")?;
            if room >= frames * OUT_FRAME_BYTES {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let next = self.ask_timing(now).min(deadline);
            turn(&mut self.live()?.mainloop, next.saturating_duration_since(now))?;
        }
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), String> {
        let bytes = sample_bytes(frames, &mut self.bytes);
        let Some(live) = self.live.as_mut() else { return Err("the stream is not open".to_string()) };
        live.stream
            .write_copy(bytes, 0, SeekMode::Relative)
            .map_err(|error| format!("the audio server refused audio: {error}"))?;
        self.written += (frames.len() / OUT_CHANNELS) as i64;
        turn(&mut live.mainloop, Duration::ZERO) // on its way now
    }

    fn clock(&mut self, written: i64) -> Clock {
        let now = Instant::now();
        let clock = self.estimate.clock(now);
        if self.clock_logged.is_none_or(|logged| now.duration_since(logged) >= CLOCK_LOG_INTERVAL) {
            self.clock_logged = Some(now);
            let played = clock.played_at(written, now);
            self.log.push((
                false,
                format!(
                    "pulse: written {written} played {played} latency {} ms device {} ms underflows {}",
                    (written - played + clock.delay) * 1000 / i64::from(SAMPLE_RATE),
                    clock.delay * 1000 / i64::from(SAMPLE_RATE),
                    self.underflows()
                ),
            ));
        }
        clock
    }

    fn can_pause(&self) -> bool {
        true
    }

    fn pause(&mut self) -> Result<(), String> {
        self.set_corked(true)
    }

    fn resume(&mut self) -> Result<(), String> {
        self.set_corked(false)
    }

    fn flush(&mut self) -> Result<i64, String> {
        if self.corked {
            // a quantum taken just before the cork is counted once its cycle is over
            std::thread::sleep(CORK_SETTLE.saturating_sub(self.corked_at.elapsed()));
        }
        let live = self.live()?;
        let flush = live.stream.flush(None);
        let timing = live.stream.update_timing_info(None); // answered after the flush
        self.complete(&[&flush, &timing])?;
        let info = self.live()?.stream.get_timing_info().copied().ok_or("the audio server gave no timing")?;
        if info.write_index_corrupt != 0 {
            return Err("the audio server gave no write position after the flush".to_string());
        }
        self.written = info.write_index / OUT_FRAME_BYTES as i64;
        let now = Instant::now();
        self.estimate.stall(now, self.written);
        self.next_timing = now;
        Ok(self.written)
    }

    fn rewrite_from(&mut self) -> Result<i64, String> {
        let timing = self.live()?.stream.update_timing_info(None);
        self.complete(&[&timing])?;
        let info = self.live()?.stream.get_timing_info().copied().ok_or("the audio server gave no timing")?;
        if info.read_index_corrupt != 0 {
            return Err("the audio server gave no read position".to_string());
        }
        // the reply may be a quantum old, and another quantum may be taken before a write
        // arrives
        Ok(info.read_index / OUT_FRAME_BYTES as i64 + 2 * self.estimate.quantum())
    }

    fn rewrite(&mut self, at: i64, frames: &[f32]) -> Result<(), String> {
        let bytes = sample_bytes(frames, &mut self.bytes);
        let Some(live) = self.live.as_mut() else { return Err("the stream is not open".to_string()) };
        live.stream
            .write_copy(bytes, at * OUT_FRAME_BYTES as i64, SeekMode::Absolute)
            .map_err(|error| format!("the audio server refused audio: {error}"))?;
        self.written = at + (frames.len() / OUT_CHANNELS) as i64;
        turn(&mut live.mainloop, Duration::ZERO) // on its way now
    }

    fn underflows(&self) -> u64 {
        self.settings.underflows.load(Ordering::Relaxed)
    }

    fn idle(&mut self, _waker: &Waker, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.attend()?;
            let now = Instant::now();
            if self.notes.woken.replace(false) || now >= deadline {
                return Ok(());
            }
            let next = self.ask_timing(now).min(deadline);
            turn(&mut self.live()?.mainloop, next.saturating_duration_since(now))?;
        }
    }

    fn take_log(&mut self) -> Vec<(bool, String)> {
        std::mem::take(&mut self.log)
    }

    fn status(&self) -> (Option<String>, Option<u32>) {
        let Some(live) = &self.live else { return (None, None) };
        let state = match live.stream.get_state() {
            StreamState::Ready if self.corked => "corked",
            StreamState::Ready => "streaming",
            StreamState::Creating => "connecting",
            StreamState::Unconnected => "unconnected",
            StreamState::Failed => "failed",
            StreamState::Terminated => "ended",
        };
        (Some(state.to_string()), live.stream.get_index())
    }

    fn drain(&mut self, timeout: Duration) {
        if self.corked {
            return;
        }
        let Some(live) = self.live.as_mut() else { return };
        let drain = live.stream.drain(None);
        let deadline = Instant::now() + timeout;
        while drain.get_state() == OperationState::Running && self.check().is_ok() {
            let now = Instant::now();
            let Some(live) = self.live.as_mut().filter(|_| now < deadline) else { break };
            if turn(&mut live.mainloop, deadline - now).is_err() {
                break;
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut live) = self.live.take() {
            live.wake = None;
            let _ = live.stream.disconnect();
            live.context.disconnect();
            // the stream, the context and the main loop go in this order
        }
    }
}

impl Drop for PulseConsumer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(at: Instant, read: i64, playing: bool) -> Reply {
        Reply { at, read, playing, delay: 0 }
    }

    /// The read position the server reports is ahead of the frame being played by up to a
    /// quantum; the lowest extrapolation of the recent replies is closest to it.
    #[test]
    fn the_read_position_is_the_lowest_extrapolation() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut estimate = ReadEstimate::default();
        assert_eq!(estimate.clock(at(0)), Clock { played: 0, at: Some(at(0)), running: false, delay: 0 });
        // a quantum of 2048 frames taken at 0 ms; replies at 10 ms and at 50 ms (after the next
        // quantum was taken at 42.7 ms)
        estimate.reply(reply(at(10), 2048, true));
        estimate.reply(reply(at(50), 4096, true));
        let clock = estimate.clock(at(60));
        assert!(clock.running);
        // 4096 + 10 ms (480) from the second, 2048 + 50 ms (2400) from the first: the first is lower
        assert_eq!(clock.played, 2048 + 2400);
        // a reply that does not play holds the position and forgets the others
        estimate.reply(reply(at(70), 4500, false));
        assert_eq!(estimate.clock(at(90)), Clock { played: 4500, at: Some(at(70)), running: false, delay: 0 });
        estimate.reply(reply(at(100), 6000, true));
        assert_eq!(estimate.clock(at(110)).played, 6000 + 480);
        // a stall holds the estimate, never beyond what was written
        estimate.stall(at(120), 6200);
        assert_eq!(estimate.clock(at(200)), Clock { played: 6200, at: Some(at(120)), running: false, delay: 0 });
        // replies older than the window are left out
        let mut estimate = ReadEstimate::default();
        estimate.reply(reply(at(0), 0, true));
        estimate.reply(reply(at(3000), 144_000 + 1000, true));
        assert_eq!(estimate.samples.len(), 1);
    }

    /// The quantum is what the steps of the read position between replies are multiples of.
    #[test]
    fn the_quantum_is_found_from_the_steps() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut estimate = ReadEstimate::default();
        assert_eq!(estimate.quantum(), 2048, "assumed until the replies tell");
        estimate.reply(reply(at(0), 0, true));
        estimate.reply(reply(at(50), 9 * 256, true));
        assert_eq!(estimate.quantum(), 2048, "one step says nothing yet");
        estimate.reply(reply(at(100), 19 * 256, true));
        estimate.reply(reply(at(150), 28 * 256, true));
        assert_eq!(estimate.quantum(), 256);
        // a reply while it does not play is no step; the bounds hold
        estimate.reply(reply(at(200), 28 * 256 + 64, false));
        estimate.reply(reply(at(250), 28 * 256 + 128, true));
        estimate.reply(reply(at(300), 28 * 256 + 192, true));
        assert_eq!(estimate.quantum(), 256, "at least 256 frames: {:?}", estimate.steps);
        assert_eq!(sample_bytes(&[1.0, -2.0], &mut Vec::new()), [0, 0, 128, 63, 0, 0, 0, 192]);
    }

    #[test]
    fn buffer_sizes_are_frames_of_stereo_float() {
        assert_eq!(bytes_of_ms(200), 9600 * 8);
        assert_eq!(frames_of(MicroSeconds(20_000)), 960);
        let attr = BufferAttr { maxlength: 0, tlength: 76_800, prebuf: 69_128, minreq: 7680, fragsize: 0 };
        assert_eq!(describe(&attr), "buffer 9600 frames (200 ms), prebuf 8641 frames, minreq 960 frames");
    }
}
