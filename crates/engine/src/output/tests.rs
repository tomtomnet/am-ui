//! The parts that need no consumer, the feeder's 16-channel blocks against the Python feeder's,
//! the chain inside the output, and corks, rewrites, flushes and positions with a stand-in consumer that
//! plays when the test says; the running output is tested against a stand-in consumer program in
//! `tests/output.rs` and against a private audio server in `tests/output_audio.rs`.

use super::consumer::Clock;
use super::feeder::{Composer, Corked, Feeder, Flow, Mode};
use super::log::timestamp;
use super::state::{EventKind, apply_gain};
use super::*;
use crate::config::CHANNELS;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;

struct Named(PathBuf);

impl PcmSource for Named {
    fn pcm_path(&self) -> &Path {
        &self.0
    }

    fn channels(&self) -> usize {
        2
    }

    fn frames_available(&self) -> u64 {
        0
    }

    fn total_frames(&self) -> Option<u64> {
        None
    }

    fn done(&self) -> bool {
        true
    }

    fn error(&self) -> Option<String> {
        None
    }
}

#[test]
fn constants_are_the_python_module_s() {
    // a block is still 480 frames; what it weighs and the pipe are stereo now, the pipe holding
    // the same 8192 frames (~170 ms) as the 512 KiB of 16 channels did
    assert_eq!((BLOCK_FRAMES, BLOCK_BYTES, PIPE_BYTES), (480, 3840, 65536));
    assert_eq!(PIPE_BYTES / OUT_FRAME_BYTES, 8192);
    assert_eq!((RESERVE_FRAMES, RESUME_FRAMES, TRIM_RAMP_BLOCKS, RING_FRAMES), (480, 24000, 5, 1_440_000));
    assert_eq!((TRIM_RAMP_S, RESUME_SECONDS, RING_SECONDS), (0.05, 0.5, 30.0));
    assert_eq!((FEEDER_GAP_WARN_MS, START_GRACE_S, STOP_GRACE_S), (40.0, 0.5, 2.0));
    assert_eq!((LOG_MAX_BYTES, MAX_SEGMENT_AGE_FRAMES), (1048576, 480_000));
    assert_eq!((BUFFER_MS, IDLE_CORK_FRAMES, INTERRUPT_MIN_FRAMES, SENT_RING_FRAMES), (200, 96_000, 3008, 96_000));
    assert_eq!((APPLICATION_ID, STREAM_NAME, config::PW_APP_NAME), ("dev.amui.Music", "Music", "Apple Music"));
}

#[test]
fn source_labels_name_the_cache_directory() {
    let label = |path: &str| source_label(Some(&Named(PathBuf::from(path))));
    assert_eq!(source_label(None), "none");
    assert_eq!(label("/var/cache/pcm/1234567890.atmos/atmos-916.f32le"), "1234567890.atmos");
    assert_eq!(label("stereo.f32le"), "stereo.f32le");
    assert_eq!(label("/stereo.f32le"), "stereo.f32le");
    assert_eq!(label("1234567890.stereo/../x.f32le"), "..");
    assert_eq!(label(""), "");
}

#[test]
fn playback_log_is_private_stamped_and_rotates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs/playback.log");
    let log = PlaybackLog::new(&path);
    assert_eq!(log.path(), path);
    log.write("set_source 1234567890.atmos @ 0");
    log.write("pause 1234567890.atmos @ 480");
    let text = fs::read_to_string(&path).unwrap();
    let stamped = Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3} (.*)$").unwrap();
    let lines: Vec<&str> = text.lines().map(|line| stamped.captures(line).unwrap().get(1).unwrap().as_str()).collect();
    assert_eq!(lines, ["set_source 1234567890.atmos @ 0", "pause 1234567890.atmos @ 480"]);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);

    // a line that would take the file past the limit goes into a new file; the old one is kept as .1
    let filler = "x".repeat(1000);
    let mut written = fs::metadata(&path).unwrap().len();
    let per_line = (timestamp().len() + 1 + filler.len() + 1) as u64;
    while written + per_line <= LOG_MAX_BYTES {
        log.write(&filler);
        written += per_line;
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), written);
    assert!(!dir.path().join("logs/playback.log.1").exists());
    log.write(&filler);
    assert_eq!(fs::metadata(dir.path().join("logs/playback.log.1")).unwrap().len(), written);
    assert_eq!(fs::metadata(&path).unwrap().len(), per_line);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    // set_path() moves on to another file; close() is harmless and writing reopens
    let other = dir.path().join("other.log");
    log.set_path(&other);
    log.close();
    log.write("resume 1234567890.atmos @ 480");
    assert!(fs::read_to_string(&other).unwrap().ends_with(" resume 1234567890.atmos @ 480\n"));
    // a log that cannot be written is skipped
    let blocked = PlaybackLog::new(other.join("below-a-file/playback.log"));
    blocked.write("nowhere");
    assert_eq!(fs::read_to_string(&other).unwrap().lines().count(), 1);
}

#[test]
fn timestamps_are_local_iso_with_milliseconds() {
    let stamp = timestamp();
    assert!(Regex::new(r"^20\d\d-[01]\d-[0-3]\dT[0-2]\d:[0-5]\d:[0-6]\d\.\d{3}$").unwrap().is_match(&stamp), "{stamp}");
}

/// The ramp is numpy's `linspace(start, end, 480, endpoint=False, dtype=float32)` applied to
/// float32 samples: the bit patterns are from numpy (`tests/reference/gain_ramps.py`).
#[test]
fn gain_ramps_are_numpy_s() {
    let ramp = |start: f64, end: f64| {
        let mut block = vec![0.5f32; BLOCK_FRAMES * CHANNELS];
        apply_gain(&mut block, start, end);
        for frame in block.as_chunks::<CHANNELS>().0 {
            assert!(frame.iter().all(|sample| sample.to_bits() == frame[0].to_bits()));
        }
        [0, 1, 240, 479].map(|frame| block[frame * CHANNELS].to_bits())
    };
    assert_eq!(ramp(0.0, 0.8), [0x00000000, 0x3a5a740e, 0x3e4ccccd, 0x3ecc5f93]);
    assert_eq!(ramp(0.5, 0.15), [0x3e800000, 0x3e7fa06d, 0x3e266666, 0x3d9a58bf]);
    assert_eq!(ramp(1.0, 0.0), [0x3f000000, 0x3eff7777, 0x3e800000, 0x3a888889]);
    assert_eq!(ramp(0.3, 0.30000000000000004), [0x3e19999a; 4]);
    assert_eq!(ramp(0.0, 5e-324), [0; 4]);
    assert_eq!(ramp(0.3, 0.3), [(0.5f64 * 0.3) as f32; 4].map(f32::to_bits));
    assert_eq!(ramp(1.0, 1.0), [0.5f32.to_bits(); 4]);
    // samples are not clipped; only values that are not numbers are removed
    let mut block = vec![1.25f32; BLOCK_FRAMES * CHANNELS];
    (block[0], block[1], block[2]) = (f32::NAN, f32::INFINITY, f32::NEG_INFINITY);
    apply_gain(&mut block, 1.0, 1.0);
    assert_eq!(&block[..4], [0.0, 0.0, 0.0, 1.25]);
}

#[test]
fn consumer_trouble_lines_are_recognised() {
    assert!(TROUBLE_RE.is_match("remote: Underrun on node 157") && TROUBLE_RE.is_match("XRUN"));
    assert!(TROUBLE_RE.is_match("connection error"));
    assert!(!TROUBLE_RE.is_match("stream node 157"));
}

#[test]
fn start_errors_read_like_python_s() {
    let exited = OutputError::Exited { program: "consumer".into(), status: 7, detail: "boom".into() };
    assert_eq!(exited.to_string(), "consumer exited with status 7: boom");
    let silent = OutputError::Exited { program: "consumer".into(), status: -15, detail: String::new() };
    assert_eq!(silent.to_string(), "consumer exited with status -15");
    let refused = OutputError::Server("cannot reach the audio server: Connection refused".into());
    assert_eq!(
        refused.to_string(),
        "could not open the audio stream: cannot reach the audio server: Connection refused"
    );
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &Default::default());
    let log = Arc::new(PlaybackLog::for_config(&config));
    assert_eq!(log.path(), config.log_dir.join("playback.log"));
    let missing = dir.path().join("no-such-program").display().to_string();
    let options = OutputOptions {
        command: Some(vec![missing.clone()]),
        priority: Arc::new(KeepPriority),
        ..OutputOptions::default()
    };
    let output = PipeWireOutput::new(&config, log, options);
    let error = output.start().unwrap_err();
    let text = format!("could not start {missing}: [Errno 2] No such file or directory: '{missing}'");
    assert_eq!(error.to_string(), text);
    assert!(!output.alive());
    output.stop(); // not started: nothing to do
    // the output's own log goes to the log folder unless told otherwise
    assert!(fs::read_to_string(config.log_dir.join("output.log")).unwrap().contains("output: starting "));
    assert_eq!((output.target(), output.latency_ms(), output.volume(), output.buffer_ms()), ("", 50, 1.0, 200));
    assert_eq!((output.pipe_bytes(), output.delay_frames(), output.node_id()), (PIPE_BYTES, 2400, None));
    assert_eq!(
        (output.stream_state(), output.priority(), output.underflows(), output.corked()),
        (None, None, 0, false)
    );
    assert_eq!(output.dsp(), default_dsp());
    // the buffer that can be asked for has limits
    let bounded = |buffer_ms| {
        let options = OutputOptions { buffer_ms, ..OutputOptions::default() };
        PipeWireOutput::new(&config, Arc::new(PlaybackLog::new(dir.path().join("x.log"))), options).buffer_ms()
    };
    assert_eq!((bounded(0), bounded(5000)), (20, 1000));
}

// --- the feeder against the Python feeder, block by block

/// A source the script controls: what it announces is independent of what its file holds.
struct Scripted {
    path: PathBuf,
    channels: usize,
    available: AtomicU64,
    done: AtomicBool,
    total: Option<u64>,
    failed: AtomicBool,
}

impl PcmSource for Scripted {
    fn pcm_path(&self) -> &Path {
        &self.path
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn frames_available(&self) -> u64 {
        self.available.load(Ordering::SeqCst)
    }

    fn total_frames(&self) -> Option<u64> {
        self.total
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn error(&self) -> Option<String> {
        self.failed.load(Ordering::SeqCst).then(|| "render failed".to_string())
    }
}

fn write_f32(path: &Path, samples: impl Iterator<Item = f32>) {
    fs::write(path, samples.flat_map(f32::to_le_bytes).collect::<Vec<u8>>()).unwrap();
}

/// A complete source over a new file in `dir` holding `samples`.
fn scripted(dir: &Path, name: &str, channels: usize, samples: Vec<f32>) -> Scripted {
    let path = dir.join(name).join(if channels == 16 { "atmos-916.f32le" } else { "stereo.f32le" });
    fs::create_dir(path.parent().unwrap()).unwrap();
    let frames = (samples.len() / channels) as u64;
    write_f32(&path, samples.into_iter());
    Scripted { path, channels, available: frames.into(), done: true.into(), total: Some(frames), failed: false.into() }
}

/// An output that is never started, at the engine's defaults, but with its scheduling left alone.
fn idle_output(dir: &Path, options: OutputOptions) -> PipeWireOutput {
    let config = Config::new(dir.join("am-ui"), &dir.join("home"), &Default::default());
    let log = Arc::new(PlaybackLog::new(dir.join("playback.log")));
    let options = OutputOptions { log_path: Some(dir.join("output.log")), priority: Arc::new(KeepPriority), ..options };
    PipeWireOutput::new(&config, log, options)
}

/// The script of `tests/reference/output_trace.py`, run on this feeder the way that program
/// runs it on the Python one: no thread, no consumer, no clock.
struct Trace {
    output: PipeWireOutput,
    block: Vec<f32>,
    stream: Sha256,
    lines: Vec<String>,
    dir: tempfile::TempDir,
}

impl Trace {
    fn new() -> Trace {
        let dir = tempfile::tempdir().unwrap();
        Trace {
            output: idle_output(dir.path(), OutputOptions::default()), // never started
            block: vec![0.0; BLOCK_FRAMES * CHANNELS],
            stream: Sha256::new(),
            lines: Vec::new(),
            dir,
        }
    }

    /// Sample n of a source's file is ((7 n + seed) mod 251) / 251 - 0.5.
    fn write_signal(path: &Path, channels: usize, frames: u64, seed: u64) {
        let samples = (0..frames * channels as u64).map(|n| (((n * 7 + seed) % 251) as f64 / 251.0 - 0.5) as f32);
        write_f32(path, samples);
    }

    /// A complete source over a new file of `frames` frames. The script changes what the
    /// source announces.
    fn file(&self, name: &str, channels: usize, frames: u64, seed: u64) -> Scripted {
        let source = scripted(self.dir.path(), name, channels, Vec::new());
        Trace::write_signal(&source.path, channels, frames, seed);
        Scripted { available: frames.into(), total: Some(frames), ..source }
    }

    fn step(&mut self, count: usize, last: bool) {
        let inner = &self.output.inner;
        for _ in 0..count {
            lock(&inner.state).fill_block(&mut self.block, last);
            let data: Vec<u8> = self.block.iter().flat_map(|sample| sample.to_le_bytes()).collect();
            self.stream.update(&data);
            // what the feeder does once the block is written
            let (due, logged, segment, gain, trim, cursor) = {
                let mut state = lock(&inner.state);
                state.advance(true);
                let audible = state.audible(Instant::now());
                let due = state.due_events(audible);
                let logged = std::mem::take(&mut state.pending_log);
                let last = state.segments.back().unwrap();
                let segment = format!(
                    "{}:{},{},{},{}",
                    state.segments.len(),
                    last.stream_start,
                    last.frame,
                    u8::from(last.advancing),
                    last.generation
                );
                (due, logged, segment, state.gain, state.trim, state.cursor)
            };
            let output = &self.output;
            let mut line = format!(
                "{:03} {:.12} gain={:016x} trim={:016x} cur={cursor} pos={} cf={} bf={} buf={} under={} hand={} \
                 seg={segment}",
                self.lines.len(),
                format!("{:x}", Sha256::digest(&data)),
                gain.to_bits(),
                trim.to_bits(),
                output.position_frames(),
                output.cursor_frame(),
                output.buffered_frames(),
                u8::from(output.buffering()),
                output.underruns(),
                output.handoffs(),
            );
            let events: Vec<String> = due
                .iter()
                .map(|event| {
                    let kind = if event.kind == EventKind::Started { "started" } else { "ended" };
                    format!("{kind}:{}@{}", source_label(Some(&*event.source)), event.stream_frame)
                })
                .collect();
            if !events.is_empty() {
                line += &format!(" ev={}", events.join(","));
            }
            if !logged.is_empty() {
                line += &format!(" log={}", logged.join("|"));
            }
            self.lines.push(line);
        }
    }
}

/// The same commands give the same 16-channel stream, sample for sample, the same positions,
/// events and log lines as `amui/output.py`: `tests/reference/output_trace.txt` is what the
/// Python feeder does with this script (printed by `output_trace.py` next to it). The trims in
/// it come from the C library's pow(); another library could differ in a last bit.
#[test]
fn follows_the_python_feeder_block_by_block() {
    let mut trace = Trace::new();
    let dynamic = |source: &Arc<Scripted>| -> Arc<dyn PcmSource> { source.clone() };
    let a = Arc::new(trace.file("1000000001.atmos", 16, 9000, 1));
    let b = Arc::new(trace.file("1000000002.stereo", 2, 4100, 2));
    let growing = trace.file("1000000003.atmos", 16, 30000, 3);
    let c = Arc::new(Scripted { available: 1300.into(), done: false.into(), ..growing });
    let d = Arc::new(trace.file("1000000004.stereo", 2, 900, 4));
    // the file is shorter than announced
    let short = trace.file("1000000005.atmos", 16, 1203, 5);
    let e = Arc::new(Scripted { available: 1500.into(), total: Some(1500), ..short });
    let f = Arc::new(trace.file("1000000006.atmos", 16, 1000, 6));
    // no total: plays what is available
    let g = Arc::new(Scripted { total: None, ..trace.file("1000000007.stereo", 2, 500, 7) });
    // growing, with a file behind what is announced
    let behind = trace.file("1000000008.atmos", 16, 600, 8);
    let h = Arc::new(Scripted { available: 2000.into(), done: false.into(), total: Some(2000), ..behind });
    // will fail while growing
    let failing = trace.file("1000000009.stereo", 2, 5000, 9);
    let i = Arc::new(Scripted { available: 700.into(), done: false.into(), ..failing });
    // complete by its own account, but there is no file
    let j = Arc::new(trace.file("1000000010.atmos", 16, 1000, 10));
    fs::remove_file(&j.path).unwrap();
    // growing, no file yet
    let k = Arc::new(Scripted { done: false.into(), ..trace.file("1000000011.stereo", 2, 2000, 11) });
    fs::remove_file(&k.path).unwrap();

    trace.step(2, false); // nothing to play
    trace.output.seek(100); // no source: ignored
    trace.output.set_trim(&dynamic(&a), -3.0, TRIM_RAMP_S);
    trace.output.set_source(Some(dynamic(&a)), 0);
    trace.output.seek(240); // moves the start of the pending source
    trace.output.set_next_source(Some(dynamic(&b)));
    trace.output.set_next_source(Some(dynamic(&b))); // the same again: nothing
    trace.step(3, false);
    trace.output.set_volume(0.7);
    trace.step(2, false);
    trace.output.pause();
    trace.output.pause(); // already paused: nothing
    trace.step(3, false);
    trace.output.resume();
    trace.step(2, false);
    trace.output.seek(6000); // fade out, jump, fade in
    trace.step(3, false);
    trace.output.set_volume(0.25); // quieter
    trace.step(3, false);
    trace.output.set_volume(1.7); // more than everything is everything
    trace.step(1, false);
    trace.output.set_trim(&dynamic(&b), -6.0, 0.05);
    trace.step(3, false); // a ends inside a block: b continues in it
    trace.output.set_sound_check(false);
    trace.step(3, false);
    trace.output.set_sound_check(true);
    trace.step(2, false);
    trace.output.set_trim(&dynamic(&c), f64::NAN, TRIM_RAMP_S); // not a number: no trim
    trace.output.set_next_source(Some(dynamic(&c)));
    trace.step(6, false); // b ends inside a block: c (growing) continues in it, plays its reserve and starves
    c.available.store(6300, Ordering::SeqCst); // more audio, but less than the resume threshold: still waiting
    trace.step(2, false);
    c.available.store(27000, Ordering::SeqCst); // enough: fade in
    trace.step(4, false);
    trace.output.set_trim(&dynamic(&c), -5.0, 0.2); // a refined measurement while the source plays: 20 blocks
    trace.step(6, false);
    trace.output.set_trim(&dynamic(&c), 4.0, f64::INFINITY); // a boost is no trim; an endless ramp is the usual one
    trace.step(3, false);
    trace.output.seek(9999); // dropped by the source change that follows
    trace.output.set_source(Some(dynamic(&d)), 100); // while c plays: fade out, then d from frame 100
    trace.output.set_trim(&dynamic(&d), -2.5, 0.004); // a ramp shorter than a block is one block
    trace.step(5, false); // d ends, nothing follows
    trace.output.set_source(Some(dynamic(&e)), 0); // announces 1500 frames, the file holds 1203
    trace.step(5, false);
    trace.output.set_source(Some(dynamic(&f)), 0);
    trace.step(1, false);
    f.available.store(800, Ordering::SeqCst); // a complete file that shrinks
    trace.step(3, false);
    trace.output.set_source(Some(dynamic(&g)), 0);
    trace.output.set_next_source(Some(dynamic(&h)));
    trace.step(4, false); // g ends, h starts and starves at the end of its file
    h.done.store(true, Ordering::SeqCst); // done with less than announced: ends at the file's end
    trace.step(3, false);
    trace.output.set_source(Some(dynamic(&i)), 100);
    trace.step(2, false); // too little for a first block: waits
    i.failed.store(true, Ordering::SeqCst); // an error ends a growing source at what it has
    trace.step(3, false);
    trace.output.set_source(Some(dynamic(&j)), 0); // nothing to read: ends at once
    trace.step(2, false);
    trace.output.set_source(Some(dynamic(&k)), 0);
    trace.step(2, false); // nothing to read yet: waits
    Trace::write_signal(&k.path, 2, 2000, 11);
    trace.step(1, false); // too little to resume
    k.done.store(true, Ordering::SeqCst);
    trace.step(6, false);
    trace.output.set_source(None, 0);
    trace.step(2, false);
    trace.output.set_trim(&dynamic(&c), -4.0, TRIM_RAMP_S); // c keeps its place among the trims
    for number in 0..30 {
        // 34 trims: the two oldest (a, b) are dropped
        let named: Arc<dyn PcmSource> = Arc::new(Named(PathBuf::from(format!("2{number:09}.stereo/stereo.f32le"))));
        trace.output.set_trim(&named, -1.0, TRIM_RAMP_S);
    }
    trace.output.set_source(Some(dynamic(&a)), 8000);
    trace.output.set_next_source(Some(dynamic(&c)));
    trace.step(2, false); // a without its trim
    trace.output.seek(8900); // a ends in the block that fades out for the jump: no continuation in that block
    trace.step(3, false); // the last frames of a once more, then c from frame 0 with -4 dB
    trace.output.pause();
    trace.step(2, false);
    trace.output.resume();
    trace.step(2, false);
    trace.step(1, true); // the fade stop() ends with

    let mut lines = std::mem::take(&mut trace.lines);
    lines.push(format!("stream {:x}", trace.stream.clone().finalize()));
    let commands = fs::read_to_string(trace.dir.path().join("playback.log")).unwrap();
    lines.extend(commands.lines().map(|line| format!("log {}", line.split_once(' ').unwrap().1)));
    let expected: Vec<&str> = include_str!("../../tests/reference/output_trace.txt").lines().collect();
    for (number, (line, expected)) in lines.iter().zip(&expected).enumerate() {
        assert_eq!(line, expected, "line {}", number + 1);
    }
    assert_eq!(lines.len(), expected.len());
}

/// At volume 0 the song goes on in silence (the Python feeder held it where it was), and it
/// is back at once when the volume comes back.
#[test]
fn a_source_plays_on_at_volume_zero() {
    let mut trace = Trace::new();
    let song: Arc<dyn PcmSource> = Arc::new(trace.file("1000000001.atmos", 16, 48_000, 1));
    trace.output.set_source(Some(song.clone()), 0);
    trace.step(4, false);
    let before = trace.output.cursor_frame();
    assert!(before > 0);
    trace.output.set_volume(0.0);
    trace.step(10, false);
    assert_eq!(trace.output.cursor_frame(), before + 10 * BLOCK_FRAMES as u64, "the song is read on");
    assert!(trace.block.iter().all(|sample| *sample == 0.0), "in silence");
    assert_eq!(lock(&trace.output.inner.state).gain, 0.0);
    trace.output.set_volume(1.0);
    trace.step(2, false);
    assert!(trace.block.iter().any(|sample| *sample != 0.0));
    assert_eq!(trace.output.cursor_frame(), before + 12 * BLOCK_FRAMES as u64);
    // a pause still holds the song
    trace.output.pause();
    trace.step(3, false);
    let held = trace.output.cursor_frame();
    trace.step(3, false);
    assert_eq!(trace.output.cursor_frame(), held);
}

/// A source that panics when asked is finished where the cursor is (Python: a source that
/// raises), and the feeder goes on.
#[test]
fn a_source_that_panics_counts_as_finished() {
    struct Broken(PathBuf);

    impl PcmSource for Broken {
        fn pcm_path(&self) -> &Path {
            &self.0
        }

        fn channels(&self) -> usize {
            16
        }

        fn frames_available(&self) -> u64 {
            panic!("the render session is gone")
        }

        fn total_frames(&self) -> Option<u64> {
            None
        }

        fn done(&self) -> bool {
            false
        }

        fn error(&self) -> Option<String> {
            None
        }
    }

    let trace = Trace::new();
    let inner = &trace.output.inner;
    let broken: Arc<dyn PcmSource> = Arc::new(Broken(trace.dir.path().join("1000000010.atmos/atmos-916.f32le")));
    let next: Arc<dyn PcmSource> = Arc::new(trace.file("1000000011.stereo", 2, 600, 11));
    let mut block = vec![0.0f32; BLOCK_FRAMES * CHANNELS];
    trace.output.set_source(Some(broken.clone()), 480);
    assert_eq!(trace.output.buffered_frames(), 0);
    lock(&inner.state).fill_block(&mut block, false);
    {
        let state = lock(&inner.state);
        assert!(state.ended && state.cursor == 480 && !state.buffering && state.gain == 0.0);
        let queued: Vec<(i64, bool)> =
            state.events.iter().map(|event| (event.stream_frame, event.kind == EventKind::Ended)).collect();
        assert_eq!(queued, [(0, true)], "ended where it began, in the first block");
        assert!(Arc::ptr_eq(state.source.as_ref().unwrap(), &broken));
    }
    // as in Python, a source armed after the end does not start by itself
    trace.output.set_next_source(Some(next.clone()));
    lock(&inner.state).fill_block(&mut block, false);
    assert!(Arc::ptr_eq(&trace.output.next_source().unwrap(), &next));
    assert!(Arc::ptr_eq(&trace.output.active_source().unwrap(), &broken));
    assert!(block.iter().all(|sample| *sample == 0.0));
}

// --- the chain inside the output

/// Settings that pass FL to the left and FR to the right unchanged, and nothing else.
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

/// Compose blocks the way the feeder does, without a thread: (16 channels, stereo) of each.
fn compose(output: &PipeWireOutput, composer: &mut Composer, count: usize) -> Vec<(Vec<f32>, Vec<f32>)> {
    (0..count)
        .map(|_| {
            let mut state = lock(&output.inner.state);
            composer.compose(&mut state, false);
            state.advance(true);
            (composer.block.clone(), composer.stereo.clone())
        })
        .collect()
}

/// What the chain makes of the composed blocks is what `dsp::Chain` makes of them; a stereo
/// source comes out as it went in (the default downmix at 0 dB: the front gain is 1, the other
/// channels add exact zeros), after the delay that aligns the channels with the LFE's filter.
#[test]
fn the_chain_makes_stereo_of_the_blocks_and_a_stereo_source_passes() {
    let dir = tempfile::tempdir().unwrap();
    let output = idle_output(dir.path(), OutputOptions::default());
    let samples: Vec<f32> = (0..48_000).flat_map(|_| [0.25, -0.25]).collect();
    let song: Arc<dyn PcmSource> = Arc::new(scripted(dir.path(), "1000000001.stereo", 2, samples));
    let mut composer = Composer::new(output.dsp());
    let mut reference = dsp::Chain::new(default_dsp(), SAMPLE_RATE);
    output.set_source(Some(song), 0);
    let blocks = compose(&output, &mut composer, 6);
    for (block, stereo) in &blocks {
        let mut expected = vec![0.0; BLOCK_FRAMES * OUT_CHANNELS];
        reference.process(block, &mut expected);
        assert_eq!(stereo, &expected);
    }
    // the fade-in is over after the first block, the delay after 160 frames more
    let delay = (default_dsp().lfe_delay * SAMPLE_RATE as f32) as usize;
    assert_eq!(delay, 160);
    let played: Vec<[f32; 2]> = blocks.iter().flat_map(|(_, stereo)| stereo.as_chunks::<2>().0.to_vec()).collect();
    assert!(played[..delay].iter().all(|frame| *frame == [0.0, 0.0]));
    assert!(played[BLOCK_FRAMES + delay..].iter().all(|frame| *frame == [0.25, -0.25]), "unchanged");
    // what dump_recent() keeps is the stereo stream
    let wav = output.dump_recent(&dir.path().join("recent.wav")).unwrap();
    let raw = fs::read(wav).unwrap();
    let data = raw.windows(4).position(|window| window == b"data").unwrap() + 8;
    let dumped: Vec<f32> = raw[data..].as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect();
    let all: Vec<f32> = blocks.iter().flat_map(|(_, stereo)| stereo.clone()).collect();
    assert_eq!(dumped, all);
}

/// `set_dsp()` is taken before the next block and heard cross-faded over the chain's 20 ms.
#[test]
fn a_change_of_the_settings_is_heard_cross_faded() {
    let dir = tempfile::tempdir().unwrap();
    let output = idle_output(dir.path(), OutputOptions { dsp: front_pair(), ..OutputOptions::default() });
    let samples: Vec<f32> = (0..48_000).flat_map(|_| [0.5; 16]).collect();
    let song: Arc<dyn PcmSource> = Arc::new(scripted(dir.path(), "1000000001.atmos", 16, samples));
    let mut composer = Composer::new(output.dsp());
    output.set_source(Some(song), 0);
    compose(&output, &mut composer, 3);
    assert!(composer.stereo.iter().all(|sample| *sample == 0.5));
    let quieter = dsp::Settings { preamp: 0.5, ..front_pair() };
    output.set_dsp(quieter.clone());
    assert_eq!(output.dsp(), quieter);
    output.set_dsp(quieter.clone()); // the same again: nothing
    assert_eq!(fs::read_to_string(dir.path().join("playback.log")).unwrap().matches("set_dsp").count(), 1);
    let left: Vec<f32> = compose(&output, &mut composer, 4)
        .iter()
        .flat_map(|(_, stereo)| stereo.iter().step_by(2).copied().collect::<Vec<f32>>())
        .collect();
    assert!(lock(&output.inner.state).dsp_pending.is_none(), "taken by the composer");
    assert_eq!(left[dsp::FADE_FRAMES - 1..], vec![0.25; 4 * BLOCK_FRAMES - dsp::FADE_FRAMES + 1][..]);
    assert!(left[0] < 0.5 && left[0] > 0.25, "the fade begins with the first block");
    let step = left.windows(2).map(|pair| pair[0] - pair[1]).fold(0.0f32, f32::max);
    assert!(step <= 0.25 / dsp::FADE_FRAMES as f32 * 1.01, "no step: {step}");
}

// --- corks, rewrites, flushes and positions, with a consumer that plays when the test says

/// A consumer that holds what it is given until the test plays it; it can be corked, rewritten
/// and flushed.
#[derive(Default)]
struct Sim {
    capacity: usize,
    queue: VecDeque<[f32; 2]>,
    /// Everything it was given, replaced or not.
    given: Vec<[f32; 2]>,
    heard: Vec<[f32; 2]>,
    read: i64,
    written: i64,
    corked: bool,
    /// (written before, where it stopped) of each flush
    flushes: Vec<(i64, i64)>,
    /// (written before, where the rewrite began) of each rewrite
    rewrites: Vec<(i64, i64)>,
    drained: bool,
    stopped: bool,
}

impl Sim {
    fn play(&mut self, frames: usize) {
        if self.corked {
            return;
        }
        for _ in 0..frames {
            let Some(frame) = self.queue.pop_front() else { break };
            self.heard.push(frame);
            self.read += 1;
        }
    }
}

struct SimConsumer(Arc<Mutex<Sim>>);

impl Consumer for SimConsumer {
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn wait(&mut self, frames: usize, waker: &Waker, timeout: Duration) -> Result<bool, String> {
        let room = {
            let sim = lock(&self.0);
            sim.queue.len() + frames <= sim.capacity
        };
        if !room {
            waker.wait(timeout.min(Duration::from_millis(1))); // a feeder thread must not spin
        }
        Ok(room)
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), String> {
        let mut sim = lock(&self.0);
        sim.queue.extend(frames.as_chunks::<2>().0.iter().copied());
        sim.given.extend(frames.as_chunks::<2>().0.iter().copied());
        sim.written += (frames.len() / 2) as i64;
        Ok(())
    }

    fn clock(&mut self, _written: i64) -> Clock {
        Clock { played: lock(&self.0).read, at: Some(Instant::now()), running: false, delay: 0 }
    }

    fn can_pause(&self) -> bool {
        true
    }

    fn pause(&mut self) -> Result<(), String> {
        lock(&self.0).corked = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        lock(&self.0).corked = false;
        Ok(())
    }

    fn flush(&mut self) -> Result<i64, String> {
        let mut sim = lock(&self.0);
        assert!(sim.corked, "flushed only while corked");
        let (before, cut) = (sim.written, sim.read);
        sim.flushes.push((before, cut));
        sim.queue.clear();
        sim.written = cut;
        Ok(cut)
    }

    fn rewrite_from(&mut self) -> Result<i64, String> {
        Ok(lock(&self.0).read) // nothing moves while the test does not play
    }

    fn rewrite(&mut self, at: i64, frames: &[f32]) -> Result<(), String> {
        let mut sim = lock(&self.0);
        let before = sim.written;
        sim.rewrites.push((before, at));
        let keep = (at - sim.read).max(0) as usize;
        sim.queue.truncate(keep);
        sim.queue.extend(frames.as_chunks::<2>().0.iter().copied());
        sim.given.extend(frames.as_chunks::<2>().0.iter().copied());
        sim.written = at + (frames.len() / 2) as i64;
        Ok(())
    }

    fn drain(&mut self, _timeout: Duration) {
        lock(&self.0).drained = true;
    }

    fn stop(&mut self) {
        lock(&self.0).stopped = true;
    }
}

/// Frame n of an index source: FL n / 2^24, FR 0.25; what front_pair() passes through.
fn index_samples(frames: usize) -> Vec<f32> {
    let mut samples = vec![0.0; frames * 16];
    for (index, frame) in samples.as_chunks_mut::<16>().0.iter_mut().enumerate() {
        frame[0] = (index as f64 / 16_777_216.0) as f32;
        frame[1] = 0.25;
    }
    samples
}

/// (source frame, gain) of a frame heard of an index source, `None` for silence.
fn decode(frame: [f32; 2]) -> Option<(i64, f32)> {
    (frame[1] != 0.0).then(|| (((frame[0] / frame[1]) as f64 * 0.25 * 16_777_216.0).round() as i64, frame[1] / 0.25))
}

/// A feeder driven step by step, writing into a [`Sim`].
struct Bench {
    feeder: Feeder,
    sim: Arc<Mutex<Sim>>,
    output: PipeWireOutput,
    dir: tempfile::TempDir,
}

impl Bench {
    fn new(capacity: usize) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let output = idle_output(dir.path(), OutputOptions { dsp: front_pair(), ..OutputOptions::default() });
        lock(&output.inner.state).started = true;
        let run = output.inner.begin_run();
        let sim = Arc::new(Mutex::new(Sim { capacity, ..Sim::default() }));
        let feeder = Feeder::new(output.inner.clone(), run, Box::new(SimConsumer(sim.clone())), front_pair());
        Bench { feeder, sim, output, dir }
    }

    fn source(&self, name: &str, frames: usize) -> Arc<dyn PcmSource> {
        Arc::new(scripted(self.dir.path(), name, 16, index_samples(frames)))
    }

    fn progress(&self) -> (i64, usize, usize, bool, Mode, i64) {
        let sim = lock(&self.sim);
        let frames = lock(&self.output.inner.state).frames_written;
        (sim.written, sim.flushes.len(), sim.rewrites.len(), sim.corked, self.feeder.mode, frames)
    }

    /// Step until the feeder has nothing left to do while the consumer does not play.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            let before = self.progress();
            match self.feeder.step().unwrap() {
                Flow::Exit => panic!("the run ended"),
                Flow::Wait => return,
                Flow::Continue => {}
            }
            if self.progress() == before {
                return;
            }
        }
        panic!("the feeder does not settle");
    }

    /// The consumer plays `frames`, then the feeder catches up.
    fn play(&mut self, frames: usize) {
        lock(&self.sim).play(frames);
        self.settle();
    }

    fn heard(&self) -> Vec<[f32; 2]> {
        lock(&self.sim).heard.clone()
    }

    fn events(&self) -> Arc<Mutex<Vec<(&'static str, String)>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let started = seen.clone();
        self.output.on_source_started(move |source| {
            lock(&started).push(("started", source_label(Some(&*source))));
        });
        let ended = seen.clone();
        self.output.on_source_ended(move |source| lock(&ended).push(("ended", source_label(Some(&*source)))));
        seen
    }
}

/// The source frames of what was heard, in order, with the gain they were heard at.
fn decoded(heard: &[[f32; 2]]) -> Vec<(i64, f32)> {
    heard.iter().filter_map(|frame| decode(*frame)).collect()
}

/// With nothing to play for two seconds the stream corks and stands still; a new source
/// uncorks it, the silence it held is dropped and the source is heard at once.
#[test]
fn an_idle_stream_corks_and_holds_its_position() {
    let mut bench = Bench::new(9600);
    bench.settle();
    assert_eq!(lock(&bench.sim).written, 9600, "silence fills the consumer");
    for _ in 0..30 {
        bench.play(4800);
    }
    assert_eq!(bench.feeder.mode, Mode::Corked(Corked::Idle));
    assert!(lock(&bench.sim).corked && bench.output.corked());
    let (written, frames) = (lock(&bench.sim).written, bench.output.frames_written());
    assert!(frames >= IDLE_CORK_FRAMES as u64 && frames < IDLE_CORK_FRAMES as u64 + 2 * BLOCK_FRAMES as u64);
    let audible = bench.output.audible_stream_frame();
    bench.play(4800);
    assert_eq!((lock(&bench.sim).written, bench.output.audible_stream_frame()), (written, audible), "standing still");
    assert_eq!(bench.feeder.step().unwrap(), Flow::Wait);

    let song = bench.source("1000000001.atmos", 48_000);
    bench.output.set_source(Some(song), 0);
    bench.settle();
    let sim = lock(&bench.sim);
    assert!(!sim.corked && !bench.output.corked());
    assert_eq!(sim.flushes.len(), 1, "the silence is dropped");
    let (_, cut) = sim.flushes[0];
    assert_eq!(cut, sim.read);
    let first = sim.queue.iter().position(|frame| frame[1] != 0.0).unwrap();
    assert_eq!(first, 1, "the source's fade-in is next (its first frame at gain 0)");
    assert_eq!(decode(sim.queue[1]), Some((1, 1.0 / 480.0)));
    drop(sim);
    assert_eq!(bench.output.position_frames(), 0);
    bench.play(960);
    assert_eq!(bench.output.position_frames(), 960);
}

/// Pause fades out what was about to be heard, at once, then the stream corks; while it is
/// corked nothing moves; resume gives the audio the pause took back again, fading in: every
/// frame of the source is heard once, in order.
#[test]
fn a_pause_fades_at_once_and_the_resume_loses_no_frame() {
    let mut bench = Bench::new(9600);
    let song = bench.source("1000000001.atmos", 4 * 48_000);
    bench.output.set_source(Some(song), 0);
    bench.settle();
    bench.play(9600);
    assert_eq!(bench.output.position_frames(), 9600);
    bench.output.pause();
    bench.settle();
    {
        let sim = lock(&bench.sim);
        assert_eq!(sim.rewrites, [(2 * 9600, 9600)], "what the consumer held was replaced from where it played");
        assert!(sim.flushes.is_empty(), "a stream that plays is not flushed");
        assert!(matches!(bench.feeder.mode, Mode::Fading { until: 10_080, .. }));
        // what it gets now: a fade of what it was about to play, then silence
        let fade = decoded(&sim.queue.iter().take(480).copied().collect::<Vec<_>>());
        let sources: Vec<i64> = fade.iter().map(|(frame, _)| *frame).collect();
        assert_eq!(sources, (9600..10_080).collect::<Vec<_>>());
        assert_eq!((fade[0].1, fade[479].1), (1.0, 1.0 / 480.0));
        assert!(fade.windows(2).all(|pair| pair[1].1 < pair[0].1));
        assert!(sim.queue.iter().skip(480).all(|frame| *frame == [0.0, 0.0]));
    }
    bench.play(480); // the fade has been heard: the stream corks
    assert_eq!(bench.feeder.mode, Mode::Corked(Corked::Paused));
    assert!(lock(&bench.sim).corked && bench.output.corked());
    let held = bench.output.position_frames();
    assert_eq!(held, 10_080, "the first frame not heard");
    bench.play(4800);
    assert_eq!((bench.output.position_frames(), bench.heard().len()), (held, 10_080), "nothing moves while corked");
    assert_eq!(lock(&bench.output.inner.state).cursor, 2 * 9600, "the composition never saw the pause");

    bench.output.resume();
    bench.settle();
    assert!(!lock(&bench.sim).corked);
    assert_eq!(lock(&bench.sim).flushes.len(), 1, "the silence is dropped (corked: nothing moves)");
    bench.play(9600);
    assert_eq!(bench.output.position_frames(), held + 9600);
    bench.play(9600);
    let heard = decoded(&bench.heard());
    let sources: Vec<i64> = heard.iter().map(|(frame, _)| *frame).collect();
    // frame 0 and the first frame after the resume are heard at gain 0 (the fade-ins' first)
    let expected: Vec<i64> = (1..10_080).chain(10_081..10_080 + 2 * 9600).collect();
    assert_eq!(sources, expected);
    let resumed = heard.iter().position(|(frame, _)| *frame == 10_081).unwrap();
    assert_eq!(heard[resumed].1, 1.0 / 480.0);
    assert!(heard[resumed + 479..].iter().all(|(_, gain)| *gain == 1.0), "faded in over a block");
}

/// A seek is heard at once: what the consumer holds is replaced by a fade of what it was about
/// to play and the new position. Positions and the stream frames of the events stay right.
#[test]
fn a_seek_is_heard_at_once_and_positions_stay_right() {
    let mut bench = Bench::new(9600);
    let song = bench.source("1000000001.atmos", 4 * 48_000);
    bench.output.set_source(Some(song), 0);
    bench.settle();
    bench.play(9600);
    bench.output.seek(96_000);
    assert_eq!(bench.output.position_frames(), 96_000, "the new position at once");
    bench.settle();
    let sim = lock(&bench.sim);
    assert_eq!((sim.rewrites.as_slice(), sim.flushes.len()), (&[(2 * 9600, 9600)][..], 0));
    let given = decoded(&sim.queue.iter().copied().collect::<Vec<_>>());
    drop(sim);
    // the fade of the old position, then the new one, fading in (its first frame at gain 0),
    // as much as the consumer held of the old one and a block more, in one write
    let sources: Vec<i64> = given.iter().map(|(frame, _)| *frame).collect();
    let expected: Vec<i64> = (9600..10_080).chain(96_001..96_000 + 9600 + 480).collect();
    assert_eq!(sources, expected);
    assert_eq!(given[479].1, 1.0 / 480.0);
    bench.play(480);
    assert_eq!(bench.output.position_frames(), 96_000, "the fade still belongs to the old position");
    bench.play(480);
    assert_eq!(bench.output.position_frames(), 96_480);
    // the composed frame heard: the skipped fade-out block and the dropped frames are not
    let state = lock(&bench.output.inner.state);
    let jump_start = state.segments.iter().find(|segment| segment.frame == 96_000).unwrap().stream_start;
    assert_eq!(jump_start, 2 * 9600 + 480, "after the block that faded out for the jump");
    drop(state);
    assert_eq!(bench.output.audible_stream_frame(), jump_start as u64 + 480);
    bench.play(9600);
    assert_eq!(bench.output.position_frames(), 96_480 + 9600);
}

/// Events in replaced frames: those of a gapless handoff fire once the new audio is heard; the
/// end of a source that a seek takes back into is dropped, since it was never heard.
#[test]
fn events_that_pointed_into_replaced_frames() {
    let mut bench = Bench::new(9600);
    let seen = bench.events();
    let a = bench.source("1000000001.atmos", 4800);
    let b = bench.source("1000000002.atmos", 4 * 48_000);
    bench.output.set_source(Some(a), 0);
    bench.output.set_next_source(Some(b));
    bench.settle(); // a ends at stream frame 4800, b follows in the same block
    bench.play(2400);
    assert_eq!(*lock(&seen), [("started", "1000000001.atmos".to_string())]);
    bench.output.seek(48_000); // b is the active source now
    bench.settle();
    bench.play(479); // the fade of a
    assert_eq!(lock(&seen).len(), 1);
    bench.play(1); // the first frame after the fade is being heard
    let expected = [
        ("started", "1000000001.atmos".to_string()),
        ("ended", "1000000001.atmos".to_string()),
        ("started", "1000000002.atmos".to_string()),
    ];
    assert_eq!(*lock(&seen), expected);
    assert_eq!(bench.output.position_frames(), 48_000);

    // the end of c is queued but not heard; a seek back into c drops it: c plays on
    let mut bench = Bench::new(9600);
    let seen = bench.events();
    let c = bench.source("1000000003.atmos", 4800);
    bench.output.set_source(Some(c), 0);
    bench.settle();
    bench.play(2400);
    bench.output.seek(1000);
    bench.settle();
    assert!(fs::read_to_string(bench.dir.path().join("playback.log")).unwrap().contains("was not heard"));
    for _ in 0..4 {
        bench.play(2400);
    }
    let expected = [("started", "1000000003.atmos".to_string()), ("ended", "1000000003.atmos".to_string())];
    assert_eq!(*lock(&seen), expected, "one end, the one that was heard");
    let sources: Vec<i64> = decoded(&bench.heard()).iter().map(|(frame, _)| *frame).collect();
    assert_eq!(sources.last(), Some(&4799));
}

/// A pause while the consumer holds little has nothing to cut: the composition fades out and
/// writes silence, and the stream corks once that has been heard; resume fades in from there.
#[test]
fn a_pause_with_little_queued_fades_in_the_composition() {
    let mut bench = Bench::new(1920);
    let song = bench.source("1000000001.atmos", 48_000);
    bench.output.set_source(Some(song), 0);
    bench.settle();
    bench.play(1920);
    bench.output.pause();
    bench.settle();
    assert!(matches!(bench.feeder.mode, Mode::Settling { .. }));
    for _ in 0..3 {
        bench.play(960);
    }
    assert_eq!(bench.feeder.mode, Mode::Corked(Corked::Paused));
    assert!(lock(&bench.sim).rewrites.is_empty(), "nothing was rewritten to pause");
    let held = bench.output.position_frames();
    assert_eq!(held, lock(&bench.output.inner.state).cursor as u64);
    bench.output.resume();
    bench.settle();
    assert_eq!(lock(&bench.sim).flushes.len(), 1, "the silence is dropped");
    bench.play(1920);
    let heard = decoded(&bench.heard());
    let sources: Vec<i64> = heard.iter().map(|(frame, _)| *frame).collect();
    assert!(sources.windows(2).all(|pair| pair[1] > pair[0]), "never back");
    let gaps = sources.windows(2).filter(|pair| pair[1] != pair[0] + 1).count();
    assert_eq!(gaps, 1, "the frame heard at gain 0 when fading in again");
}

/// Stopping cuts and fades what was about to be heard and lets the fade play.
#[test]
fn stopping_fades_out_what_was_about_to_play() {
    let mut bench = Bench::new(9600);
    let song = bench.source("1000000001.atmos", 48_000);
    bench.output.set_source(Some(song), 0);
    bench.settle();
    bench.play(4800);
    bench.output.inner.stop.store(true, Ordering::SeqCst);
    assert_eq!(bench.feeder.step().unwrap(), Flow::Exit);
    bench.feeder.finish();
    let sim = lock(&bench.sim);
    assert_eq!(sim.rewrites, [(4800 + 9600, 4800)]);
    assert!(sim.drained);
    let fade = decoded(&sim.queue.iter().copied().collect::<Vec<_>>());
    assert_eq!(fade.len(), 480);
    assert_eq!((fade[0], fade[479].1), ((4800, 1.0), 1.0 / 480.0));
}

// --- the feeder thread

/// The feeder asks for its scheduling once per start, on its own thread, before it opens the
/// stream, and the answer is logged; `start()` begins with a new chain at the latest settings.
#[test]
fn the_feeder_asks_for_its_priority_once_per_start() {
    struct Counting(Mutex<Vec<String>>);

    impl FeederPriority for Counting {
        fn raise(&self) -> Grant {
            lock(&self.0).push(thread::current().name().unwrap_or_default().to_string());
            Grant::RealtimeKit { priority: FEEDER_RT_PRIORITY }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let sim = Arc::new(Mutex::new(Sim { capacity: 4800, ..Sim::default() }));
    let counting = Arc::new(Counting(Mutex::new(Vec::new())));
    let made = sim.clone();
    let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &Default::default());
    let options = OutputOptions {
        dsp: front_pair(),
        consumer: Some(Arc::new(move || Box::new(SimConsumer(made.clone())) as Box<dyn Consumer>)),
        priority: counting.clone(),
        log_path: Some(dir.path().join("output.log")),
        ..OutputOptions::default()
    };
    let output = PipeWireOutput::new(&config, Arc::new(PlaybackLog::new(dir.path().join("playback.log"))), options);
    let song: Arc<dyn PcmSource> = Arc::new(scripted(dir.path(), "1000000001.atmos", 16, vec![0.5; 16 * 48_000]));
    output.set_dsp(dsp::Settings { preamp: 0.5, ..front_pair() }); // before the start: no fade
    output.set_source(Some(song), 0);
    output.start().unwrap();
    let full = || lock(&sim).given.len() >= 4800;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !full() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    output.stop();
    assert!(full());
    assert_eq!(lock(&sim).rewrites.len(), 1, "stopping replaced what it held by a fade");
    let given = lock(&sim).given.clone();
    assert!(given[BLOCK_FRAMES..4800].iter().all(|frame| *frame == [0.25, 0.25]), "the new settings from the start");
    assert!(lock(&sim).stopped);
    output.start().unwrap();
    output.stop();
    assert_eq!(*lock(&counting.0), ["am-ui-feeder", "am-ui-feeder"]);
    assert_eq!(output.priority(), Some(Grant::RealtimeKit { priority: FEEDER_RT_PRIORITY }));
    let log = fs::read_to_string(dir.path().join("playback.log")).unwrap();
    assert_eq!(log.matches("output: feeder priority: SCHED_RR priority 5 through RealtimeKit").count(), 2);
}
