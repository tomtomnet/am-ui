//! The audio output: 16-channel blocks composed as before, downmixed to stereo in the engine
//! ([`dsp::Chain`]) and handed to the desktop's audio server as an ordinary stereo stream of
//! its own.
//!
//! A feeder thread composes 10 ms blocks of 16 channels (480 frames, FL FR FC LFE SL SR RL RR
//! FLW FRW TFL TFR TSL TSR TRL TRR) from the active source, exactly as the Python feeder did,
//! turns each into stereo with the downmix and equalizer chain (the arithmetic of the PipeWire
//! filter chain the "Atmos 9.1.6 Downmix" sink ran) and gives the stereo block to a
//! [`Consumer`]: by default a PulseAudio playback stream (48 kHz float32 stereo,
//! `application.name` = [`config::PW_APP_NAME`], `media.role` = music) of whatever server the
//! session has, pipewire-pulse on a PipeWire desktop; for tests and diagnostics the stdin of a
//! program ([`OutputOptions::command`]) or an injected pipe ([`OutputOptions::consumer_fd`]),
//! which get the same stereo stream. No sink of our own, no PipeWire configuration. The stream
//! stays open across tracks (gapless). Software gain with short ramps (10 ms) on volume changes,
//! seeks and pause/resume avoids clicks; a per-track Sound Check trim ramps over 50 ms. Position
//! is derived from what the consumer says it has played, mapped back to source frames through a
//! segment table, so track switches and seeks report correct positions.
//!
//! Design:
//!
//! * Pacing comes from the consumer. The feeder waits until the consumer has room for a block
//!   (the PulseAudio stream asks for data as its server plays; a pipe's write blocks), then
//!   composes it, as late as possible, so that commands apply to the next block. Nothing in
//!   here sleeps on a timer. The audio server keeps [`BUFFER_MS`] of audio
//!   ([`OutputOptions::buffer_ms`], the stream's `tlength`): the cushion against the feeder
//!   being held off the processor. Measured on a private PipeWire server (with the stream asking
//!   for 1024 frames at a time, which pipewire-pulse also asks its graph for as the quantum): a
//!   200 ms buffer plays through a feeder stalled for 175 ms (100 ms: 75 ms, 150 ms: 125 ms,
//!   250 ms: 225 ms), the latency it reports settles at 195..218 ms (median 205), a minute beside
//!   twice as many busy loops as there are processors has no underflow, and a stall of a second
//!   is one underflow after which playback goes on with nothing skipped. Pause and seeks do not
//!   wait for the buffer (see below), volume and `set_dsp()` changes do. The feeder thread asks
//!   for real-time scheduling once per start ([`FeederPriority`], see `output/priority.rs`) and
//!   logs what it got.
//! * Audible position: the consumer reports its read position ([`Clock`]; the PulseAudio stream
//!   from the server's timing replies, a pipe from what is left in it), which moves on in real
//!   time while it plays, less the device latency the server reports. A table of pieces maps
//!   the frames given to the consumer back to the frames the feeder composed (the identity
//!   until the first rewrite, see below), and the segment table maps those to source frames: the
//!   feeder extends it whenever the relation between stream and source frames changes (new
//!   source, seek, pause/resume, starvation). Monotonic within a segment by construction and by
//!   a clamp per set_source/seek generation. Stream frames (`frames_written()`,
//!   `audible_stream_frame()`, the events) count composed frames.
//! * Starvation (a render that has not reached the cursor yet) plays one 10 ms block of reserve
//!   audio faded out, then silence with `buffering()` true, and resumes with a fade-in only once
//!   [`RESUME_SECONDS`] of audio exist beyond the cursor (hysteresis: a render head hovering
//!   near the cursor must not gate the sound on and off). Every episode counts in `underruns()`
//!   and is logged. When the audio server itself runs out of audio (the feeder was late), that
//!   is an underflow, counted apart in `underflows()`.
//! * A 2-channel source lands in FL/FR of a zeroed 16-channel block, which the chain passes on
//!   with the front gain and the preamp, as the sink's matrix did: a stereo song sounds as it
//!   did. Samples are passed through unclipped (float).
//! * Gapless: `set_next_source()` hands the feeder the following track; when the current source
//!   is exhausted (done and cursor == total frames) the very same block continues with frame 0
//!   of the next source; `handoffs()` counts them.
//! * Readers keep their file: a source file that is replaced or shrunk under us (cache eviction
//!   race) is still read from the inode we opened, never at a wrong offset of a new file; the
//!   file's size caps what we read and the event is logged.
//! * The downmix and the equalizer: `set_dsp()` takes new [`dsp::Settings`] from any thread;
//!   the feeder hands them to its chain before the next block and the chain cross-fades over
//!   20 ms. `start()` begins with a new chain (no fade, empty filters).
//! * Diagnostics: the playback log (`<log dir>/playback.log`, 0600, rotated at 1 MiB) gets
//!   commands, handoffs, starvation start/end, underflows, corks and rewrites, feeder gaps (>
//!   [`FEEDER_GAP_WARN_MS`] between two writes, the waits for room left out: the feeder itself
//!   was late) and "buffer low" events (the consumer held less than [`LOW_WATER_FRAMES`] when
//!   the feeder got to write). The output's own log (`output.log`) gets what the consumer says
//!   about itself: the stream's buffer, the program's stderr, the stream's clock once a minute.
//!   A ring buffer keeps the last [`RING_SECONDS`] of the composed stereo stream (float32,
//!   ~11.5 MB); `dump_recent()` writes it as a WAV.
//!
//! Commands that act at once, with a consumer that can be paused, rewritten and flushed (the
//! PulseAudio stream; a pipe cannot, and keeps the Python behaviour: silence while paused, a
//! jump after the queued audio). The server's buffer is not dropped while it plays (a flush of a
//! playing pipewire-pulse stream can repeat, skip or delay a quantum, which clicks): what it
//! holds from a little ahead of its read position on is replaced instead, in one write
//! ([`Consumer::rewrite`]), by a 10 ms fade of what it was about to play there (the frames it
//! was given are kept in a small ring of their own) and what follows. The old audio plays up to
//! the fade without a seam; how far ahead is two quanta of the server's graph:
//!
//! * Seek and a new source: the feeder composes the block that fades the old position out as
//!   before, and the new audio; the block is not given, the fade and the new audio replace what
//!   the stream held. Measured: the new position heard 29 ms (median of 20) after `seek()` when
//!   the graph runs 256-frame quanta, 68 ms when the stream plays alone (1024 frames), every
//!   fade right after the old audio.
//! * Pause: the fade and silence replace what the stream held; it corks once the fade has been
//!   heard. What was replaced after the fade is kept; resume flushes the silence of the corked
//!   stream (nothing moves, so the flush is exact), uncorks and gives the kept audio again with
//!   a 10 ms fade-in before the feeder composes on: no frame is lost or played twice, and the
//!   composition never sees the pause. Measured: silent 22 ms (256-frame quanta) or 69 ms (1024)
//!   after `pause()`, heard again 6 ms after `resume()`. (When the server holds too little to
//!   replace, the composition fades out and writes silence as before, and the stream corks once
//!   that has been heard; resume fades in from there.)
//! * An output without anything to play (no source, or one that ended) for [`IDLE_CORK_S`]
//!   corks too instead of writing silence for ever. A new source or a seek uncorks it, after a
//!   flush of the silence it holds.
//! * `stop()` replaces what the stream holds by a fade the same way, lets it play and closes the
//!   stream.
//! * Positions across corks, flushes and rewrites: a corked stream's read position stands still,
//!   so do `position_frames()` and `audible_stream_frame()`. A flush or a rewrite moves the
//!   stream's write position back; the piece table then maps the frames given after it (the
//!   fade, the silence, the new audio, the audio given again) to the composed frames they carry,
//!   so stream frames, segments and events keep their numbers. Events that pointed into frames
//!   a jump replaced: an `ended` event of the source that a seek or a new start of the same
//!   source continues is dropped (its end was never heard and it plays on); every other one
//!   fires, in order, as soon as the audio that follows the fade is heard (a gapless handoff in
//!   the replaced audio is reported when the new audio starts). A pause takes nothing back for
//!   good: its events fire when the audio given again is heard. A flush drops only silence.
//!
//! `set_source()` also clears the pending next source (the player re-arms it after a mode
//! switch).
//!
//! Differences from `amui/output.py`:
//!
//! * No pw-cat and no 16-channel sink: the downmix and the equalizer run here, the consumer
//!   gets stereo (the pipe of `command`/`consumer_fd` too), the server is asked for a buffer and
//!   reports the latency (instead of pw-cat's clock lines on stderr), pause and jumps rewrite
//!   and cork as described above, the feeder asks for real-time scheduling, an idle output corks.
//!   `target` is empty by default (the server's default sink); the output's own log is
//!   `output.log` (was `pw-cat.log`). The pipe is 64 KiB (8192 stereo frames, the same 170 ms
//!   as the 512 KiB of 16 channels) and [`BLOCK_BYTES`] is a stereo block.
//! * There is no module-level `LOG`/`log()`: the caller creates the [`PlaybackLog`]
//!   (`PlaybackLog::for_config()`), shares it with the player and hands it to
//!   `PipeWireOutput::new(config, log, options)`; `output.log()` returns it.
//! * The constructor's keyword arguments are the fields of [`OutputOptions`], same names and
//!   defaults where they still exist. `consumer_fd` is an owned descriptor that one `start()`
//!   uses up; starting such an output a second time fails (Python would write to a closed
//!   descriptor number).
//! * Python's read-only properties are methods of the same name (`alive()`, `handoffs()`...).
//!   The three callbacks are set with `on_source_started()`, `on_source_ended()` and
//!   `on_error()`; they run on the feeder thread, as in Python, and a panic inside one is
//!   contained the way Python swallows an exception.
//! * Sources are `Arc<dyn PcmSource>` compared by identity. The output keeps a source alive only
//!   while it is active, pending, armed as the next one or named by a queued event; the segment
//!   table and the trim table (Python: up to 32 sources kept) remember identities only.
//! * A source cannot raise: a panic inside one of its calls is treated as Python treats an
//!   exception (the source counts as finished at the cursor). A panic of the feeder itself ends
//!   the output the way a dead consumer does (`on_error`, `alive()` false); in Python the thread
//!   would die unnoticed.
//! * `start()` and `stop()` are serialized against each other, and dropping the output stops it.
//!   A feeder that `stop()` had to give up on (stuck in a write or in a callback) belongs to its
//!   own run: when it wakes after a new `start()` it neither touches the new stream nor reports
//!   its broken pipe as the new run's failure (in Python it would do both).
//! * Frame counts are `u64` in the interface (a negative start frame cannot be asked for).
//!   `dump_recent()` returns an `io::Result`. `set_trim()` always takes the ramp
//!   ([`TRIM_RAMP_S`] is Python's default).
//! * The feeder allocates nothing per block: blocks, byte buffers and read buffers are reused
//!   (Python builds arrays for every block).
//! * Small things: the feeder's one-second window and the limit of one "buffer low" line a
//!   second begin anew with every `start()`; frame numbers beyond 2^40 are taken as 2^40.
//!
//! * A song plays on at volume 0, in silence. The Python feeder stopped reading a source whose
//!   gain was 0, which held the song where it was until the volume came back.
//!
//! Apart from that, what a block of 16 channels holds is the Python algorithm unchanged: for a
//! fixed script of commands the feeder composes the same samples bit for bit, reports the same
//! positions and events and logs the same lines as `amui/output.py`
//! (`tests/reference/output_trace.py` prints what Python does; the test
//! `follows_the_python_feeder_block_by_block` compares the composition before the chain).

mod consumer;
mod feeder;
mod log;
mod priority;
mod pulse;
mod state;

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::os::fd::{AsRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use regex::Regex;

pub use consumer::{Clock, Consumer, ConsumerFactory, OUT_CHANNELS, OUT_FRAME_BYTES, Waker};
pub use log::PlaybackLog;
pub use priority::{
    FEEDER_NICE, FEEDER_RT_PRIORITY, FeederPriority, Grant, KeepPriority, RealtimeKitLimits, SchedulerCalls,
    SystemPriority, raise_with,
};
pub use pulse::{CONNECT_TIMEOUT_S, OPERATION_TIMEOUT_S, REQUEST_FRAMES, TIMING_INTERVAL_MS};

use consumer::{PipeConsumer, set_pipe_size};
use feeder::Feeder;
use log::ConsumerLog;
use pulse::{PulseConsumer, PulseSettings};
use state::{EventKind, Identity, Source, State, Trim, same_source, unit_gain};

use crate::config::{self, Config, SAMPLE_RATE};
use crate::downmix;
use crate::dsp;
use crate::loudness::{exit_code, lock, os_error_text, poll_until, write_wav_float32};
use crate::render::PcmSource;
use crate::util::Event;

/// 10 ms at 48 kHz: one block, one gain ramp.
pub const BLOCK_FRAMES: usize = 480;
/// One block as the consumer gets it (stereo float32).
pub const BLOCK_BYTES: usize = BLOCK_FRAMES * OUT_FRAME_BYTES;
/// Requested capacity of a consumer program's stdin pipe: 8192 stereo frames, ~170 ms.
pub const PIPE_BYTES: usize = 64 * 1024;
/// Kept unplayed behind a growing file: the fade-out block when it starves.
pub const RESERVE_FRAMES: usize = BLOCK_FRAMES;
/// After starving, resume only with this much audio beyond the cursor.
pub const RESUME_SECONDS: f64 = 0.5;
pub const RESUME_FRAMES: usize = (RESUME_SECONDS * SAMPLE_RATE as f64) as usize;
/// Sound Check trim ramps over 50 ms when a source starts or Sound Check is toggled.
pub const TRIM_RAMP_BLOCKS: usize = 5;
/// 0.05 s: the ramp `set_trim()` is normally given.
pub const TRIM_RAMP_S: f64 = (TRIM_RAMP_BLOCKS * BLOCK_FRAMES) as f64 / SAMPLE_RATE as f64;
/// Recent-audio ring buffer (the composed stereo stream).
pub const RING_SECONDS: f64 = 30.0;
pub const RING_FRAMES: usize = (RING_SECONDS * SAMPLE_RATE as f64) as usize;
/// Feeder-side latency between two block writes worth a log line.
pub const FEEDER_GAP_WARN_MS: f64 = 40.0;
/// A consumer program must survive this long or `start()` fails.
pub const START_GRACE_S: f64 = 0.5;
pub const STOP_GRACE_S: f64 = 2.0;
pub const LOG_MAX_BYTES: u64 = 1024 * 1024;
pub const MAX_SEGMENT_AGE_FRAMES: u64 = 10 * SAMPLE_RATE as u64;
/// The audio server is asked to keep this much audio for the stream (see the module comment).
pub const BUFFER_MS: u32 = 200;
/// The buffer that can be asked for, at least and at most.
pub const BUFFER_MS_RANGE: (u32, u32) = (20, 1000);
/// With nothing to play for this long, the stream is corked.
pub const IDLE_CORK_S: f64 = 2.0;
pub const IDLE_CORK_FRAMES: i64 = (IDLE_CORK_S * SAMPLE_RATE as f64) as i64;
/// Less than this in the consumer when the feeder writes is "buffer low" (a quantum of the
/// PipeWire graph at most).
pub const LOW_WATER_FRAMES: i64 = 2048;
/// A pause or a jump asks to rewrite only a consumer that holds at least this much (what the
/// server may take before a write arrives, and a block for the fade); the consumer has the last
/// word ([`Consumer::rewrite_from`]).
pub const INTERRUPT_MIN_FRAMES: i64 = LOW_WATER_FRAMES + 2 * BLOCK_FRAMES as i64;
/// The frames given to the consumer that are kept for fades and resumes (more than the largest
/// buffer).
pub const SENT_RING_FRAMES: usize = 2 * SAMPLE_RATE as usize;
/// The application id and icon name the stream carries by default.
pub const APPLICATION_ID: &str = "dev.amui.Music";
/// The stream's name (`media.name`) by default.
pub const STREAM_NAME: &str = "Music";
/// Frame numbers beyond this (eight months of audio) are taken for this: the feeder's sums stay
/// far from the ends of an `i64`.
const MAX_FRAME: u64 = 1 << 40;

static TROUBLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)xrun|underrun|error").expect("a valid pattern"));

/// `start()` could not bring the output up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    /// The consumer program could not be started.
    #[error("could not start {program}: {reason}")]
    Start { program: String, reason: String },
    /// It exited within the start grace; `detail` is the last line it wrote to stderr.
    #[error("{program} exited with status {status}{}", after_colon(.detail))]
    Exited { program: String, status: i32, detail: String },
    /// The audio server could not be reached or refused the stream.
    #[error("could not open the audio stream: {0}")]
    Server(String),
    /// The output itself could not start (no feeder thread, an injected descriptor already
    /// used).
    #[error("{0}")]
    Other(String),
}

fn after_colon(detail: &str) -> String {
    if detail.is_empty() { String::new() } else { format!(": {detail}") }
}

/// "1234567890.atmos" style name for log lines: the PCM cache directory of the source's file
/// (the file's own name when it has no directory), "none" without a source.
pub fn source_label(source: Option<&dyn PcmSource>) -> String {
    let Some(source) = source else { return "none".to_string() };
    let name = |path: &Path| match path.components().next_back() {
        Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
        Some(Component::ParentDir) => "..".to_string(),
        _ => String::new(),
    };
    let path = source.pcm_path();
    let folder = path.parent().map(name).unwrap_or_default();
    if folder.is_empty() { name(path) } else { folder }
}

// --- the output ------------------------------------------------------------------------------

type SourceCallback = Arc<dyn Fn(Source) + Send + Sync>;
type ErrorCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct Callbacks {
    source_started: Option<SourceCallback>,
    source_ended: Option<SourceCallback>,
    error: Option<ErrorCallback>,
}

/// A thread that says when it has ended, so that it can be waited for with a limit.
struct Worker {
    handle: JoinHandle<()>,
    finished: Arc<Event>,
}

struct SetOnDrop(Arc<Event>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

impl Worker {
    fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<Worker> {
        let finished = Arc::new(Event::new());
        let signal = SetOnDrop(finished.clone());
        let handle = thread::Builder::new().name(name.to_string()).spawn(move || {
            let _signal = signal;
            body();
        })?;
        Ok(Worker { handle, finished })
    }

    /// Wait up to `timeout` for the thread to end; true when it has. Never waits for itself.
    fn wait(&self, timeout: Duration) -> bool {
        self.handle.thread().id() != thread::current().id() && self.finished.wait(Some(timeout))
    }

    /// Wait up to `timeout`; a thread that is still running is left to end on its own.
    fn join(self, timeout: Duration) {
        if self.wait(timeout) {
            let _ = self.handle.join();
        }
    }
}

/// What `start()` and `stop()` own.
struct Lifecycle {
    /// The injected write end, until a start takes it.
    consumer_fd: Option<OwnedFd>,
    /// An injected descriptor replaces the consumer process.
    injected: bool,
    feeder: Option<Worker>,
    stderr_reader: Option<Worker>,
}

/// What the feeder of a run makes its consumer from.
enum Plan {
    Pipe(PipeConsumer),
    Pulse(PulseSettings),
    Custom(ConsumerFactory),
}

struct Inner {
    target: String,
    latency_ms: u32,
    command: Option<Vec<String>>,
    pipe_bytes_requested: usize,
    buffer_ms: u32,
    server: Option<String>,
    application_id: String,
    icon_name: String,
    stream_name: String,
    priority: Arc<dyn FeederPriority>,
    consumer_factory: Option<ConsumerFactory>,
    log: Arc<PlaybackLog>,
    consumer_log: ConsumerLog,

    state: Mutex<State>,
    lifecycle: Mutex<Lifecycle>,
    child: Mutex<Option<Child>>,
    callbacks: Mutex<Callbacks>,
    stop: AtomicBool,
    starting: AtomicBool,
    waker: Waker,

    /// What the kernel granted a consumer program's pipe.
    pipe_bytes: AtomicUsize,
    /// The audio server's underflow notifications since the output was created.
    underflows: Arc<AtomicU64>,
    /// The consumer's state and index on the server, as it last said.
    status: Mutex<(Option<String>, Option<u32>)>,
    last_stderr: Mutex<String>,
    grant: Mutex<Option<Grant>>,

    // feeder jitter monitor
    /// Largest feeder-side gap in the last completed second, as f64 bits.
    feeder_gap_ms: AtomicU64,
    pipe_low_count: AtomicU64,
}

impl Inner {
    /// Frames assumed between being taken by a consumer and being heard, until it says.
    fn fallback_delay(&self) -> i64 {
        i64::from(self.latency_ms) * i64::from(SAMPLE_RATE) / 1000
    }

    /// The return code of the consumer process once it has exited.
    fn child_exit(&self) -> Option<i32> {
        lock(&self.child).as_mut().and_then(exit_code)
    }

    /// "<program> exited with status N" once the consumer process has.
    fn consumer_exited(&self) -> Option<String> {
        let code = self.child_exit()?;
        let program = self.command.as_ref().and_then(|command| command.first()).cloned().unwrap_or_default();
        Some(format!("{program} exited with status {code}"))
    }

    fn signal_child(&self, signal: libc::c_int) {
        if let Some(child) = lock(&self.child).as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            // SAFETY: the process has not been waited for, so the id is still its own.
            unsafe { libc::kill(child.id() as libc::pid_t, signal) };
        }
    }

    fn fail(&self, run: u64, message: &str) {
        {
            let mut state = lock(&self.state);
            if state.run != run || state.error.is_some() {
                return;
            }
            state.error = Some(message.to_string());
            state.alive = false;
        }
        self.consumer_log.write(&format!("output: {message}"));
        self.log.write(&format!("output: {message}"));
        // during start() the caller gets the error instead
        if !self.starting.load(Ordering::SeqCst) {
            let callback = lock(&self.callbacks).error.clone();
            if let Some(callback) = callback {
                let _ = catch_unwind(AssertUnwindSafe(|| callback(message)));
            }
        }
    }

    /// A new stream for a new run; returns the run's number.
    fn begin_run(&self) -> u64 {
        let mut state = lock(&self.state);
        state.run += 1;
        state.alive = true;
        state.begin_stream(self.fallback_delay());
        *lock(&self.status) = (None, None);
        state.run
    }

    // ------------------------------------------------------------------ feeder
    fn run_feeder(self: &Arc<Self>, plan: Plan, run: u64, ready: SyncSender<Result<(), OutputError>>) {
        let grant = self.priority.raise();
        let line = format!("output: feeder priority: {grant}");
        *lock(&self.grant) = Some(grant);
        self.consumer_log.write(&line);
        self.log.write(&line);
        let pipe = matches!(plan, Plan::Pipe(_));
        let mut consumer: Box<dyn Consumer> = match plan {
            Plan::Pipe(pipe) => Box::new(pipe),
            Plan::Pulse(settings) => Box::new(PulseConsumer::new(settings)),
            Plan::Custom(factory) => factory(),
        };
        let started = consumer.start();
        for (important, line) in consumer.take_log() {
            self.consumer_log.write(&line);
            if important {
                self.log.write(&line);
            }
        }
        if let Err(error) = started {
            let _ = ready.send(Err(OutputError::Server(error)));
            return;
        }
        // start() begins with a new chain
        let settings = {
            let mut state = lock(&self.state);
            state.dsp_pending = None;
            state.dsp.clone()
        };
        let _ = ready.send(Ok(()));
        let feeder = Feeder::new(self.clone(), run, consumer, settings);
        // a feeder that panics ends the output like a consumer that went away
        let outcome = catch_unwind(AssertUnwindSafe(|| feeder.run()))
            .unwrap_or_else(|_| Err("the feeder stopped unexpectedly".to_string()));
        if let Err(error) = outcome
            && !self.stop.load(Ordering::SeqCst)
        {
            let detail = lock(&self.last_stderr).clone();
            let what = if pipe { "output pipe closed" } else { "audio stream ended" };
            self.fail(run, &format!("{what} ({error}){}", after_colon(&detail)));
        }
    }

    // ------------------------------------------------------------------ consumer stderr
    fn read_stderr(&self, stderr: ChildStderr) {
        let mut lines = BufReader::new(stderr);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match lines.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim_end();
            if line.is_empty() {
                continue;
            }
            *lock(&self.last_stderr) = line.to_string();
            if TROUBLE_RE.is_match(line) {
                self.log.write(&format!("consumer: {line}"));
            }
            self.consumer_log.write(line);
        }
    }

    // ------------------------------------------------------------------ lifecycle
    /// See the consumer process out (its stdin closes with the feeder); returns its return code.
    fn shutdown_process(&self, lifecycle: &mut Lifecycle) -> Option<i32> {
        if lock(&self.child).is_none() {
            return None;
        }
        let mut code = poll_until(Duration::from_secs_f64(STOP_GRACE_S), || self.child_exit());
        if code.is_none() {
            self.signal_child(libc::SIGTERM);
            code = poll_until(Duration::from_secs(1), || self.child_exit());
        }
        if code.is_none() {
            code = lock(&self.child).as_mut().map(|child| {
                let _ = child.kill();
                child.wait().map(crate::loudness::return_code).unwrap_or(0)
            });
        }
        if let Some(reader) = lifecycle.stderr_reader.take() {
            reader.join(Duration::from_secs(1));
        }
        let code = code.unwrap_or(0);
        self.consumer_log.write(&format!("output: the consumer exited with status {code}"));
        *lock(&self.child) = None;
        Some(code)
    }
}

/// The settings of the PEQdB downmix at 0 dB without an equalizer: the chain an output starts
/// with unless told otherwise.
pub fn default_dsp() -> dsp::Settings {
    let params = downmix::resolve("peqdb", Some(0.0)).expect("the PEQdB profile is built in");
    dsp::Settings::new(&params, 0.0, &[])
}

/// How `PipeWireOutput::new()` builds the output; `Default` is the engine's output.
pub struct OutputOptions {
    /// The sink to play into; empty: the audio server's default, and the stream follows the
    /// user's choice of output.
    pub target: String,
    /// The delay assumed after a pipe (its reader plays what it read this much later), and
    /// before a consumer has said anything.
    pub latency_ms: u32,
    /// 0..1 linear software gain.
    pub volume: f64,
    /// The downmix and the equalizer the output starts with.
    pub dsp: dsp::Settings,
    /// The buffer the audio server is asked to keep (within [`BUFFER_MS_RANGE`]).
    pub buffer_ms: u32,
    /// The PulseAudio server (libpulse's spelling); `None`: the one libpulse finds.
    pub server: Option<String>,
    /// `application.id` of the stream.
    pub application_id: String,
    /// `application.icon_name` of the stream.
    pub icon_name: String,
    /// `media.name` of the stream.
    pub stream_name: String,
    /// How the feeder thread asks for its scheduling (tests: [`KeepPriority`]).
    pub priority: Arc<dyn FeederPriority>,
    /// Tests and diagnostics: a program run instead of the audio server's stream; the stereo
    /// stream goes to its stdin.
    pub command: Option<Vec<String>>,
    /// Tests: the write end of a pipe that is fed instead of a consumer process (closed by
    /// `stop()`).
    pub consumer_fd: Option<OwnedFd>,
    /// Tests: makes the consumer of each run instead (on the feeder thread).
    pub consumer: Option<ConsumerFactory>,
    /// Where the output's own lines go; `None`: `output.log` in the log folder.
    pub log_path: Option<PathBuf>,
    /// Requested capacity of a consumer program's stdin pipe.
    pub pipe_bytes: usize,
}

impl Default for OutputOptions {
    fn default() -> OutputOptions {
        OutputOptions {
            target: String::new(),
            latency_ms: config::OUTPUT_LATENCY_MS,
            volume: 1.0,
            dsp: default_dsp(),
            buffer_ms: BUFFER_MS,
            server: None,
            application_id: APPLICATION_ID.to_string(),
            icon_name: APPLICATION_ID.to_string(),
            stream_name: STREAM_NAME.to_string(),
            priority: Arc::new(SystemPriority::default()),
            command: None,
            consumer_fd: None,
            consumer: None,
            log_path: None,
            pipe_bytes: PIPE_BYTES,
        }
    }
}

/// The engine's audio output: see the module documentation.
pub struct PipeWireOutput {
    inner: Arc<Inner>,
}

impl PipeWireOutput {
    /// An output that is not running yet. `log` is the playback log the output's commands and
    /// events go to.
    pub fn new(config: &Config, log: Arc<PlaybackLog>, options: OutputOptions) -> PipeWireOutput {
        let injected = options.consumer_fd.is_some();
        let fallback_delay = i64::from(options.latency_ms) * i64::from(SAMPLE_RATE) / 1000;
        let (least, most) = BUFFER_MS_RANGE;
        PipeWireOutput {
            inner: Arc::new(Inner {
                target: options.target,
                latency_ms: options.latency_ms,
                command: options.command,
                pipe_bytes_requested: options.pipe_bytes,
                buffer_ms: options.buffer_ms.clamp(least, most),
                server: options.server,
                application_id: options.application_id,
                icon_name: options.icon_name,
                stream_name: options.stream_name,
                priority: options.priority,
                consumer_factory: options.consumer,
                log,
                consumer_log: ConsumerLog::new(options.log_path.unwrap_or_else(|| config.log_dir.join("output.log"))),
                state: Mutex::new(State::new(options.volume, options.dsp, fallback_delay)),
                lifecycle: Mutex::new(Lifecycle {
                    consumer_fd: options.consumer_fd,
                    injected,
                    feeder: None,
                    stderr_reader: None,
                }),
                child: Mutex::new(None),
                callbacks: Mutex::new(Callbacks::default()),
                stop: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                waker: Waker::new(),
                pipe_bytes: AtomicUsize::new(options.pipe_bytes),
                underflows: Arc::new(AtomicU64::new(0)),
                status: Mutex::new((None, None)),
                last_stderr: Mutex::new(String::new()),
                grant: Mutex::new(None),
                feeder_gap_ms: AtomicU64::new(0f64.to_bits()),
                pipe_low_count: AtomicU64::new(0),
            }),
        }
    }

    /// The sink the stream plays into; empty: the server's default.
    pub fn target(&self) -> &str {
        &self.inner.target
    }

    /// The delay assumed after a pipe consumer.
    pub fn latency_ms(&self) -> u32 {
        self.inner.latency_ms
    }

    /// The buffer the audio server is asked to keep.
    pub fn buffer_ms(&self) -> u32 {
        self.inner.buffer_ms
    }

    /// The playback log this output writes to.
    pub fn log(&self) -> &Arc<PlaybackLog> {
        &self.inner.log
    }

    /// Called with a source when its first frame is audible. Runs on the feeder thread: return
    /// quickly and do not call `stop()` from it.
    pub fn on_source_started(&self, callback: impl Fn(Arc<dyn PcmSource>) + Send + Sync + 'static) {
        lock(&self.inner.callbacks).source_started = Some(Arc::new(callback));
    }

    /// Called with a source when its last frame has been audible (feeder thread).
    pub fn on_source_ended(&self, callback: impl Fn(Arc<dyn PcmSource>) + Send + Sync + 'static) {
        lock(&self.inner.callbacks).source_ended = Some(Arc::new(callback));
    }

    /// Called with the message when the output dies after `start()` returned (feeder thread).
    pub fn on_error(&self, callback: impl Fn(&str) + Send + Sync + 'static) {
        lock(&self.inner.callbacks).error = Some(Arc::new(callback));
    }

    // ------------------------------------------------------------------ lifecycle
    /// Open the stream and start the feeder (silence flows at once). Fails when the audio server
    /// refuses the stream, or a consumer program cannot be started or exits within
    /// [`START_GRACE_S`]. Does nothing when already started.
    pub fn start(&self) -> Result<(), OutputError> {
        let inner = &self.inner;
        let mut lifecycle = lock(&inner.lifecycle);
        {
            let mut state = lock(&inner.state);
            if state.started {
                return Ok(());
            }
            state.started = true;
            inner.stop.store(false, Ordering::SeqCst);
            state.error = None;
        }
        inner.consumer_log.open();
        let not_started = |error: OutputError| {
            lock(&inner.state).started = false;
            Err(error)
        };
        let mut program = None;
        let plan = if let Some(factory) = &inner.consumer_factory {
            inner.consumer_log.write("output: writing to the test's consumer");
            Plan::Custom(factory.clone())
        } else if lifecycle.injected {
            let Some(fd) = lifecycle.consumer_fd.take() else {
                return not_started(OutputError::Other(
                    "the injected descriptor was closed by an earlier stop()".into(),
                ));
            };
            inner.consumer_log.write(&format!("output: writing to injected fd {}", fd.as_raw_fd()));
            Plan::Pipe(self.pipe_consumer(File::from(fd)))
        } else if let Some(command) = inner.command.as_ref().filter(|command| !command.is_empty()) {
            inner.consumer_log.write(&format!("output: starting {}", command.join(" ")));
            let spawned = Command::new(&command[0])
                .args(&command[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match spawned {
                Ok(child) => child,
                Err(error) => {
                    let reason = format!("{}: '{}'", os_error_text(&error), command[0]);
                    return not_started(OutputError::Start { program: command[0].clone(), reason });
                }
            };
            let stdin = child.stdin.take().expect("the child was given a stdin pipe");
            let stderr = child.stderr.take();
            *lock(&inner.child) = Some(child);
            lifecycle.stderr_reader = stderr.and_then(|stderr| {
                let inner = inner.clone();
                Worker::spawn("am-ui-cons-log", move || inner.read_stderr(stderr)).ok()
            });
            program = Some(command[0].clone());
            Plan::Pipe(self.pipe_consumer(File::from(OwnedFd::from(stdin))))
        } else {
            Plan::Pulse(PulseSettings {
                server: inner.server.clone(),
                target: Some(inner.target.clone()).filter(|target| !target.is_empty()),
                buffer_ms: inner.buffer_ms,
                application_id: inner.application_id.clone(),
                icon_name: inner.icon_name.clone(),
                stream_name: inner.stream_name.clone(),
                underflows: inner.underflows.clone(),
                wake_fd: inner.waker.fd(),
            })
        };
        let run = inner.begin_run();
        inner.starting.store(true, Ordering::SeqCst);
        let (ready, answer) = mpsc::sync_channel(1);
        let feeder = {
            let inner = inner.clone();
            Worker::spawn("am-ui-feeder", move || inner.run_feeder(plan, run, ready))
        };
        let outcome = match feeder {
            Ok(feeder) => {
                lifecycle.feeder = Some(feeder);
                let opened = answer
                    .recv_timeout(Duration::from_secs_f64(CONNECT_TIMEOUT_S + 2.0))
                    .unwrap_or_else(|_| Err(OutputError::Server("the feeder did not open the stream".into())));
                match (opened, program) {
                    (Ok(()), Some(program)) => self.watch_start(&mut lifecycle, program),
                    (Ok(()), None) => Ok(()),
                    (Err(error), _) => {
                        self.abandon_start(&mut lifecycle);
                        Err(error)
                    }
                }
            }
            Err(error) => {
                inner.stop.store(true, Ordering::SeqCst);
                inner.shutdown_process(&mut lifecycle);
                let mut state = lock(&inner.state);
                state.alive = false;
                state.started = false;
                Err(OutputError::Other(format!("could not start the feeder thread: {}", os_error_text(&error))))
            }
        };
        inner.starting.store(false, Ordering::SeqCst);
        outcome
    }

    /// A pipe for the consumer: asked to hold [`OutputOptions::pipe_bytes`].
    fn pipe_consumer(&self, pipe: File) -> PipeConsumer {
        let inner = &self.inner;
        let pipe_bytes = set_pipe_size(&pipe, inner.pipe_bytes_requested);
        let pipe_frames = (pipe_bytes / OUT_FRAME_BYTES).max(1);
        inner.pipe_bytes.store(pipe_bytes, Ordering::Relaxed);
        inner.consumer_log.write(&format!("output: pipe {pipe_bytes} bytes ({pipe_frames} frames)"));
        inner.log.write(&format!(
            "output: started, pipe {pipe_bytes} bytes ({} ms)",
            pipe_frames * 1000 / SAMPLE_RATE as usize
        ));
        PipeConsumer::new(pipe, pipe_bytes, inner.fallback_delay())
    }

    /// The stream did not open: let the feeder and the consumer process go.
    fn abandon_start(&self, lifecycle: &mut Lifecycle) {
        let inner = &self.inner;
        inner.stop.store(true, Ordering::SeqCst);
        inner.waker.wake();
        if let Some(feeder) = lifecycle.feeder.take() {
            feeder.join(Duration::from_secs(1));
        }
        inner.shutdown_process(lifecycle);
        let mut state = lock(&inner.state);
        state.alive = false;
        state.started = false;
    }

    /// The consumer process must survive the start grace with the feeder writing.
    fn watch_start(&self, lifecycle: &mut Lifecycle, program: String) -> Result<(), OutputError> {
        let inner = &self.inner;
        let deadline = Instant::now() + Duration::from_secs_f64(START_GRACE_S);
        while Instant::now() < deadline {
            if inner.child_exit().is_some() || lock(&inner.state).error.is_some() {
                inner.stop.store(true, Ordering::SeqCst);
                if let Some(feeder) = lifecycle.feeder.take() {
                    feeder.join(Duration::from_secs(1));
                }
                let status = inner.shutdown_process(lifecycle).unwrap_or(0);
                let detail = lock(&inner.last_stderr).clone();
                let mut state = lock(&inner.state);
                state.alive = false;
                state.started = false;
                return Err(OutputError::Exited { program, status, detail });
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Let the feeder end the stream with a last 10 ms fade (heard at once on a stream that can
    /// be rewritten, after what it holds on a pipe), close the stream and see a consumer process
    /// out (terminated when it does not drain). Does nothing when not started.
    pub fn stop(&self) {
        let inner = &self.inner;
        let mut lifecycle = lock(&inner.lifecycle);
        {
            let state = lock(&inner.state);
            if !state.started {
                return;
            }
            inner.stop.store(true, Ordering::SeqCst);
        }
        inner.waker.wake();
        if let Some(feeder) = lifecycle.feeder.take() {
            let mut ended = feeder.wait(Duration::from_secs(1));
            if !ended && lock(&inner.child).is_some() {
                // the write is stuck: the consumer is not reading; terminating it breaks the pipe
                inner.signal_child(libc::SIGTERM);
                ended = feeder.wait(Duration::from_secs(1));
            }
            if ended {
                feeder.join(Duration::ZERO);
            }
        }
        inner.shutdown_process(&mut lifecycle);
        let (underruns, handoffs) = {
            let mut state = lock(&inner.state);
            state.alive = false;
            state.started = false;
            state.corked = false;
            state.readers.clear();
            (state.underruns, state.handoffs)
        };
        inner.consumer_log.write("output: stopped");
        inner.log.write(&format!(
            "output: stopped (underruns {underruns}, handoffs {handoffs}, pipe low {}, underflows {})",
            inner.pipe_low_count.load(Ordering::Relaxed),
            inner.underflows.load(Ordering::Relaxed)
        ));
        inner.consumer_log.close();
    }

    /// True while the output is started and has not failed.
    pub fn alive(&self) -> bool {
        lock(&self.inner.state).alive && !self.inner.stop.load(Ordering::SeqCst)
    }

    /// Why the output died, when it did.
    pub fn error(&self) -> Option<String> {
        lock(&self.inner.state).error.clone()
    }

    // ------------------------------------------------------------------ commands
    /// Switch to a new track (`None` = silence). On a stream that can be rewritten it is heard
    /// at once (after a 10 ms fade of the old one); on a pipe after the queued audio drains.
    pub fn set_source(&self, source: Option<Arc<dyn PcmSource>>, start_frame: u64) {
        let start = start_frame.min(MAX_FRAME) as i64;
        let label = source_label(source.as_deref());
        {
            let mut state = lock(&self.inner.state);
            if let Some(source) = &source {
                let index = state.reader_index(source);
                state.readers[index].open();
            }
            state.pending_source = Some((source, start));
            state.pending_seek = None;
            state.next_source = None;
            state.generation += 1;
            state.jump_frame = start;
            state.last_position = None;
        }
        self.inner.waker.wake();
        self.inner.log.write(&format!("set_source {label} @ {start}"));
    }

    /// Gapless continuation once the current source is exhausted (`None` disarms it).
    pub fn set_next_source(&self, source: Option<Arc<dyn PcmSource>>) {
        let label = source_label(source.as_deref());
        {
            let mut state = lock(&self.inner.state);
            if same_source(source.as_ref(), state.next_source.as_ref()) {
                return;
            }
            if let Some(source) = &source {
                let index = state.reader_index(source);
                state.readers[index].open();
            }
            state.next_source = source;
        }
        self.inner.log.write(&format!("set_next_source {label}"));
    }

    /// Continue the active source at `frame`.
    pub fn seek(&self, frame: u64) {
        let frame = frame.min(MAX_FRAME) as i64;
        let label = {
            let mut state = lock(&self.inner.state);
            if let Some((_, start)) = state.pending_source.as_mut() {
                *start = frame;
            } else if state.source.is_some() {
                state.pending_seek = Some(frame);
            } else {
                return;
            }
            state.generation += 1;
            state.jump_frame = frame;
            state.last_position = None;
            source_label(state.active_source().map(|source| &**source))
        };
        self.inner.waker.wake();
        self.inner.log.write(&format!("seek {label} -> {frame}"));
    }

    fn set_paused(&self, paused: bool) {
        let (label, cursor) = {
            let mut state = lock(&self.inner.state);
            if state.paused == paused {
                return;
            }
            state.paused = paused;
            (source_label(state.active_source().map(|source| &**source)), state.cursor_frame())
        };
        self.inner.waker.wake();
        self.inner.log.write(&format!("{} {label} @ {cursor}", if paused { "pause" } else { "resume" }));
    }

    /// Fade to silence and hold the position: at once on a stream that can be corked (which
    /// then corks), after the queued audio on a pipe (which gets silence).
    pub fn pause(&self) {
        self.set_paused(true);
    }

    pub fn resume(&self) {
        self.set_paused(false);
    }

    pub fn paused(&self) -> bool {
        lock(&self.inner.state).paused
    }

    /// 0..1 linear software gain, reached with a 10 ms ramp.
    pub fn set_volume(&self, volume: f64) {
        lock(&self.inner.state).volume = unit_gain(volume);
    }

    pub fn volume(&self) -> f64 {
        lock(&self.inner.state).volume
    }

    /// The downmix and the equalizer (any thread): the feeder takes them before its next block
    /// and the chain cross-fades to them over 20 ms.
    pub fn set_dsp(&self, settings: dsp::Settings) {
        let line = format!(
            "set_dsp preamp {:.4} eq gain {:.4} filters {} lfe {:.4}",
            settings.preamp,
            settings.eq_gain,
            settings.bands.len(),
            settings.lfe
        );
        {
            let mut state = lock(&self.inner.state);
            if state.dsp == settings {
                return;
            }
            state.dsp = settings.clone();
            state.dsp_pending = Some(settings);
        }
        self.inner.log.write(&line);
    }

    /// The downmix and equalizer settings in effect or on their way.
    pub fn dsp(&self) -> dsp::Settings {
        lock(&self.inner.state).dsp.clone()
    }

    /// Sound Check trim for one source (dB, attenuation only). A source starts at its trim
    /// within 50 ms; a change while that source plays (a refined loudness measurement) ramps
    /// over `ramp_s` ([`TRIM_RAMP_S`] unless the caller wants it slower).
    pub fn set_trim(&self, source: &Arc<dyn PcmSource>, db: f64, ramp_s: f64) {
        let gain = if db.is_finite() { 10f64.powf(db.min(0.0) / 20.0) } else { 1.0 };
        let blocks = if ramp_s.is_finite() {
            ((ramp_s * f64::from(SAMPLE_RATE) / BLOCK_FRAMES as f64).round_ties_even() as i64).max(1)
        } else {
            TRIM_RAMP_BLOCKS as i64
        };
        let mut state = lock(&self.inner.state);
        match state.trims.iter_mut().find(|trim| trim.source.is(Some(source))) {
            Some(trim) => (trim.gain, trim.blocks) = (gain, blocks),
            None => state.trims.push(Trim { source: Identity::of(Some(source)), gain, blocks }),
        }
        while state.trims.len() > 32 {
            state.trims.remove(0);
        }
    }

    /// Apply the trims (on) or play every source at its own level (off); either way with a
    /// 50 ms ramp.
    pub fn set_sound_check(&self, enabled: bool) {
        {
            let mut state = lock(&self.inner.state);
            if state.sound_check == enabled {
                return;
            }
            state.sound_check = enabled;
            state.trim_fast = true;
        }
        self.inner.log.write(&format!("sound check {}", if enabled { "on" } else { "off" }));
    }

    // ------------------------------------------------------------------ inspection
    /// The source about to start, else the one playing (it stays after it ended).
    pub fn active_source(&self) -> Option<Arc<dyn PcmSource>> {
        lock(&self.inner.state).active_source().cloned()
    }

    pub fn next_source(&self) -> Option<Arc<dyn PcmSource>> {
        lock(&self.inner.state).next_source.clone()
    }

    /// True while a growing source has not reached the cursor (silence is playing).
    pub fn buffering(&self) -> bool {
        lock(&self.inner.state).buffering
    }

    /// Starvation episodes since the output was created (a growing render fell behind the
    /// cursor).
    pub fn underruns(&self) -> u64 {
        lock(&self.inner.state).underruns
    }

    /// Times the audio server ran out of audio since the output was created (the feeder was
    /// late); apart from [`underruns`](Self::underruns).
    pub fn underflows(&self) -> u64 {
        self.inner.underflows.load(Ordering::Relaxed)
    }

    /// Gapless continuations into a next source since the output was created.
    pub fn handoffs(&self) -> u64 {
        lock(&self.inner.state).handoffs
    }

    /// The Sound Check trim currently applied (end of the last block), in dB.
    pub fn trim_db(&self) -> f64 {
        let trim = lock(&self.inner.state).trim;
        if 0.0 < trim && trim < 1.0 { 20.0 * trim.log10() } else { 0.0 }
    }

    pub fn sound_check(&self) -> bool {
        lock(&self.inner.state).sound_check
    }

    /// Largest feeder-side gap between two block writes in the last completed second.
    pub fn feeder_gap_ms(&self) -> f64 {
        f64::from_bits(self.inner.feeder_gap_ms.load(Ordering::Relaxed))
    }

    /// Writes that found less than [`LOW_WATER_FRAMES`] in the consumer (it was about to run
    /// out).
    pub fn pipe_low_count(&self) -> u64 {
        self.inner.pipe_low_count.load(Ordering::Relaxed)
    }

    /// Frames composed since `start()` (the stream frames of positions and events).
    pub fn frames_written(&self) -> u64 {
        lock(&self.inner.state).frames_written.max(0) as u64
    }

    /// The source frame the feeder reads next (the target of a jump that has not landed yet).
    pub fn cursor_frame(&self) -> u64 {
        lock(&self.inner.state).cursor_frame().max(0) as u64
    }

    /// Capacity of a consumer program's pipe (what the kernel granted once started).
    pub fn pipe_bytes(&self) -> usize {
        self.inner.pipe_bytes.load(Ordering::Relaxed)
    }

    /// Frames between being taken by the consumer and being heard: the device latency the
    /// audio server reports (`latency_ms * 48` for a pipe and before it said).
    pub fn delay_frames(&self) -> u64 {
        lock(&self.inner.state).clock.delay.max(0) as u64
    }

    /// Frames given to the consumer that have not been heard yet (its buffer and the device's
    /// latency).
    pub fn latency_frames(&self) -> u64 {
        let state = lock(&self.inner.state);
        (state.transport.sent - state.heard(Instant::now())).max(0) as u64
    }

    /// The stream's index on the audio server (its sink input), once it is open.
    pub fn node_id(&self) -> Option<u32> {
        lock(&self.inner.status).1
    }

    /// The stream's state as the consumer last said ("streaming", "corked"...).
    pub fn stream_state(&self) -> Option<String> {
        lock(&self.inner.status).0.clone()
    }

    /// The stream is corked (paused, or nothing to play).
    pub fn corked(&self) -> bool {
        lock(&self.inner.state).corked
    }

    /// The scheduling the feeder thread got in the last start.
    pub fn priority(&self) -> Option<Grant> {
        lock(&self.inner.grant).clone()
    }

    /// Source frame currently audible (approximately), for the active source.
    pub fn position_frames(&self) -> u64 {
        let mut state = lock(&self.inner.state);
        if state.active_source().is_none() {
            return 0;
        }
        let mut value = state.map_position(state.audible(Instant::now()));
        let generation = state.generation;
        if let Some((seen, position)) = state.last_position
            && seen == generation
        {
            value = value.max(position);
        }
        state.last_position = Some((generation, value));
        value.max(0) as u64
    }

    /// Source frames available beyond the current position (render progress ahead of playback).
    pub fn buffered_frames(&self) -> u64 {
        let (source, cursor) = {
            let state = lock(&self.inner.state);
            match &state.pending_source {
                Some((source, start)) => (source.clone(), *start),
                None => (state.source.clone(), state.cursor),
            }
        };
        let Some(source) = source else { return 0 };
        let available = catch_unwind(AssertUnwindSafe(|| source.frames_available())).unwrap_or(0);
        (available.min(MAX_FRAME) as i64 - cursor).max(0) as u64
    }

    /// Stream frame (since start) estimated to be leaving the speakers now.
    pub fn audible_stream_frame(&self) -> u64 {
        lock(&self.inner.state).audible(Instant::now()) as u64
    }

    /// Write the last [`RING_SECONDS`] of the composed stereo stream as a 48 kHz float WAV.
    pub fn dump_recent(&self, path: &Path) -> io::Result<PathBuf> {
        let (data, stream_end) = {
            let state = lock(&self.inner.state);
            let data = if state.ring_count < RING_FRAMES {
                state.ring[..state.ring_count * OUT_CHANNELS].to_vec()
            } else {
                let (newest, oldest) = state.ring.split_at(state.ring_pos * OUT_CHANNELS);
                [oldest, newest].concat()
            };
            (data, state.ring_stream_end)
        };
        let result = write_wav_float32(path, &data, OUT_CHANNELS, SAMPLE_RATE)?;
        self.inner.log.write(&format!(
            "dump_recent {} ({:.1} s ending at stream frame {stream_end})",
            result.display(),
            (data.len() / OUT_CHANNELS) as f64 / f64::from(SAMPLE_RATE)
        ));
        Ok(result)
    }
}

impl Drop for PipeWireOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
