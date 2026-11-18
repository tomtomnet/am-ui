//! What takes the stereo stream: the seam, the waker that interrupts its waits, and the pipe.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::SAMPLE_RATE;
use crate::loudness::os_error_text;

/// Interleaved stereo frames out of the chain.
pub const OUT_CHANNELS: usize = 2;
/// One stereo float32 frame.
pub const OUT_FRAME_BYTES: usize = OUT_CHANNELS * 4;

/// Where a consumer's playback is, as it says: what the output places what is heard by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clock {
    /// Frames the consumer had taken (its read position) at `at`, counted like the frames
    /// written to it.
    pub played: i64,
    /// When `played` held; `None` while nothing is known: what is written counts as taken.
    pub at: Option<Instant>,
    /// Whether the read position moves on in real time after `at`.
    pub running: bool,
    /// Frames between being taken and being heard (the device's part of the latency).
    pub delay: i64,
}

impl Clock {
    /// Nothing known yet: what is written counts as taken, `delay` frames before it is heard.
    pub fn unknown(delay: i64) -> Clock {
        Clock { played: 0, at: None, running: false, delay }
    }

    /// The read position at `now` of a consumer that was given `written` frames: moved on from
    /// `played` while running, never beyond what it was given.
    pub fn played_at(&self, written: i64, now: Instant) -> i64 {
        let Some(at) = self.at else { return written };
        let moved = if self.running {
            (now.saturating_duration_since(at).as_secs_f64() * f64::from(SAMPLE_RATE)) as i64
        } else {
            0
        };
        self.played.saturating_add(moved).min(written)
    }

    /// The frame being heard at `now` (frames before the first count as the first).
    pub fn heard(&self, written: i64, now: Instant) -> i64 {
        (self.played_at(written, now) - self.delay).max(0)
    }
}

/// Interrupts the feeder's waits: commands (pause, seek, a new source, stop) wake it, so that
/// they act at once. An eventfd, which a consumer may watch in its own poll.
pub struct Waker {
    fd: Option<OwnedFd>,
}

impl Waker {
    pub(super) fn new() -> Waker {
        // SAFETY: eventfd takes no pointers; a descriptor it returns is ours alone.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        // SAFETY: as above, the descriptor is new and owned by nothing else.
        Waker { fd: (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }) }
    }

    /// The descriptor to watch for input, when there is one.
    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Wake the feeder (any thread).
    pub fn wake(&self) {
        if let Some(fd) = self.fd() {
            let one = 1u64;
            // SAFETY: eight bytes from a valid u64; a full counter (EAGAIN) means woken anyway.
            unsafe { libc::write(fd, (&raw const one).cast(), 8) };
        }
    }

    /// Take back a wake-up that was not waited for.
    pub fn clear(&self) {
        if let Some(fd) = self.fd() {
            let mut count = 0u64;
            // SAFETY: eight bytes into a valid u64; the descriptor does not block.
            unsafe { libc::read(fd, (&raw mut count).cast(), 8) };
        }
    }

    /// Wait until woken or `timeout` passed; true when woken.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Some(fd) = self.fd() else {
            thread::sleep(timeout.min(Duration::from_millis(20)));
            return false;
        };
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd for the call.
        let ready = unsafe { libc::poll(&mut poll, 1, millis) };
        self.clear();
        ready > 0
    }
}

/// What takes the stereo stream (interleaved 32-bit float, 48 kHz), as a seam: the PulseAudio
/// stream, a pipe, a test's stand-in. A consumer is made on the feeder thread (by a
/// [`ConsumerFactory`] or by the output itself) and every call comes from that thread.
pub trait Consumer {
    /// Connect; an error ends `start()` with this message.
    fn start(&mut self) -> Result<(), String>;

    /// Wait until `frames` can be written without waiting, `waker` was woken or `timeout`
    /// passed; true when there is room. A consumer whose writes wait by themselves says so at
    /// once.
    fn wait(&mut self, frames: usize, waker: &Waker, timeout: Duration) -> Result<bool, String>;

    /// Write interleaved stereo frames.
    fn write(&mut self, frames: &[f32]) -> Result<(), String>;

    /// How far it has played; `written` is what it was given (its write position: frames since it
    /// started, as moved by [`flush`](Consumer::flush)).
    fn clock(&mut self, written: i64) -> Clock;

    /// Whether it can be paused (corked), flushed and rewritten. One that cannot gets silence
    /// while the output is paused and jumps after what it holds, as a pipe does.
    fn can_pause(&self) -> bool {
        false
    }

    /// Stop taking frames, keeping what it holds (cork).
    fn pause(&mut self) -> Result<(), String> {
        Err("this consumer cannot pause".to_string())
    }

    /// Take frames again (uncork).
    fn resume(&mut self) -> Result<(), String> {
        Err("this consumer cannot pause".to_string())
    }

    /// Drop what it holds and has not played yet; returns its write position afterwards, where
    /// the next frame written lands. The feeder flushes only a corked consumer, whose read
    /// position stands still.
    fn flush(&mut self) -> Result<i64, String> {
        Err("this consumer cannot flush".to_string())
    }

    /// The first frame of what it holds that can still be replaced while it plays: one it will
    /// not have taken yet when a write sent now arrives.
    fn rewrite_from(&mut self) -> Result<i64, String> {
        Err("this consumer cannot rewrite".to_string())
    }

    /// Replace what it holds from frame `at` on (at or after [`rewrite_from`]) with `frames`:
    /// its write position becomes `at` plus their count. What it holds before `at` plays as it
    /// was.
    ///
    /// [`rewrite_from`]: Consumer::rewrite_from
    fn rewrite(&mut self, at: i64, frames: &[f32]) -> Result<(), String> {
        let _ = (at, frames);
        Err("this consumer cannot rewrite".to_string())
    }

    /// The underflows it reported: times it ran out of frames to play.
    fn underflows(&self) -> u64 {
        0
    }

    /// Wait for `waker` or `timeout`, attending to the consumer meanwhile (the output is corked).
    fn idle(&mut self, waker: &Waker, timeout: Duration) -> Result<(), String> {
        waker.wait(timeout);
        Ok(())
    }

    /// Lines for the output's log, oldest first; `true`: the playback log gets the line too.
    fn take_log(&mut self) -> Vec<(bool, String)> {
        Vec::new()
    }

    /// Its state in words ("streaming", "corked"...) and its index on the audio server.
    fn status(&self) -> (Option<String>, Option<u32>) {
        (None, None)
    }

    /// Wait up to `timeout` until what it was given has been played (not while corked).
    fn drain(&mut self, timeout: Duration) {
        let _ = timeout;
    }

    /// Let go of the stream.
    fn stop(&mut self);
}

/// Makes the consumer of each run, on the feeder thread (tests: a stand-in).
pub type ConsumerFactory = Arc<dyn Fn() -> Box<dyn Consumer> + Send + Sync>;

/// The write end of a pipe: a consumer process's stdin or an injected descriptor. A pipe cannot
/// be corked, flushed or rewritten, so a paused output writes silence into it, as before; its
/// reader is taken to play in real time, `delay` frames behind what it read.
pub(super) struct PipeConsumer {
    pipe: File,
    /// Its capacity in stereo frames.
    capacity: i64,
    fionread_ok: bool,
    delay: i64,
    bytes: Vec<u8>,
}

impl PipeConsumer {
    pub(super) fn new(pipe: File, capacity_bytes: usize, delay: i64) -> PipeConsumer {
        PipeConsumer {
            pipe,
            capacity: (capacity_bytes / OUT_FRAME_BYTES).max(1) as i64,
            fionread_ok: true,
            delay,
            bytes: Vec::new(),
        }
    }

    /// Frames in the pipe (FIONREAD on the write end, which Linux supports); the whole pipe when
    /// that cannot be asked (the writer is blocked almost always).
    fn queued(&mut self) -> i64 {
        if self.fionread_ok {
            let mut queued: libc::c_int = 0;
            // SAFETY: FIONREAD stores one int through the pointer, which is valid for the call.
            if unsafe { libc::ioctl(self.pipe.as_raw_fd(), libc::FIONREAD, &mut queued) } == 0 {
                return i64::from(queued.max(0)) / OUT_FRAME_BYTES as i64;
            }
            self.fionread_ok = false;
        }
        self.capacity
    }
}

impl Consumer for PipeConsumer {
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn wait(&mut self, _frames: usize, _waker: &Waker, _timeout: Duration) -> Result<bool, String> {
        Ok(true) // the write waits
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), String> {
        self.bytes.clear();
        self.bytes.extend(frames.iter().flat_map(|sample| sample.to_ne_bytes()));
        self.pipe.write_all(&self.bytes).map_err(|error| os_error_text(&error))
    }

    fn clock(&mut self, written: i64) -> Clock {
        let queued = self.queued().min(written);
        Clock { played: written - queued, at: Some(Instant::now()), running: true, delay: self.delay }
    }

    fn stop(&mut self) {}
}

/// Ask for a pipe of `requested` bytes; returns what the pipe holds afterwards.
pub(super) fn set_pipe_size(pipe: &File, requested: usize) -> usize {
    let fd = pipe.as_raw_fd();
    let requested = libc::c_int::try_from(requested).unwrap_or(libc::c_int::MAX);
    // SAFETY: fcntl with integer arguments on a descriptor this process owns.
    let granted = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, requested) };
    if granted >= 0 {
        return granted as usize;
    }
    // SAFETY: as above.
    let current = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    if current >= 0 { current as usize } else { 64 * 1024 }
}
