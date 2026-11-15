//! What the feeder and the commands share: the 16-channel composition of the Python feeder
//! (sources, cursor, segments, events, gain ramps, Sound Check trims), the table that says
//! which composed frame each frame given to the consumer carries, and the consumer's clock.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Weak};
use std::time::Instant;

use super::consumer::{Clock, OUT_CHANNELS};
use super::{
    BLOCK_FRAMES, MAX_FRAME, MAX_SEGMENT_AGE_FRAMES, RESERVE_FRAMES, RESUME_FRAMES, RING_FRAMES, TRIM_RAMP_BLOCKS,
    source_label,
};
use crate::config::{CHANNELS, SAMPLE_RATE};
use crate::dsp;
use crate::render::PcmSource;

pub(super) type Source = Arc<dyn PcmSource>;

/// Which source a segment or a trim belongs to. Only ever compared: holding it keeps the
/// source's address from being used again, not the source alive.
#[derive(Clone, Default)]
pub(super) struct Identity(Option<Weak<dyn PcmSource>>);

impl Identity {
    pub(super) fn of(source: Option<&Source>) -> Identity {
        Identity(source.map(Arc::downgrade))
    }

    pub(super) fn is(&self, source: Option<&Source>) -> bool {
        match (&self.0, source) {
            (None, None) => true,
            (Some(mine), Some(other)) => std::ptr::addr_eq(mine.as_ptr(), Arc::as_ptr(other)),
            _ => false,
        }
    }
}

pub(super) fn same_source(a: Option<&Source>, b: Option<&Source>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// Stream frames from `stream_start` on map to `source`: frame + offset when advancing, held
/// otherwise.
pub(super) struct Segment {
    pub(super) stream_start: i64,
    pub(super) source: Identity,
    pub(super) frame: i64,
    pub(super) advancing: bool,
    pub(super) generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum EventKind {
    Started,
    Ended,
}

/// A callback due once the stream frame is audible.
pub(super) struct QueuedEvent {
    pub(super) stream_frame: i64,
    pub(super) kind: EventKind,
    pub(super) source: Source,
}

/// Sound Check trim of one source: linear gain and the blocks a change ramps over.
pub(super) struct Trim {
    pub(super) source: Identity,
    pub(super) gain: f64,
    pub(super) blocks: i64,
}

/// Positioned reads of a (possibly still growing) interleaved f32le file.
///
/// The file is opened as soon as the source is armed and kept: if the cache directory is
/// re-claimed or evicted under us, reads continue from the inode we opened and `file_frames()`
/// caps what the feeder believes exists, so no block is ever read at a wrong offset of a
/// replacement file.
pub(super) struct Reader {
    source: Source,
    channels: usize,
    frame_bytes: usize,
    file: Option<File>,
    max_available: i64,
    replaced_logged: bool,
    shrink_logged: bool,
    /// One block of the file's bytes.
    bytes: Vec<u8>,
}

impl Reader {
    fn new(source: Source) -> Reader {
        let channels = match source.channels() {
            0 => CHANNELS,
            channels => channels,
        };
        Reader {
            source,
            channels,
            frame_bytes: channels * 4,
            file: None,
            max_available: 0,
            replaced_logged: false,
            shrink_logged: false,
            bytes: vec![0; BLOCK_FRAMES * channels * 4],
        }
    }

    pub(super) fn open(&mut self) -> bool {
        if self.file.is_none() {
            self.file = File::open(self.source.pcm_path()).ok();
        }
        self.file.is_some()
    }

    /// Frames in the file we actually read (`None` when it could not be opened yet).
    fn file_frames(&mut self) -> Option<i64> {
        if !self.open() {
            return None;
        }
        let size = self.file.as_ref()?.metadata().ok()?.len();
        Some((size / self.frame_bytes as u64) as i64)
    }

    /// Read up to `count` frames from `frame` into `bytes`; returns the frames read (fewer: the
    /// file is shorter).
    fn read(&mut self, frame: i64, count: usize) -> usize {
        if !self.open() {
            return 0;
        }
        let Some(file) = self.file.as_ref() else { return 0 };
        let want = count.min(BLOCK_FRAMES) * self.frame_bytes;
        let offset = frame.max(0) as u64 * self.frame_bytes as u64;
        let mut filled = 0;
        while filled < want {
            match file.read_at(&mut self.bytes[filled..want], offset + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        filled / self.frame_bytes
    }
}

/// What a source says about itself, in the feeder's terms. A source that panics counts as
/// finished at `cursor` (a dead render session: play what we have).
fn ask(source: &Source, cursor: i64) -> (i64, bool, Option<i64>) {
    let frames = |count: u64| count.min(MAX_FRAME) as i64;
    catch_unwind(AssertUnwindSafe(|| {
        let available = frames(source.frames_available());
        let done = source.done() || source.error().is_some();
        let total = if done { source.total_frames().map(frames) } else { None };
        (available, done, total)
    }))
    .unwrap_or((cursor, true, None))
}

/// From consumer frame `at` on, the consumer plays the composed frames from `ours` on, or, for
/// padding (`held`), silence that stands for composed frame `ours`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Piece {
    pub(super) at: i64,
    pub(super) ours: i64,
    pub(super) held: bool,
}

/// Which composed frame each frame given to the consumer carries. A consumer that is never
/// rewritten or flushed gets every composed block once, in order: one piece, the identity. A
/// rewrite or a flush takes back what the consumer had not played from some frame on, which
/// moves its write position back; the frames it gets next (a fade of what it was about to play,
/// the jump's new audio, silence while paused, the audio taken back again on resume) are
/// described by the pieces that follow.
pub(super) struct Transport {
    /// Frames given to the consumer: its write position.
    pub(super) sent: i64,
    /// In order of `at`; never empty.
    pub(super) pieces: VecDeque<Piece>,
}

impl Transport {
    pub(super) fn new() -> Transport {
        Transport { sent: 0, pieces: VecDeque::from([Piece { at: 0, ours: 0, held: false }]) }
    }

    /// The composed frame that consumer frame `frame` carries.
    pub(super) fn ours(&self, frame: i64) -> i64 {
        let piece = self.pieces.iter().rev().find(|piece| piece.at <= frame).or(self.pieces.front());
        match piece {
            Some(piece) if piece.held => piece.ours,
            Some(piece) => piece.ours + (frame - piece.at).max(0),
            None => frame,
        }
    }

    fn push(&mut self, piece: Piece) {
        if self.pieces.back().is_some_and(|last| last.at >= piece.at) {
            self.pieces.pop_back(); // replaced before a frame of it was given
        }
        self.pieces.push_back(piece);
    }

    /// The next `count` frames given carry composed frames from `ours` on.
    pub(super) fn send(&mut self, ours: i64, count: i64) {
        let sent = self.sent;
        let continues = self.pieces.back().is_some_and(|last| !last.held && last.ours + (sent - last.at) == ours);
        if !continues {
            self.push(Piece { at: sent, ours, held: false });
        }
        self.sent += count;
    }

    /// The next `count` frames given are silence that stands for composed frame `ours`.
    pub(super) fn send_held(&mut self, ours: i64, count: i64) {
        if !self.pieces.back().is_some_and(|last| last.held && last.ours == ours) {
            self.push(Piece { at: self.sent, ours, held: true });
        }
        self.sent += count;
    }

    /// Frames given whose pieces are already in place (the audio taken back, given again).
    pub(super) fn advance(&mut self, count: i64) {
        self.sent += count;
    }

    /// The consumer dropped what it had from frame `at` on: its write position is `at` again.
    /// Returns the pieces that began there or later.
    pub(super) fn cut(&mut self, at: i64) -> Vec<Piece> {
        let keep = self.pieces.iter().position(|piece| piece.at >= at).unwrap_or(self.pieces.len()).max(1);
        self.sent = at.min(self.sent);
        self.pieces.drain(keep..).collect()
    }

    /// Place pieces again from consumer frame `at` on, keeping their distances.
    pub(super) fn place(&mut self, pieces: &[Piece], at: i64) {
        let Some(first) = pieces.first() else { return };
        let shift = at - first.at;
        for piece in pieces {
            self.push(Piece { at: piece.at + shift, ..*piece });
        }
    }

    /// Forget pieces that ended before consumer frame `horizon`.
    fn prune(&mut self, horizon: i64) {
        while self.pieces.len() > 1 && self.pieces[1].at < horizon {
            self.pieces.pop_front();
        }
    }
}

/// Everything the feeder and the commands share (one lock).
pub(super) struct State {
    pub(super) started: bool,
    /// Counts the starts. A feeder serves the run it was started for: one that outlives its
    /// run (stuck in a write or a callback while `stop()` gave up on it) finds another number
    /// here and leaves the state alone.
    pub(super) run: u64,
    pub(super) alive: bool,
    pub(super) error: Option<String>,

    pub(super) source: Option<Source>,
    pub(super) cursor: i64,
    pub(super) next_source: Option<Source>,
    pub(super) pending_source: Option<(Option<Source>, i64)>,
    pub(super) pending_seek: Option<i64>,
    pub(super) paused: bool,
    pub(super) volume: f64,
    /// Total gain (volume x trim) at the end of the last block composed.
    pub(super) gain: f64,
    pub(super) buffering: bool,
    /// Inside a starvation episode: waiting for `RESUME_FRAMES`.
    pub(super) starved: bool,
    pub(super) underruns: u64,
    pub(super) handoffs: u64,
    /// The active source is exhausted (no next source was armed).
    pub(super) ended: bool,
    /// Queue the started event when the source's first frame is written.
    pub(super) start_pending: bool,
    /// Frames composed since `start()` (the stream frames of segments and events).
    pub(super) frames_written: i64,
    pub(super) segments: VecDeque<Segment>,
    pub(super) events: VecDeque<QueuedEvent>,
    pub(super) readers: Vec<Reader>,
    /// Bumped by every set_source/seek request and gapless handoff.
    pub(super) generation: u64,
    /// The generation the feeder is currently writing.
    pub(super) content_generation: u64,
    /// Position reported while older content is still audible.
    pub(super) jump_frame: i64,
    pub(super) last_position: Option<(u64, i64)>,
    /// Lines composed under the lock, written after the block.
    pub(super) pending_log: Vec<String>,

    // Sound Check trims (linear, ramp blocks) per source
    pub(super) trims: Vec<Trim>,
    pub(super) sound_check: bool,
    pub(super) trim: f64,
    pub(super) trim_target: f64,
    pub(super) trim_step: f64,
    /// The source the current trim target belongs to.
    pub(super) trim_source: Identity,
    /// The next target change ramps over `TRIM_RAMP_BLOCKS` (Sound Check toggled).
    pub(super) trim_fast: bool,

    /// The downmix and equalizer settings, and those not taken over by the feeder yet.
    pub(super) dsp: dsp::Settings,
    pub(super) dsp_pending: Option<dsp::Settings>,

    /// The consumer's frames and the composed frames they carry.
    pub(super) transport: Transport,
    /// Where the consumer's playback is, as it last said.
    pub(super) clock: Clock,
    /// The consumer is corked (paused, or idle).
    pub(super) corked: bool,

    // recent-audio ring buffer (the composed stereo stream), interleaved
    pub(super) ring: Vec<f32>,
    pub(super) ring_pos: usize,
    pub(super) ring_count: usize,
    /// Stream frame following the newest ring sample.
    pub(super) ring_stream_end: i64,
}

impl State {
    pub(super) fn new(volume: f64, dsp: dsp::Settings, fallback_delay: i64) -> State {
        State {
            started: false,
            run: 0,
            alive: false,
            error: None,
            source: None,
            cursor: 0,
            next_source: None,
            pending_source: None,
            pending_seek: None,
            paused: false,
            volume: unit_gain(volume),
            gain: 0.0,
            buffering: false,
            starved: false,
            underruns: 0,
            handoffs: 0,
            ended: false,
            start_pending: false,
            frames_written: 0,
            segments: VecDeque::with_capacity(64),
            events: VecDeque::with_capacity(8),
            readers: Vec::with_capacity(4),
            generation: 0,
            content_generation: 0,
            jump_frame: 0,
            last_position: None,
            pending_log: Vec::new(),
            trims: Vec::new(),
            sound_check: true,
            trim: 1.0,
            trim_target: 1.0,
            trim_step: 0.0,
            trim_source: Identity::default(),
            trim_fast: false,
            dsp,
            dsp_pending: None,
            transport: Transport::new(),
            clock: Clock::unknown(fallback_delay),
            corked: false,
            ring: vec![0.0; RING_FRAMES * OUT_CHANNELS],
            ring_pos: 0,
            ring_count: 0,
            ring_stream_end: 0,
        }
    }

    /// A new stream (`start()`): nothing written, nothing heard.
    pub(super) fn begin_stream(&mut self, fallback_delay: i64) {
        self.frames_written = 0;
        self.segments.clear();
        self.events.clear();
        self.gain = 0.0;
        self.starved = false;
        self.transport = Transport::new();
        self.clock = Clock::unknown(fallback_delay);
        self.corked = false;
    }

    /// The source commands refer to: the one about to start, else the one playing.
    pub(super) fn active_source(&self) -> Option<&Source> {
        match &self.pending_source {
            Some((source, _)) => source.as_ref(),
            None => self.source.as_ref(),
        }
    }

    pub(super) fn cursor_frame(&self) -> i64 {
        match (&self.pending_source, self.pending_seek) {
            (Some((_, start)), _) => *start,
            (None, Some(frame)) => frame,
            (None, None) => self.cursor,
        }
    }

    /// A set_source or seek waits for the feeder.
    pub(super) fn jump_pending(&self) -> bool {
        self.pending_source.is_some() || self.pending_seek.is_some()
    }

    /// The source a pending jump continues: the new one, or the active one for a seek.
    pub(super) fn jump_source(&self) -> Option<&Source> {
        match &self.pending_source {
            Some((source, _)) => source.as_ref(),
            None if self.pending_seek.is_some() => self.source.as_ref(),
            None => None,
        }
    }

    /// Nothing to play and nothing on its way: no source, or one that ended.
    pub(super) fn idle(&self) -> bool {
        match &self.pending_source {
            Some((source, _)) => source.is_none(),
            None => self.source.is_none() || (self.ended && self.pending_seek.is_none()),
        }
    }

    pub(super) fn reader_index(&mut self, source: &Source) -> usize {
        if let Some(index) = self.readers.iter().position(|reader| Arc::ptr_eq(&reader.source, source)) {
            return index;
        }
        self.readers.push(Reader::new(source.clone()));
        self.readers.len() - 1
    }

    fn prune_readers(&mut self) {
        let pending = self.pending_source.as_ref().and_then(|(source, _)| source.as_ref());
        let keep = [self.source.as_ref(), self.next_source.as_ref(), pending];
        self.readers.retain(|reader| keep.iter().flatten().any(|source| Arc::ptr_eq(source, &reader.source)));
    }

    /// The consumer frame being heard at `now`.
    pub(super) fn heard(&self, now: Instant) -> i64 {
        self.clock.heard(self.transport.sent, now)
    }

    /// Stream frame (composed) estimated to be leaving the speakers at `now`.
    pub(super) fn audible(&self, now: Instant) -> i64 {
        self.transport.ours(self.heard(now)).clamp(0, self.frames_written.max(0))
    }

    pub(super) fn map_position(&self, audible: i64) -> i64 {
        let Some(index) = self.segments.iter().rposition(|segment| segment.stream_start <= audible) else {
            return self.jump_frame;
        };
        let segment = &self.segments[index];
        if segment.generation < self.generation || !segment.source.is(self.active_source()) {
            return self.jump_frame; // what is audible predates the latest jump: report where we are going
        }
        if !segment.advancing {
            return segment.frame;
        }
        let value = segment.frame + (audible - segment.stream_start);
        match self.segments.get(index + 1) {
            Some(next) => value.min(segment.frame + (next.stream_start - segment.stream_start)),
            None => value,
        }
    }

    /// The events whose frame is audible, in order.
    pub(super) fn due_events(&mut self, audible: i64) -> Vec<QueuedEvent> {
        let mut due = Vec::new();
        while self.events.front().is_some_and(|event| event.stream_frame <= audible) {
            due.extend(self.events.pop_front());
        }
        due
    }

    /// A composed block was given to the consumer (`sent`) or skipped: count it.
    pub(super) fn advance(&mut self, sent: bool) {
        if sent {
            self.transport.send(self.frames_written, BLOCK_FRAMES as i64);
        }
        self.frames_written += BLOCK_FRAMES as i64;
        let horizon = self.transport.sent - MAX_SEGMENT_AGE_FRAMES as i64;
        self.transport.prune(horizon);
    }

    /// The consumer will never play the composed frames `unheard` (a jump replaced them), and
    /// the frames composed after them start at once. An `ended` event among them of `source`,
    /// the one the jump goes on with, is dropped (its end was not heard and it plays on); the
    /// other events stay and fire as soon as the frames that follow are audible.
    pub(super) fn drop_unheard(&mut self, source: Option<&Source>, unheard: std::ops::Range<i64>) {
        let Some(source) = source else { return };
        let before = self.events.len();
        self.events.retain(|event| {
            let ended = event.kind == EventKind::Ended && Arc::ptr_eq(&event.source, source);
            !(ended && unheard.contains(&event.stream_frame))
        });
        if self.events.len() < before {
            self.pending_log
                .push(format!("jump: the end of {} was not heard, it plays on", source_label(Some(&**source))));
        }
    }

    fn apply_jump(&mut self) {
        if let Some((source, start)) = self.pending_source.take() {
            self.pending_seek = None;
            self.start_pending = source.is_some();
            self.source = source;
            self.cursor = start;
        } else if let Some(frame) = self.pending_seek.take() {
            self.cursor = frame;
        }
        self.content_generation = self.generation;
        self.ended = false;
        self.buffering = false;
        self.starved = false;
        self.prune_readers();
    }

    /// (gain, ramp blocks) for the active source: 1.0 when unknown or Sound Check is off.
    fn trim_target(&self) -> (f64, i64) {
        let Some(source) = self.source.as_ref().filter(|_| self.sound_check) else {
            return (1.0, TRIM_RAMP_BLOCKS as i64);
        };
        match self.trims.iter().find(|trim| trim.source.is(Some(source))) {
            Some(trim) => (trim.gain, trim.blocks),
            None => (1.0, TRIM_RAMP_BLOCKS as i64),
        }
    }

    /// The trim gain at the end of this block: a linear ramp toward the target, over
    /// `TRIM_RAMP_BLOCKS` when the source changed or Sound Check was toggled, else over the
    /// source's own ramp (`set_trim()`).
    fn advance_trim(&mut self) -> f64 {
        let (target, mut blocks) = self.trim_target();
        if !self.trim_source.is(self.source.as_ref()) || self.trim_fast {
            self.trim_source = Identity::of(self.source.as_ref());
            self.trim_fast = false;
            blocks = TRIM_RAMP_BLOCKS as i64;
        }
        if target != self.trim_target {
            self.trim_target = target;
            self.trim_step = (target - self.trim) / blocks.max(1) as f64;
        }
        if self.trim == self.trim_target {
            return self.trim;
        }
        let value = self.trim + self.trim_step;
        let reached =
            (self.trim_step > 0.0 && value >= self.trim_target) || (self.trim_step < 0.0 && value <= self.trim_target);
        if reached { self.trim_target } else { value }
    }

    /// Compose the next 10 ms of 16 channels (lock held): content, segments, events and the
    /// gain ramp. What the Python feeder wrote, sample for sample.
    pub(super) fn fill_block(&mut self, block: &mut [f32], last: bool) {
        block.fill(0.0);
        let base = self.frames_written;
        let jump = self.jump_pending();
        let mut forced_target = None;
        if last {
            forced_target = Some(0.0);
        } else if jump {
            if self.gain > 0.0 {
                forced_target = Some(0.0); // fade the old position out first; the jump lands in the next block
            } else {
                self.apply_jump();
            }
        }
        let idle = self.paused || self.source.is_none() || self.ended;
        let mut desired = forced_target.unwrap_or(if idle { 0.0 } else { self.volume });
        // a source that plays is read on at volume 0: silence does not hold the song back
        let playing = forced_target.is_none() && !idle;
        let need_audio = self.source.is_some() && !self.ended && (playing || self.gain > 0.0);
        if need_audio {
            let (filled, starving) = self.fill_from_sources(block, base, forced_target.is_none());
            if starving || (filled == 0 && self.ended) {
                desired = 0.0;
            }
            if filled < BLOCK_FRAMES {
                self.note_segment(base + filled as i64, self.cursor, false);
            }
        } else {
            self.note_segment(base, self.cursor, false);
        }
        let trim = self.advance_trim();
        desired *= trim;
        apply_gain(block, self.gain, desired);
        self.gain = desired;
        self.trim = trim;
    }

    /// Record that from `stream_frame` on the stream plays (or holds at) `frame` of the active
    /// source.
    fn note_segment(&mut self, stream_frame: i64, frame: i64, advancing: bool) {
        let source = self.source.as_ref();
        if let Some(last) = self.segments.back() {
            if last.source.is(source) && last.advancing == advancing {
                if advancing && last.frame - last.stream_start == frame - stream_frame {
                    return;
                }
                if !advancing && last.frame == frame {
                    return;
                }
            }
            if last.stream_start == stream_frame {
                self.segments.pop_back(); // replaced within the same block (e.g. a zero-length piece)
            }
        }
        let source = Identity::of(source);
        self.segments.push_back(Segment {
            stream_start: stream_frame,
            source,
            frame,
            advancing,
            generation: self.content_generation,
        });
        let horizon = self.frames_written - MAX_SEGMENT_AGE_FRAMES as i64;
        while self.segments.len() > 2 && self.segments[1].stream_start < horizon {
            self.segments.pop_front();
        }
    }

    /// Keep a composed stereo block in the recent-audio ring.
    pub(super) fn ring_write(&mut self, stereo: &[f32]) {
        let mut at = self.ring_pos;
        for frame in stereo.as_chunks::<OUT_CHANNELS>().0 {
            self.ring[OUT_CHANNELS * at..OUT_CHANNELS * (at + 1)].copy_from_slice(frame);
            at += 1;
            if at == RING_FRAMES {
                at = 0;
            }
        }
        self.ring_pos = at;
        self.ring_count = (self.ring_count + stereo.len() / OUT_CHANNELS).min(RING_FRAMES);
        self.ring_stream_end = self.frames_written + (stereo.len() / OUT_CHANNELS) as i64;
    }

    /// Fill from the active source, continuing into the next one when it is exhausted.
    ///
    /// Returns (frames filled, starving). Starving: the block holds the last reserve audio (to
    /// be faded out) or nothing, and `buffering` is set.
    fn fill_from_sources(&mut self, block: &mut [f32], base: i64, allow_handoff: bool) -> (usize, bool) {
        let mut pos = 0;
        while pos < BLOCK_FRAMES {
            let Some(source) = self.source.clone() else { break };
            let remaining = BLOCK_FRAMES - pos;
            let (available, done, total) = ask(&source, self.cursor);
            let available = self.checked_available(&source, available, done);
            let count = if done {
                if self.starved {
                    self.end_starvation(&source, available);
                }
                let end = total.map_or(available, |total| total.min(available));
                if self.cursor >= end {
                    if self.handoff(base + pos as i64, allow_handoff) {
                        continue;
                    }
                    break;
                }
                (remaining as i64).min(end - self.cursor) as usize
            } else {
                if self.starved {
                    if available - self.cursor < RESUME_FRAMES as i64 {
                        self.buffering = true;
                        return (pos, true);
                    }
                    self.end_starvation(&source, available);
                }
                let limit = available - RESERVE_FRAMES as i64;
                if self.cursor + remaining as i64 <= limit {
                    remaining
                } else if pos == 0 && self.gain > 0.0 && self.cursor + remaining as i64 <= available {
                    self.begin_starvation(&source, available); // last reserve block, faded out by the caller
                    self.read_into(block, pos, &source, remaining);
                    self.buffering = true;
                    return (BLOCK_FRAMES, true);
                } else {
                    self.begin_starvation(&source, available);
                    self.buffering = true;
                    return (pos, true);
                }
            };
            let got = self.read_into(block, pos, &source, count);
            if got == 0 {
                if done {
                    // the file is shorter than announced: finish here
                    if self.handoff(base + pos as i64, allow_handoff) {
                        continue;
                    }
                    break;
                }
                self.begin_starvation(&source, available);
                self.buffering = true;
                return (pos, true);
            }
            pos += got;
        }
        (pos, false)
    }

    /// Cap `frames_available()` by the inode we read; log a replaced/shrunk file once.
    fn checked_available(&mut self, source: &Source, mut available: i64, done: bool) -> i64 {
        let index = self.reader_index(source);
        let reader = &mut self.readers[index];
        if let Some(file_frames) = reader.file_frames()
            && file_frames < available
        {
            if !reader.replaced_logged {
                reader.replaced_logged = true;
                self.pending_log.push(format!(
                    "source {}: file reports {available} frames but the opened inode holds {file_frames} \
                     (replaced under us); reading our inode",
                    source_label(Some(&**source))
                ));
            }
            available = file_frames;
        }
        if done {
            if available < reader.max_available && !reader.shrink_logged {
                reader.shrink_logged = true;
                self.pending_log.push(format!(
                    "source {}: complete file shrank from {} to {available} frames",
                    source_label(Some(&**source)),
                    reader.max_available
                ));
            }
            reader.max_available = reader.max_available.max(available);
        }
        available
    }

    fn lead_ms(&self, available: i64) -> f64 {
        (available - self.cursor) as f64 / f64::from(SAMPLE_RATE) * 1000.0
    }

    fn begin_starvation(&mut self, source: &Source, available: i64) {
        if self.starved {
            return;
        }
        self.starved = true;
        self.underruns += 1;
        self.pending_log.push(format!(
            "starvation start: {} cursor {} available {available} (lead {:.0} ms), episode {}",
            source_label(Some(&**source)),
            self.cursor,
            self.lead_ms(available),
            self.underruns
        ));
    }

    fn end_starvation(&mut self, source: &Source, available: i64) {
        self.starved = false;
        self.pending_log.push(format!(
            "starvation end: {} cursor {} available {available} (lead {:.0} ms)",
            source_label(Some(&**source)),
            self.cursor,
            self.lead_ms(available)
        ));
    }

    /// Copy up to `count` frames at the cursor into the block from frame `pos` on; returns the
    /// frames copied.
    fn read_into(&mut self, block: &mut [f32], pos: usize, source: &Source, count: usize) -> usize {
        let index = self.reader_index(source);
        let got = self.readers[index].read(self.cursor, count);
        if got == 0 {
            return 0;
        }
        if self.start_pending {
            let stream_frame = self.frames_written + pos as i64;
            self.events.push_back(QueuedEvent { stream_frame, kind: EventKind::Started, source: source.clone() });
            self.start_pending = false;
        }
        self.note_segment(self.frames_written + pos as i64, self.cursor, true);
        let reader = &self.readers[index];
        let channels = reader.channels.min(CHANNELS);
        let frames = reader.bytes[..got * reader.frame_bytes].chunks_exact(reader.frame_bytes);
        for (out, frame) in block[pos * CHANNELS..].as_chunks_mut::<CHANNELS>().0.iter_mut().zip(frames) {
            for (sample, bytes) in out[..channels].iter_mut().zip(frame.as_chunks::<4>().0) {
                *sample = f32::from_le_bytes(*bytes);
            }
        }
        self.cursor += got as i64;
        self.buffering = false;
        got
    }

    /// The active source is exhausted at `stream_frame`: queue its ended event, start the next
    /// one.
    fn handoff(&mut self, stream_frame: i64, allow_handoff: bool) -> bool {
        if !self.ended {
            self.ended = true;
            if let Some(source) = self.source.clone() {
                self.events.push_back(QueuedEvent { stream_frame, kind: EventKind::Ended, source });
            }
        }
        if !allow_handoff {
            return false;
        }
        let Some(next) = self.next_source.take() else { return false };
        self.pending_log.push(format!(
            "handoff {} -> {} at stream frame {stream_frame} (cursor {})",
            source_label(self.source.as_deref()),
            source_label(Some(&*next)),
            self.cursor
        ));
        self.source = Some(next);
        self.cursor = 0;
        self.ended = false;
        self.starved = false;
        self.start_pending = true;
        self.handoffs += 1;
        self.generation += 1;
        self.content_generation = self.generation;
        self.jump_frame = 0;
        self.last_position = None;
        self.prune_readers();
        true
    }
}

/// A volume within 0..1 (not a number: silence).
pub(super) fn unit_gain(volume: f64) -> f64 {
    if volume.is_nan() { 0.0 } else { volume.clamp(0.0, 1.0) }
}

/// The block times a linear ramp from `start` to `end` (numpy's `linspace(start, end,
/// BLOCK_FRAMES, endpoint=False)` as float32, one value per frame).
pub(super) fn apply_gain(block: &mut [f32], start: f64, end: f64) {
    if start == end {
        if start != 1.0 {
            let gain = start as f32;
            block.iter_mut().for_each(|sample| *sample *= gain);
        }
    } else {
        let delta = end - start;
        let step = delta / BLOCK_FRAMES as f64;
        for (index, frame) in block.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let gain = if step == 0.0 {
                index as f64 / BLOCK_FRAMES as f64 * delta + start // a difference too small to divide
            } else {
                index as f64 * step + start
            } as f32;
            frame.iter_mut().for_each(|sample| *sample *= gain);
        }
    }
    // No hard clip: the chain is float and the downmix preamp provides the headroom. Decoded
    // AAC regularly peaks above full scale (1.1-1.3 on loud masters); clipping it here produced
    // audible distortion. Only non-finite samples are removed.
    for sample in block.iter_mut() {
        if !sample.is_finite() {
            *sample = 0.0;
        }
    }
}
