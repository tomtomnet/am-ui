//! `PipeWireOutput` against a paced stand-in consumer (no audio server needed): the tests of
//! `tests/test_output.py`, with the same consumer, now fed the stereo stream out of the chain.
//!
//! The stand-in is the small Python program of the Python tests on the other end of the stdin
//! pipe (so `python3` has to be installed). It behaves like a player: it reads one quantum (1024
//! stereo frames) per cycle, "plays" it for the quantum's duration and records every frame it
//! consumed to a file with a timestamped log of its progress. The output is told that it plays
//! what it read [`CONSUMER_DELAY_MS`] later. Where a test looks at what the consumer received,
//! the chain passes FL to the left and FR to the right unchanged (`front_pair()`), lets every
//! channel through (`every_channel()`), or weighs each channel differently and the test compares
//! with what `dsp::Chain` makes of the 16 channels (`weights()`).
//!
//! These tests live in their own test program: they measure time, and the unit tests of the
//! library (loudness above all) keep every processor busy. They are written to stand a busy
//! machine: what a test reads is compared with the consumer's log for the time before and after
//! the reading, and a state that needs real time to arrive is waited for (`settle`) after the
//! pause the Python test makes. A machine that cannot run the consumer in real time (hundreds of
//! runnable processes) fails them, as it fails the Python tests; a note in the test's output
//! says when that was the case.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::sleep;
use std::time::Duration;

use amui::config::{Config, SAMPLE_RATE};
use amui::dsp;
use amui::output::{
    BLOCK_FRAMES, KeepPriority, OUT_FRAME_BYTES, OutputError, OutputOptions, PIPE_BYTES, PipeWireOutput, PlaybackLog,
    RESUME_FRAMES,
};
use amui::render::PcmSource;

const RATE: usize = SAMPLE_RATE as usize;
/// The stand-in consumer's cycle (21.3 ms).
const QUANTUM: usize = 1024;
/// It plays what it read about a quantum later: what the output is told to assume.
const CONSUMER_DELAY_MS: u32 = 21;
/// Frame indexes encoded as index / 2^24 are exact in float32.
const INDEX_SCALE: f64 = 16_777_216.0;

const CONSUMER: &str = r#"
import os, sys, time
quantum, out_path, log_path, mode = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
rate, nbytes, fd, frames, start = 48000, int(sys.argv[1]) * 8, 0, 0, None
with open(out_path, "wb") as out, open(log_path, "w") as log:
    while True:
        if mode == "die" and frames >= quantum * 40:  # ~0.85 s in: past the output's start grace
            os._exit(3)
        buf = bytearray()
        while len(buf) < nbytes:
            chunk = os.read(fd, nbytes - len(buf))
            if not chunk:
                break
            buf += chunk
        if not buf:
            break
        now = time.monotonic()
        if start is None:
            start = now
        out.write(buf)
        frames += len(buf) // 8
        log.write("%.6f %d\n" % (now, frames))
        log.flush()
        if len(buf) < nbytes:
            break
        delay = start + frames / rate - time.monotonic()
        if delay > 0:
            time.sleep(delay)
"#;

/// FL to the left and FR to the right, unchanged, and nothing else.
fn front_pair() -> dsp::Settings {
    dsp::Settings {
        gains: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        lfe: 0.0,
        lfe_lowpass: 1e6,
        lfe_q: [0.707, 0.707],
        lfe_delay: 0.0,
        preamp: 1.0,
        eq_gain: 1.0,
        bands: Vec::new(),
    }
}

/// Every channel of a side with gain 1: whatever is in a channel besides FL and FR shows.
fn every_channel() -> dsp::Settings {
    dsp::Settings { gains: [1.0; 8], ..front_pair() }
}

/// Another weight for every channel group and the LFE (unfiltered), so that a channel in the
/// wrong place changes what comes out.
fn weights() -> dsp::Settings {
    dsp::Settings {
        gains: [1.0, 0.5, 0.25, 0.125, 0.0625, 0.03125, 0.015625, 0.007_812_5],
        lfe: 0.003_906_25,
        ..front_pair()
    }
}

/// The clock the consumer stamps its log with (Python's `time.monotonic()`).
fn monotonic() -> f64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the pointer is valid for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as f64 + now.tv_nsec as f64 * 1e-9
}

fn seconds(duration: f64) {
    sleep(Duration::from_secs_f64(duration));
}

/// Time for a busy machine: true as soon as `reached()` holds, false when it does not within
/// five seconds. On a quiet machine the pause before it was enough and this returns at once.
fn settle(reached: impl Fn() -> bool) -> bool {
    let deadline = monotonic() + 5.0;
    while !reached() && monotonic() < deadline {
        seconds(0.01);
    }
    reached()
}

/// Say (in the output of a failing test) when the machine does not keep time.
fn note_a_busy_machine() {
    let begun = monotonic();
    for _ in 0..5 {
        seconds(0.002);
    }
    let took = (monotonic() - begun) * 1000.0;
    if took > 40.0 {
        eprintln!("note: this machine is busy (five 2 ms pauses took {took:.0} ms): timing may be off for that reason");
    }
}

fn f32le(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
}

/// Minimal `PcmSource` over a file this test wrote (complete, or growing while `done` is false).
struct FileSource {
    path: PathBuf,
    channels: usize,
    frames: AtomicU64,
    done: AtomicBool,
    total: AtomicU64,
    /// Milliseconds the next `frames_available()` takes: a source that holds the feeder up.
    stall_ms: AtomicU64,
}

impl PcmSource for FileSource {
    fn pcm_path(&self) -> &Path {
        &self.path
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn frames_available(&self) -> u64 {
        let stall = self.stall_ms.swap(0, Ordering::SeqCst);
        if stall > 0 {
            sleep(Duration::from_millis(stall));
        }
        self.frames.load(Ordering::SeqCst)
    }

    fn total_frames(&self) -> Option<u64> {
        Some(self.total.load(Ordering::SeqCst))
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn error(&self) -> Option<String> {
        None
    }
}

impl FileSource {
    fn grow(&self, data: &[f32], done: bool) {
        fs::OpenOptions::new().append(true).open(&self.path).unwrap().write_all(&f32le(data)).unwrap();
        let added = (data.len() / self.channels) as u64;
        let frames = self.frames.fetch_add(added, Ordering::SeqCst) + added;
        if done {
            self.total.store(frames, Ordering::SeqCst);
            self.done.store(true, Ordering::SeqCst);
        }
    }
}

/// The source as the output knows it.
fn dynamic(source: &Arc<FileSource>) -> Arc<dyn PcmSource> {
    source.clone()
}

/// Interleaved frames of `channels` channels, every sample `value`.
fn constant_signal(frames: usize, value: f32, channels: usize) -> Vec<f32> {
    vec![value; frames * channels]
}

/// 16 channels: FL carries the frame index (from `start` on), FR the source's id.
fn index_signal(frames: usize, source_id: f32, start: usize) -> Vec<f32> {
    let mut data = vec![0.0; frames * 16];
    for (index, frame) in data.as_chunks_mut::<16>().0.iter_mut().enumerate() {
        frame[0] = ((start + index) as f64 / INDEX_SCALE) as f32;
        frame[1] = source_id;
    }
    data
}

/// One side (0 left, 1 right) of interleaved stereo frames.
fn channel(frames: &[f32], channel: usize) -> Vec<f32> {
    frames.as_chunks::<2>().0.iter().map(|frame| frame[channel]).collect()
}

/// The frame indexes an `index_signal` stream carries.
fn indexes(frames: &[f32]) -> Vec<i64> {
    channel(frames, 0).iter().map(|&value| (f64::from(value) * INDEX_SCALE).round_ties_even() as i64).collect()
}

/// The values of `values` where `keep` holds.
fn select<T: Copy>(values: &[T], keep: &[bool]) -> Vec<T> {
    values.iter().zip(keep).filter(|(_, keep)| **keep).map(|(value, _)| *value).collect()
}

fn equals(samples: &[f32], value: f32) -> Vec<bool> {
    samples.iter().map(|&sample| sample == value).collect()
}

fn count(flags: &[bool]) -> usize {
    flags.iter().filter(|flag| **flag).count()
}

/// Largest step between neighbours.
fn max_step(samples: &[f32]) -> f64 {
    samples.windows(2).map(|pair| f64::from((pair[1] - pair[0]).abs())).fold(0.0, f64::max)
}

/// numpy's `isclose(samples, value, atol=1e-6)`.
fn is_close(sample: f32, value: f32) -> bool {
    f64::from((sample - value).abs()) <= 1e-6 + 1e-5 * f64::from(value.abs())
}

/// (start, length) of runs of exact zeros at least `minimum` long.
fn zero_runs(samples: &[f32], minimum: usize) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (index, &sample) in samples.iter().enumerate() {
        match (sample == 0.0, start) {
            (true, None) => start = Some(index),
            (false, Some(begun)) => {
                if index - begun >= minimum {
                    runs.push((begun, index - begun));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begun) = start
        && samples.len() - begun >= minimum
    {
        runs.push((begun, samples.len() - begun));
    }
    runs
}

#[track_caller]
fn assert_close(actual: f64, expected: f64, delta: f64) {
    assert!((actual - expected).abs() <= delta, "{actual} is not within {delta} of {expected}");
}

type Events = Arc<Mutex<Vec<(f64, &'static str, Arc<dyn PcmSource>)>>>;

struct Harness {
    /// First, so that it is stopped before the folder goes away.
    output: PipeWireOutput,
    events: Events,
    errors: Arc<Mutex<Vec<String>>>,
    playback_log: PathBuf,
    consumed_path: PathBuf,
    log_path: PathBuf,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Harness {
        Harness::with("run", front_pair())
    }

    fn with(mode: &str, dsp: dsp::Settings) -> Harness {
        note_a_busy_machine();
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new());
        let playback_log = dir.path().join("playback.log"); // the test's own playback log
        let consumed_path = dir.path().join("consumed.f32le");
        let log_path = dir.path().join("consumer.log");
        let command = vec![
            "python3".to_string(),
            "-c".to_string(),
            CONSUMER.to_string(),
            QUANTUM.to_string(),
            consumed_path.display().to_string(),
            log_path.display().to_string(),
            mode.to_string(),
        ];
        let options = OutputOptions {
            command: Some(command),
            log_path: Some(dir.path().join("consumer-output.log")),
            latency_ms: CONSUMER_DELAY_MS,
            dsp,
            priority: Arc::new(KeepPriority),
            ..OutputOptions::default()
        };
        let output = PipeWireOutput::new(&config, Arc::new(PlaybackLog::new(&playback_log)), options);
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        output.on_source_started(move |source| seen.lock().unwrap().push((monotonic(), "started", source)));
        let seen = events.clone();
        output.on_source_ended(move |source| seen.lock().unwrap().push((monotonic(), "ended", source)));
        let seen = errors.clone();
        output.on_error(move |message| seen.lock().unwrap().push(message.to_string()));
        Harness { output, events, errors, playback_log, consumed_path, log_path, dir }
    }

    fn file_source(
        &self,
        name: &str,
        data: &[f32],
        channels: usize,
        done: bool,
        total: Option<usize>,
    ) -> Arc<FileSource> {
        let path = self.dir.path().join(name);
        fs::write(&path, f32le(data)).unwrap();
        let frames = (data.len() / channels) as u64;
        Arc::new(FileSource {
            path,
            channels,
            frames: AtomicU64::new(frames),
            done: AtomicBool::new(done),
            total: AtomicU64::new(total.map_or(frames, |total| total as u64)),
            stall_ms: AtomicU64::new(0),
        })
    }

    /// A complete 16-channel source.
    fn source(&self, name: &str, data: &[f32]) -> Arc<FileSource> {
        self.file_source(name, data, 16, true, None)
    }

    /// Start `source` at its first frame.
    fn play(&self, source: &Arc<FileSource>) {
        self.output.set_source(Some(dynamic(source)), 0);
    }

    /// Every frame the consumer read, interleaved stereo.
    fn consumed(&self) -> Vec<f32> {
        let raw = fs::read(&self.consumed_path).unwrap();
        let samples: Vec<f32> = raw.as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect();
        samples[..samples.len() / 2 * 2].to_vec()
    }

    /// (time, frames consumed so far) after every quantum.
    fn consumer_log(&self) -> Vec<(f64, i64)> {
        let text = fs::read_to_string(&self.log_path).unwrap();
        text.lines()
            .map(|line| {
                let (stamp, frames) = line.split_once(' ').unwrap();
                (stamp.parse().unwrap(), frames.parse().unwrap())
            })
            .collect()
    }

    fn playback_log(&self) -> String {
        fs::read_to_string(&self.playback_log).unwrap()
    }

    fn wait_events(&self, count: usize, timeout: f64) {
        let deadline = monotonic() + timeout;
        while self.events.lock().unwrap().len() < count && monotonic() < deadline {
            seconds(0.01);
        }
    }

    fn event_kinds(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().iter().map(|(_, kind, _)| *kind).collect()
    }

    fn event_times(&self) -> Vec<f64> {
        self.events.lock().unwrap().iter().map(|(time, _, _)| *time).collect()
    }

    /// Whether the events were, in order, `expected` (kind, source).
    fn events_are(&self, expected: &[(&str, &Arc<FileSource>)]) -> bool {
        let events = self.events.lock().unwrap();
        events.len() == expected.len()
            && events
                .iter()
                .zip(expected)
                .all(|((_, kind, source), (wanted, of))| kind == wanted && Arc::ptr_eq(source, &dynamic(of)))
    }

    fn wait_while_buffering(&self, wanted: bool, timeout: f64) {
        let deadline = monotonic() + timeout;
        while self.output.buffering() != wanted && monotonic() < deadline {
            seconds(0.01);
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        note_a_busy_machine(); // also at the end: what a failing test prints says how the machine was
    }
}

/// Frame the stand-in consumer was playing at time `at` (it plays each quantum right after
/// reading it).
fn consumer_audible(log: &[(f64, i64)], at: f64) -> Option<f64> {
    let (stamp, frames) = *log.iter().take_while(|(stamp, _)| *stamp <= at).last()?;
    Some((frames as f64).min(frames as f64 - QUANTUM as f64 + (at - stamp) * RATE as f64))
}

#[test]
fn pacing_and_position_accuracy() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 4, 0.5, 16));
    h.output.start().unwrap();
    assert_eq!(h.output.pipe_bytes(), PIPE_BYTES);
    assert_eq!(PIPE_BYTES / OUT_FRAME_BYTES, 8192, "170 ms of cushion against feeder stalls (see NOTES)");
    h.play(&src);
    let t0 = monotonic();
    // (time, position, time, frames written, time): each reading between two looks at the clock,
    // so that a test thread that was held up between them does not count against the output
    let mut samples = Vec::new();
    while monotonic() - t0 < 2.5 {
        seconds(0.05);
        let before = monotonic();
        let position = h.output.position_frames() as i64;
        let between = monotonic();
        let written = h.output.frames_written() as i64;
        samples.push((before, position, between, written, monotonic()));
    }
    h.output.stop();
    let log = h.consumer_log();
    assert!(log.len() > 50);
    // ~1 s of audio per wall second: the consumer never waited for the feeder
    let ((first_t, first_f), (last_t, last_f)) = (log[0], log[log.len() - 1]);
    let rate = (last_f - first_f) as f64 / (last_t - first_t);
    assert_close(rate / RATE as f64, 1.0, 0.03);
    // everything written was consumed (stop() drains: the final fade block included)
    assert_eq!(h.output.frames_written() as i64, last_f);
    // the feeder stays ahead by the pipe at most (plus the block in flight and the consumer's quantum)
    let consumed_by = |at: f64| log.iter().rev().find(|(stamp, _)| *stamp <= at).map_or(0, |(_, frames)| *frames);
    for &(_, _, before, written, after) in &samples {
        let pipe_frames = h.output.pipe_bytes() / OUT_FRAME_BYTES;
        assert!(written - consumed_by(after) <= (pipe_frames + BLOCK_FRAMES + QUANTUM) as i64);
        assert!(written - consumed_by(before) >= 0);
    }
    // a pipe says nothing about its reader: the delay is the one the output was told
    assert_eq!(h.output.delay_frames(), u64::from(CONSUMER_DELAY_MS) * 48);
    // position estimate vs the consumer's own clock, after the pipeline has settled
    // the consumer counts stream frames; the source began at the first non-silent stream frame
    let source_origin = channel(&h.consumed(), 0).iter().position(|&sample| sample != 0.0).unwrap_or(0);
    assert!(source_origin > 0);
    let errors: Vec<f64> = samples
        .iter()
        .filter(|(before, ..)| before - t0 >= 0.4)
        .filter_map(|&(before, estimate, after, ..)| {
            // the consumer's position when the estimate was made: between these two
            let (earliest, latest) = (consumer_audible(&log, before)?, consumer_audible(&log, after)?);
            let estimate = estimate as f64 + source_origin as f64;
            Some((estimate - estimate.clamp(earliest, latest.max(earliest))) / RATE as f64)
        })
        .collect();
    assert!(errors.len() > 20);
    let worst = errors.iter().fold(0.0f64, |worst, error| worst.max(error.abs()));
    assert!(worst < 0.100, "position errors (s): {errors:?}");
    println!(
        "position error vs consumer clock: mean {:+.1} ms, max |err| {:.1} ms over {} samples",
        errors.iter().sum::<f64>() / errors.len() as f64 * 1000.0,
        worst * 1000.0,
        errors.len()
    );
    let values: Vec<i64> = samples.iter().map(|&(_, position, ..)| position).collect();
    assert!(values.is_sorted(), "position must be monotonic within a segment");
    assert!(values[values.len() - 1] > 2 * RATE as i64);
}

#[test]
fn silence_while_paused() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 4, 0.5, 16));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.4);
    h.output.pause();
    assert!(h.output.paused());
    seconds(0.3);
    settle(|| h.output.position_frames() == h.output.cursor_frame()); // the pause is audible
    let held_a = h.output.position_frames();
    seconds(0.2);
    let held_b = h.output.position_frames();
    assert_eq!(held_a, held_b, "position must hold while paused");
    h.output.resume();
    assert!(!h.output.paused());
    seconds(0.4);
    h.output.stop();
    let ch0 = channel(&h.consumed(), 0);
    let runs: Vec<(usize, usize)> =
        zero_runs(&ch0, (0.35 * RATE as f64) as usize).into_iter().filter(|(start, _)| *start > RATE / 5).collect();
    assert_eq!(runs.len(), 1, "expected one silent stretch for the pause, got {runs:?}");
    let (start, length) = runs[0];
    assert!(length < (0.75 * RATE as f64) as usize);
    // audio before and after the pause, at full level
    assert!(ch0[..start].contains(&0.5));
    assert!(ch0[start + length..].contains(&0.5), "audio must resume after the pause");
    // the pause and the resume ramp: no step larger than a 10 ms ramp allows
    assert!(max_step(&ch0) < 0.05);
    // the paused position corresponds to the stream frame where silence began
    assert!(held_a > (0.3 * RATE as f64) as u64);
    assert!(held_a < (0.8 * RATE as f64) as u64);
}

#[test]
fn stereo_expansion_lands_in_front_pair() {
    // every channel reaches a side: anything besides FL and FR would show
    let h = Harness::with("run", every_channel());
    let data: Vec<f32> = (0..RATE).flat_map(|_| [0.25, -0.25]).collect();
    let src = h.file_source("stereo.f32le", &data, 2, true, None);
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.6);
    h.output.stop();
    let frames = h.consumed();
    // gain exactly 1.0 (ramps excluded)
    let full: Vec<&[f32; 2]> = frames.as_chunks::<2>().0.iter().filter(|frame| frame[0] == 0.25).collect();
    assert!(full.len() > RATE / 5);
    assert!(full.iter().all(|frame| frame[1] == -0.25), "channels FC..TRR must stay silent for a stereo source");
    let unexpected = frames.as_chunks::<2>().0.iter().any(|frame| frame[0] != -frame[1]);
    assert!(!unexpected, "only FL and FR, always");
}

#[test]
fn volume_ramp_is_smooth() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 3, 0.5, 16));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.45);
    h.output.set_volume(0.3);
    seconds(0.45);
    h.output.set_volume(0.8);
    seconds(0.45);
    h.output.stop();
    let ch0 = channel(&h.consumed(), 0);
    assert!(ch0.len() > (1.2 * RATE as f64) as usize);
    let steps = max_step(&ch0);
    assert!(steps < 0.05);
    assert!(steps < 0.5 / BLOCK_FRAMES as f64 + 1e-4, "a 10 ms linear ramp: tiny per-sample steps");
    for level in [0.5f32, (0.5f64 * 0.3) as f32, (0.5f64 * 0.8) as f32] {
        assert!(ch0.contains(&level), "expected plateaus at 0.5, 0.15 and 0.4; none at {level}");
    }
    let tail = &ch0[ch0.len() - RATE / 4..ch0.len() - BLOCK_FRAMES * 2];
    assert!(tail.iter().all(|&sample| sample == 0.4));
}

#[test]
fn seek_maps_segments() {
    let h = Harness::new();
    let src = h.source("index.f32le", &index_signal(RATE * 4, 0.25, 0));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.5);
    let before = h.output.position_frames();
    assert!(before < RATE as u64);
    let target = 2 * RATE as u64;
    let sought = monotonic();
    h.output.seek(target);
    let immediately = h.output.position_frames();
    assert!(immediately >= target, "a seek reports the new position at once");
    assert!(immediately < target + RATE as u64 / 10);
    seconds(0.5);
    let later = h.output.position_frames();
    let overslept = ((monotonic() - sought - 0.5).max(0.0) * RATE as f64) as u64; // nothing on a quiet machine
    assert!(later > target + (0.25 * RATE as f64) as u64);
    assert!(later < target + (0.65 * RATE as f64) as u64 + overslept);
    h.output.stop();
    let frames = h.consumed();
    let exact = equals(&channel(&frames, 1), 0.25); // frames played at gain exactly 1.0
    let played = select(&indexes(&frames), &exact);
    let (rate, target) = (RATE as i64, target as i64);
    assert!(played.iter().any(|&index| index < rate / 2));
    let after: Vec<i64> = played.iter().copied().filter(|&index| index >= target).collect();
    assert!(after.len() > (0.3 * RATE as f64) as usize);
    assert!(after.windows(2).all(|pair| pair[1] - pair[0] == 1), "after the seek the source is read contiguously");
    assert_eq!(after[0], target + BLOCK_FRAMES as i64, "the first block after the jump is the fade-in");
    let skipped = played.iter().any(|&index| index >= rate / 2 + rate && index < target);
    assert!(!skipped, "no audio from the skipped part");
}

#[test]
fn gapless_pair_events_and_continuity() {
    let h = Harness::new();
    let a = h.source("a.f32le", &index_signal(RATE / 2, 0.25, 0));
    let b = h.source("b.f32le", &index_signal(RATE / 2, 0.5, 0));
    h.output.start().unwrap();
    h.play(&a);
    h.output.set_next_source(Some(dynamic(&b)));
    h.wait_events(4, 4.0);
    assert!(h.events_are(&[("started", &a), ("ended", &a), ("started", &b), ("ended", &b)]), "{:?}", h.event_kinds());
    let times = h.event_times();
    assert!(times[2] - times[1] < 0.05, "ended(A) and started(B) describe the same instant");
    assert_close(times[1] - times[0], 0.5, 0.15);
    assert_close(times[3] - times[2], 0.5, 0.15);
    assert!(Arc::ptr_eq(&h.output.active_source().unwrap(), &dynamic(&b))); // B stays the active (ended) source
    seconds(0.2);
    h.output.stop();
    let frames = h.consumed();
    let ids = channel(&frames, 1);
    let index = indexes(&frames);
    let first_b = ids.iter().position(|&id| id == 0.5).unwrap_or(0);
    assert!(first_b > 0);
    assert_eq!(ids[first_b - 1], 0.25, "A's last frame at full gain right before B");
    assert_eq!(index[first_b - 1], RATE as i64 / 2 - 1);
    assert_eq!(index[first_b], 0, "B starts at its frame 0 in the very next stream frame");
    let run_b = &index[first_b..first_b + RATE / 2];
    assert!(run_b.windows(2).all(|pair| pair[1] - pair[0] == 1));
}

#[test]
fn starvation_fades_and_recovers() {
    let h = Harness::new();
    let total = RATE * 2;
    let first = (0.3 * RATE as f64) as usize;
    let src = h.file_source("grow.f32le", &constant_signal(first, 0.5, 16), 16, false, Some(total));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.6);
    assert!(settle(|| h.output.buffering()), "cursor reached the end of the growing file");
    settle(|| h.output.position_frames() == h.output.cursor_frame()); // the silence is audible
    let held = h.output.position_frames();
    seconds(0.1);
    assert_eq!(h.output.position_frames(), held);
    assert!(held <= first as u64);
    src.grow(&constant_signal(total - first, 0.5, 16), true);
    h.wait_while_buffering(false, 1.0);
    assert!(!h.output.buffering());
    h.wait_events(2, 3.0);
    assert_eq!(h.event_kinds(), ["started", "ended"]);
    h.output.stop();
    let ch0 = channel(&h.consumed(), 0);
    let runs: Vec<(usize, usize)> =
        zero_runs(&ch0, RATE / 5).into_iter().filter(|(start, _)| *start > RATE / 10 && *start < RATE).collect();
    assert_eq!(runs.len(), 1, "one silent stretch while starved, got {runs:?}");
    // the constant signal ends with a hard cut
    let last_audio = ch0.iter().rposition(|&sample| sample == 0.5).unwrap();
    assert!(max_step(&ch0[..last_audio]) < 0.05, "starvation fades out and in");
    // every source frame was played exactly once: 0.5-level frames cover the whole file minus the ramps
    let full = count(&equals(&ch0, 0.5));
    assert!(full > total - 6 * BLOCK_FRAMES);
    assert!(full <= total);
}

/// A render head hovering just ahead of the cursor must not gate the sound on and off: after
/// starving, playback resumes only with `RESUME_FRAMES` (0.5 s) beyond the cursor.
#[test]
fn starvation_hysteresis_waits_for_resume_seconds() {
    let h = Harness::new();
    let total = RATE * 3;
    let first = (0.3 * RATE as f64) as usize;
    let dribble = (0.1 * RATE as f64) as usize;
    let src = h.file_source("grow.f32le", &index_signal(first, 0.25, 0), 16, false, Some(total));
    h.output.start().unwrap();
    h.play(&src);
    h.wait_while_buffering(true, 2.0);
    assert!(h.output.buffering());
    assert_eq!(h.output.underruns(), 1);
    // feed it in dribbles of 0.1 s: four times, well above the old 20 ms resume threshold
    for step in 0..4 {
        src.grow(&index_signal(dribble, 0.25, first + step * dribble), false);
        seconds(0.15);
        assert!(h.output.buffering(), "still waiting after {} ms of new audio", (step + 1) * 100);
    }
    assert_eq!(h.output.underruns(), 1, "one episode, not one per dribble");
    let fed = (0.7 * RATE as f64) as usize;
    src.grow(&index_signal(total - fed, 0.25, fed), true);
    h.wait_while_buffering(false, 1.0);
    assert!(!h.output.buffering());
    h.wait_events(2, 4.0);
    h.output.stop();
    let frames = h.consumed();
    let exact = equals(&channel(&frames, 1), 0.25);
    let played = select(&indexes(&frames), &exact);
    assert!(played.windows(2).all(|pair| pair[1] >= pair[0]), "never goes back");
    assert_eq!(played[played.len() - 1], total as i64 - 1);
    let gaps = played.windows(2).filter(|pair| pair[1] - pair[0] > 1).count();
    assert_eq!(gaps, 1, "exactly one gap in the exact-gain frames (the faded blocks)");
    let ch0 = channel(&frames, 0);
    let last_audio = exact.iter().rposition(|&flag| flag).unwrap();
    let runs: Vec<(usize, usize)> =
        zero_runs(&ch0, RATE / 5).into_iter().filter(|(start, _)| RATE / 10 < *start && *start < last_audio).collect();
    assert_eq!(runs.len(), 1, "a single silent stretch for the whole episode, got {runs:?}");
    let log = h.playback_log();
    assert!(log.contains("starvation start"));
    assert_eq!(log.matches("starvation start").count(), 1);
    assert_eq!(log.matches("starvation end").count(), 1);
    const { assert!(RESUME_FRAMES <= RATE / 2) };
}

#[test]
fn handoff_counter_and_no_restart() {
    let h = Harness::new();
    let a = h.source("a.f32le", &index_signal(RATE / 2, 0.25, 0));
    let b = h.source("b.f32le", &index_signal(RATE, 0.5, 0));
    h.output.start().unwrap();
    assert_eq!(h.output.handoffs(), 0);
    h.play(&a);
    h.output.set_next_source(Some(dynamic(&b)));
    h.wait_events(3, 4.0);
    assert_eq!(h.output.handoffs(), 1, "the player relies on this counter to recognise a gapless continuation");
    assert!(Arc::ptr_eq(&h.output.active_source().unwrap(), &dynamic(&b)));
    seconds(0.3);
    h.output.stop();
    assert!(h.playback_log().contains("handoff"));
    let frames = h.consumed();
    let run_b = select(&indexes(&frames), &equals(&channel(&frames, 1), 0.5));
    let straight = run_b.windows(2).all(|pair| pair[1] - pair[0] == 1);
    assert!(straight, "B plays straight through: no fade-out/restart after the handoff");
}

#[test]
fn two_to_sixteen_channel_handoffs_place_channels_and_leave_no_stale_data() {
    // every channel weighs differently on its side: what B gives shows where its channels went
    let h = Harness::with("run", weights());
    let level = |channel: usize| (0.1 * (channel + 1) as f64) as f32; // every channel carries its own level
    let stereo_a: Vec<f32> = (0..RATE / 2).flat_map(|_| [0.25, -0.25]).collect();
    let wide: Vec<f32> = (0..RATE / 2).flat_map(|_| (0..16).map(level)).collect();
    // above A's level: no fade ramp of A can produce it
    let stereo_c: Vec<f32> = (0..RATE / 2).flat_map(|_| [0.3, -0.3]).collect();
    let a = h.file_source("a.f32le", &stereo_a, 2, true, None);
    let b = h.source("b.f32le", &wide);
    let c = h.file_source("c.f32le", &stereo_c, 2, true, None);
    h.output.start().unwrap();
    h.play(&a);
    h.output.set_next_source(Some(dynamic(&b)));
    h.wait_events(3, 4.0); // started(a), ended(a), started(b)
    h.output.set_next_source(Some(dynamic(&c)));
    h.wait_events(6, 4.0);
    seconds(0.2);
    h.output.stop();
    assert_eq!(h.output.handoffs(), 2);
    // B's frame through the chain: each channel with its own weight
    let mut mixed = [0.0f32; 2];
    let b_frame: Vec<f32> = (0..16).map(level).collect();
    dsp::Chain::new(weights(), SAMPLE_RATE).process(&b_frame, &mut mixed);
    let consumed = h.consumed();
    let frames = consumed.as_chunks::<2>().0;
    let in_b: Vec<bool> = frames.iter().map(|frame| *frame == mixed).collect();
    assert_eq!(count(&in_b), RATE / 2, "every frame of B exactly once, each channel in its place");
    let first_b = in_b.iter().position(|&flag| flag).unwrap();
    let last_b = in_b.iter().rposition(|&flag| flag).unwrap();
    assert_eq!(last_b - first_b + 1, RATE / 2, "B is one contiguous run");
    // the stereo sources come out as they went in: anything left of B would change them
    let in_a: Vec<bool> =
        frames.iter().enumerate().map(|(at, frame)| *frame == [0.25, -0.25] && at < first_b).collect();
    let in_c: Vec<bool> = frames.iter().enumerate().map(|(at, frame)| *frame == [0.3, -0.3] && at > last_b).collect();
    assert!(count(&in_a) > (0.4 * RATE as f64) as usize, "2-channel A: FC..TRR silent");
    assert!(count(&in_c) > (0.4 * RATE as f64) as usize, "2-channel C after the 16-channel B: no stale channels");
    // the order in the stream is A, B, C with nothing in between but the handoff frames themselves
    let last_a = in_a.iter().rposition(|&flag| flag).unwrap();
    let first_c = in_c.iter().position(|&flag| flag).unwrap();
    assert_eq!(first_b, last_a + 1);
    assert_eq!(first_c, last_b + 1);
}

#[test]
fn replaced_source_file_keeps_being_read_from_the_opened_inode() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &index_signal(RATE * 2, 0.25, 0));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.3);
    // a cache race: the directory entry is replaced by a short, different file (and a shorter announced size)
    let replacement = h.dir.path().join("tone.f32le.new");
    fs::write(&replacement, f32le(&index_signal(RATE / 10, 0.75, 0))).unwrap();
    fs::rename(&replacement, &src.path).unwrap();
    seconds(0.5);
    h.wait_events(2, 4.0);
    h.output.stop();
    let frames = h.consumed();
    let ids = channel(&frames, 1);
    assert!(!ids.contains(&0.75), "no sample of the replacement file was played");
    let played = select(&indexes(&frames), &equals(&ids, 0.25));
    let contiguous = played.windows(2).all(|pair| pair[1] - pair[0] == 1);
    assert!(contiguous, "the original file played contiguously to its end");
    assert_eq!(played[played.len() - 1], RATE as i64 * 2 - 1);
}

#[test]
fn recent_audio_ring_buffer_dump() {
    // the engine's downmix (PEQdB at 0 dB)
    let h = Harness::with("run", amui::output::default_dsp());
    let mut data = vec![0.0f32; RATE * 16];
    for (index, frame) in data.as_chunks_mut::<16>().0.iter_mut().enumerate() {
        // FL ramp (numpy's linspace(0, 0.5, RATE) as float32)
        frame[0] = if index == RATE - 1 { 0.5 } else { (index as f64 * (0.5 / (RATE - 1) as f64)) as f32 };
        frame[2] = 0.2; // FC: lands in both sides at -3 dB in the downmix
        frame[4] = 0.1; // SL: left only
    }
    let src = h.source("ramp.f32le", &data);
    h.output.start().unwrap();
    h.play(&src);
    h.wait_events(2, 4.0);
    seconds(0.1);
    let path = h.output.dump_recent(&h.dir.path().join("recent.wav")).unwrap();
    assert!(path.is_file());
    let raw = fs::read(&path).unwrap();
    assert_eq!(&raw[..4], b"RIFF");
    let data_at = raw.windows(4).position(|window| window == b"data").unwrap();
    let dumped: Vec<[f32; 2]> = raw[data_at + 8..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|frame| [0, 4].map(|at| f32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]])))
        .collect();
    h.output.stop();
    let consumed = h.consumed();
    let consumed = consumed.as_chunks::<2>().0;
    // under 30 s of stream: the ring holds everything from stream frame 0 up to the dump, and the
    // dump is exactly the stereo stream that was written (the consumer saw the same bytes)
    assert!(dumped.len() <= consumed.len());
    assert!(dumped.len() > RATE);
    let expected = &consumed[..dumped.len()];
    assert!(dumped.iter().zip(expected).all(|(got, want)| got == want));
    let active = dumped.iter().filter(|frame| frame[0].abs() > 0.0).count();
    assert!(active > (0.9 * RATE as f64) as usize);
    // FL ramp + SL 0.1 are left-only, FC is shared
    let left_minus_right = expected.iter().map(|frame| frame[0] - frame[1]).fold(f32::MIN, f32::max);
    assert!(left_minus_right > 0.5);
    assert!(h.playback_log().contains("dump_recent"));
}

#[test]
fn feeder_gap_monitor_logs_a_stall() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 4, 0.5, 16));
    h.output.start().unwrap();
    h.play(&src);
    seconds(0.5);
    // Hold the feeder off for 60 ms. The Python test does it by holding the output's lock (what
    // a long hold of the interpreter lock by another thread does); here the source takes that
    // long to answer, which keeps the feeder inside its lock for as long.
    src.stall_ms.store(60, Ordering::SeqCst);
    seconds(0.06);
    seconds(1.3); // the monitor reports per completed second
    assert!(settle(|| h.output.feeder_gap_ms() > 0.0));
    h.output.stop();
    let log = h.playback_log();
    assert!(log.contains("feeder: max gap"));
    let reported = log.split("max gap ").nth(1).and_then(|rest| rest.split(" ms").next()).unwrap();
    assert!(reported.parse::<f64>().unwrap() > 40.0);
    assert_eq!(h.output.pipe_low_count(), 0, "60 ms is within the 170 ms pipe cushion");
}

#[test]
fn sound_check_trim_ramps_over_50_ms_and_is_attenuation_only() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 3, 0.5, 16));
    h.output.start().unwrap();
    h.output.set_trim(&dynamic(&src), -6.0, amui::output::TRIM_RAMP_S);
    h.play(&src);
    seconds(0.5);
    settle(|| (h.output.trim_db() + 6.0).abs() < 0.0005);
    assert_close(h.output.trim_db(), -6.0, 0.0005);
    h.output.set_sound_check(false);
    seconds(0.4);
    settle(|| h.output.trim_db() == 0.0);
    assert_eq!(h.output.trim_db(), 0.0);
    h.output.set_trim(&dynamic(&src), 4.0, amui::output::TRIM_RAMP_S); // a boost request is clamped to 0 dB
    h.output.set_sound_check(true);
    seconds(0.4);
    assert_eq!(h.output.trim_db(), 0.0);
    h.output.stop();
    let ch0 = channel(&h.consumed(), 0);
    let trimmed = (0.5 * 10f64.powf(-6.0 / 20.0)) as f32;
    let plateau = ch0.iter().filter(|&&sample| is_close(sample, trimmed)).count();
    assert!(plateau > (0.3 * RATE as f64) as usize, "the -6 dB plateau");
    assert!(count(&equals(&ch0, 0.5)) > (0.3 * RATE as f64) as usize, "the untrimmed plateau");
    let audible: Vec<f32> = ch0.iter().copied().filter(|&sample| sample != 0.0).collect();
    assert!(max_step(&audible) < 0.5 / BLOCK_FRAMES as f64 + 1e-4, "ramps only, never a step");
    // the trim ramp spans ~5 blocks: between the two plateaus there are at least 4 blocks of intermediate values
    let between = ch0.iter().filter(|&&sample| sample > trimmed + 1e-4 && sample < 0.5 - 1e-4).count();
    assert!(between >= 4 * BLOCK_FRAMES - 10);
}

#[test]
fn trim_refinement_ramps_over_the_requested_time() {
    let h = Harness::new();
    let src = h.source("tone.f32le", &constant_signal(RATE * 3, 0.5, 16));
    h.output.start().unwrap();
    h.output.set_trim(&dynamic(&src), -2.0, amui::output::TRIM_RAMP_S);
    h.play(&src);
    seconds(0.3);
    h.output.set_trim(&dynamic(&src), -8.0, 0.5); // a refined measurement while the source plays: 50 blocks
    seconds(0.9);
    settle(|| (h.output.trim_db() + 8.0).abs() < 0.0005);
    assert_close(h.output.trim_db(), -8.0, 0.0005);
    h.output.set_sound_check(false); // toggling is quick again (50 ms), not the source's slow ramp
    seconds(0.3);
    settle(|| h.output.trim_db() == 0.0);
    assert_eq!(h.output.trim_db(), 0.0);
    h.output.stop();
    let ch0 = channel(&h.consumed(), 0);
    let low = (0.5 * 10f64.powf(-8.0 / 20.0)) as f32;
    let high = (0.5 * 10f64.powf(-2.0 / 20.0)) as f32;
    let between: Vec<usize> = (0..ch0.len()).filter(|&at| ch0[at] > low + 1e-4 && ch0[at] < high - 1e-4).collect();
    assert!(between.len() >= 45 * BLOCK_FRAMES, "the -2 -> -8 dB change spans ~50 blocks");
    assert!(between.len() <= 62 * BLOCK_FRAMES);
    assert!(
        ch0.iter().filter(|&&sample| is_close(sample, low)).count() > (0.3 * RATE as f64) as usize,
        "the -8 dB plateau"
    );
    // the Sound Check toggle back to 0 dB took ~5 blocks: few samples between the plateau and 0.5
    let last_between = between[between.len() - 1];
    let back = (last_between + 1..ch0.len()).filter(|&at| ch0[at] > low + 1e-4 && ch0[at] < 0.5 - 1e-4).count();
    assert!(back <= 8 * BLOCK_FRAMES);
}

#[test]
fn consumer_death_reports_error() {
    let h = Harness::with("die", front_pair());
    let src = h.source("tone.f32le", &constant_signal(RATE * 2, 0.5, 16));
    h.output.start().unwrap();
    h.play(&src);
    let deadline = monotonic() + 3.0;
    while h.errors.lock().unwrap().is_empty() && monotonic() < deadline {
        seconds(0.02);
    }
    let errors = h.errors.lock().unwrap().clone();
    assert_eq!(errors.len(), 1);
    assert!(!h.output.alive());
    assert!(format!("{} exited", errors[0]).contains("exited"));
    assert_eq!(h.output.error().as_deref(), Some(errors[0].as_str()));
    assert!(errors[0].starts_with("output pipe closed ("), "{}", errors[0]);
    h.output.stop(); // idempotent after a failure
}

#[test]
fn start_failure_raises_with_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new());
    // the Python test runs a Python one-liner; a shell is gone sooner, also on a busy machine
    let options = OutputOptions {
        command: Some(["sh", "-c", "echo boom >&2; exit 7"].map(String::from).to_vec()),
        log_path: Some(dir.path().join("consumer-output.log")),
        priority: Arc::new(KeepPriority),
        ..OutputOptions::default()
    };
    let output = PipeWireOutput::new(&config, Arc::new(PlaybackLog::new(dir.path().join("playback.log"))), options);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let seen = errors.clone();
    output.on_error(move |message| seen.lock().unwrap().push(message.to_string()));
    let raised = output.start().unwrap_err();
    assert!(raised.to_string().contains("status 7"), "{raised}");
    assert!(raised.to_string().contains("boom"), "{raised}");
    assert!(matches!(&raised, OutputError::Exited { status: 7, detail, .. } if detail == "boom"));
    assert!(!output.alive());
    assert!(errors.lock().unwrap().is_empty(), "start() fails instead of calling on_error");
    output.stop();
    assert!(fs::read_to_string(dir.path().join("consumer-output.log")).unwrap().contains("boom"));
}

// --- beyond the Python tests

/// The same output can be started again (the player does that after a failure): as in Python
/// the source that was active goes on where it was until the next command. A feeder that
/// `stop()` had to give up on, here one held inside a callback, belongs to the run it was
/// started for: when it wakes during the next run it takes no block of the new stream and does
/// not report its broken pipe as the new run's end.
#[test]
fn restart_leaves_a_stuck_feeder_behind() {
    let h = Harness::new();
    let first = h.source("first.f32le", &index_signal(RATE, 0.25, 0));
    // the second source carries a mark of the other sign: no ramp of one looks like the other
    let second = h.source("second.f32le", &index_signal(RATE, -0.5, 0));
    // (the callback is in, the callback may return)
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let held = gate.clone();
    h.output.on_source_started(move |_| {
        let (state, changed) = &*held;
        let mut state = state.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    });
    h.output.start().unwrap();
    h.play(&first);
    {
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        while !state.0 {
            state = changed.wait(state).unwrap();
        }
    }
    h.output.stop(); // waits for the feeder, ends the consumer, waits again, goes on without it
    assert!(!h.output.alive());
    h.output.on_source_started(|_| {});
    h.output.start().unwrap();
    assert!(h.output.alive());
    h.play(&second);
    seconds(0.3);
    {
        let (state, changed) = &*gate;
        state.lock().unwrap().1 = true; // the feeder of the first run wakes up
        changed.notify_all();
    }
    seconds(0.4);
    assert!(h.output.alive(), "{:?}", h.output.error());
    assert!(h.errors.lock().unwrap().is_empty());
    h.output.stop();
    let log = h.consumer_log();
    assert_eq!(h.output.frames_written() as i64, log[log.len() - 1].1, "the second run's stream, whole");
    let frames = h.consumed();
    let index = indexes(&frames);
    // the new run began with the first source, faded in where the first run had stopped reading
    let resumed = select(&index, &equals(&channel(&frames, 1), 0.25));
    assert!(resumed[0] > BLOCK_FRAMES as i64, "{}", resumed[0]);
    assert!(resumed.windows(2).all(|pair| pair[1] - pair[0] == 1));
    let played = select(&index, &equals(&channel(&frames, 1), -0.5));
    assert!(played.len() > RATE / 4);
    assert_eq!(played[0], BLOCK_FRAMES as i64, "the first block is the fade-in");
    assert!(played.windows(2).all(|pair| pair[1] - pair[0] == 1), "no block of the new stream went missing");
}

/// The `consumer_fd` seam: the output feeds the write end of a pipe the test reads, closes it
/// on `stop()`, and cannot be started on it a second time.
#[test]
fn injected_descriptor_is_fed_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new());
    let (mut reader, writer) = std::io::pipe().unwrap();
    let options = OutputOptions {
        consumer_fd: Some(OwnedFd::from(writer)),
        log_path: Some(dir.path().join("consumer-output.log")),
        volume: 0.5,
        dsp: every_channel(),
        priority: Arc::new(KeepPriority),
        ..OutputOptions::default()
    };
    let playback_log = dir.path().join("playback.log");
    let output = PipeWireOutput::new(&config, Arc::new(PlaybackLog::new(&playback_log)), options);
    let reading = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let path = dir.path().join("1234567890.stereo/stereo.f32le");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, f32le(&constant_signal(RATE / 10, 0.5, 2))).unwrap();
    let source: Arc<dyn PcmSource> = Arc::new(FileSource {
        path,
        channels: 2,
        frames: AtomicU64::new(RATE as u64 / 10),
        done: AtomicBool::new(true),
        total: AtomicU64::new(RATE as u64 / 10),
        stall_ms: AtomicU64::new(0),
    });
    let ended = Arc::new(AtomicBool::new(false));
    let seen = ended.clone();
    output.on_source_ended(move |_| seen.store(true, Ordering::SeqCst));
    output.start().unwrap();
    assert!(output.alive());
    output.start().unwrap(); // already started: nothing happens
    output.set_source(Some(source.clone()), 0);
    assert_eq!(output.buffered_frames(), RATE as u64 / 10);
    let deadline = monotonic() + 5.0;
    while !ended.load(Ordering::SeqCst) && monotonic() < deadline {
        seconds(0.005);
    }
    assert!(ended.load(Ordering::SeqCst));
    assert_eq!(output.position_frames(), RATE as u64 / 10);
    assert_eq!(output.cursor_frame(), RATE as u64 / 10);
    assert!(output.audible_stream_frame() <= output.frames_written());
    output.stop();
    assert!(!output.alive());
    let bytes = reading.join().unwrap(); // the read end saw the end of the stream: stop() closed ours
    assert_eq!(bytes.len() as u64, output.frames_written() * OUT_FRAME_BYTES as u64);
    let frames: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|sample| f32::from_le_bytes(*sample)).collect();
    let frames = frames.as_chunks::<2>().0;
    // the source at the output's volume on both sides, every frame after the fade-in exactly once
    let played = frames.iter().filter(|frame| frame[0] == 0.25 && frame[1] == 0.25).count();
    assert_eq!(played, RATE / 10 - BLOCK_FRAMES);
    assert!(frames.iter().all(|frame| frame[0] == frame[1]), "FL and FR only (every channel reaches a side)");
    let error = output.start().unwrap_err();
    assert!(matches!(error, OutputError::Other(_)), "{error}");
    assert!(!output.alive());
    let log = fs::read_to_string(&playback_log).unwrap();
    assert!(log.contains(" set_source 1234567890.stereo @ 0\n"), "{log}");
    assert!(log.contains("output: stopped (underruns 0, handoffs 0, pipe low "), "{log}");
    let own = fs::read_to_string(dir.path().join("consumer-output.log")).unwrap();
    assert!(own.contains("output: writing to injected fd "));
}
