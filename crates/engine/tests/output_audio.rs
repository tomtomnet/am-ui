//! `PipeWireOutput` through its real consumer, the PulseAudio stream, into the null sink of a
//! private audio server. Ignored: they need that server and are run by hand through
//! `crates/engine/tools/private-audio.sh`, which starts one (PipeWire, WirePlumber without device
//! monitors, pipewire-pulse, one null sink) and points these tests at it:
//!
//! ```sh
//! crates/engine/tools/private-audio.sh cargo test -p amui --offline --test output_audio -- --ignored --test-threads 1 --nocapture
//! ```
//!
//! Without the script they refuse to run: they must never reach the desktop session's server.
//! What was heard is recorded from the null sink's monitor with the server's own `parec`. The
//! sources carry their frame index on the left and a mark on the right; the chain passes them
//! through, so every recorded frame says which source frame it was and at which gain. The
//! feeder asks for its scheduling as the engine does (the kernel, then RealtimeKit on the system
//! bus); each test prints what it got and the numbers it measured.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

use amui::config::{Config, SAMPLE_RATE};
use amui::dsp;
use amui::output::{OutputOptions, PipeWireOutput, PlaybackLog};
use amui::render::PcmSource;

const RATE: usize = SAMPLE_RATE as usize;
const INDEX_SCALE: f64 = 16_777_216.0;
const MARK: f32 = 0.25;

/// The private server's folder, or a refusal to go on.
fn private_server() -> PathBuf {
    let dir = std::env::var("AMUI_PRIVATE_AUDIO")
        .expect("run these tests through crates/engine/tools/private-audio.sh: they need its private audio server");
    let server = std::env::var("PULSE_SERVER").unwrap_or_default();
    assert_eq!(server, format!("unix:{dir}/pulse/native"), "PULSE_SERVER must name the private server");
    assert!(dir.starts_with("/tmp/amui-audio-"), "{dir}");
    PathBuf::from(dir)
}

/// FL to the left and FR to the right, unchanged.
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

/// A complete stereo source: frame n is (n / 2^24, MARK); `stall_ms` holds the feeder up once.
struct IndexSource {
    path: PathBuf,
    frames: u64,
    stall_ms: AtomicU64,
}

impl PcmSource for IndexSource {
    fn pcm_path(&self) -> &Path {
        &self.path
    }

    fn channels(&self) -> usize {
        2
    }

    fn frames_available(&self) -> u64 {
        let stall = self.stall_ms.swap(0, Ordering::SeqCst);
        if stall > 0 {
            sleep(Duration::from_millis(stall)); // the feeder is held up inside its lock
        }
        self.frames
    }

    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn done(&self) -> bool {
        true
    }

    fn error(&self) -> Option<String> {
        None
    }
}

fn index_source(dir: &Path, name: &str, seconds: usize) -> Arc<IndexSource> {
    let path = dir.join(name).join("stereo.f32le");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let frames = seconds * RATE;
    let mut file = fs::File::create(&path).unwrap();
    let mut bytes = Vec::with_capacity(RATE * 8);
    for start in (0..frames).step_by(RATE) {
        bytes.clear();
        for index in start..(start + RATE).min(frames) {
            bytes.extend_from_slice(&((index as f64 / INDEX_SCALE) as f32).to_le_bytes());
            bytes.extend_from_slice(&MARK.to_le_bytes());
        }
        file.write_all(&bytes).unwrap();
    }
    Arc::new(IndexSource { path, frames: frames as u64, stall_ms: AtomicU64::new(0) })
}

/// (source frame, gain) of a recorded frame, `None` for silence.
fn decode(frame: [f32; 2]) -> Option<(i64, f32)> {
    (frame[1] != 0.0)
        .then(|| (((frame[0] / frame[1]) as f64 * f64::from(MARK) * INDEX_SCALE).round() as i64, frame[1] / MARK))
}

/// The null sink's monitor, recorded with the private server's `parec`; every chunk with the
/// time it arrived.
struct Recorder {
    child: Child,
    reader: JoinHandle<Vec<(Instant, Vec<[f32; 2]>)>>,
}

/// What was recorded, frame by frame, each with the time it was heard at the latest.
struct Recording {
    frames: Vec<[f32; 2]>,
    times: Vec<Instant>,
}

impl Recorder {
    /// `AMUI_RECORD_LATENCY_MS` (10 by default) is the latency the recorder asks for; the graph's
    /// quantum follows the smallest one asked for (10 ms: 256 frames; 100 ms leaves the stream's
    /// own 2048).
    fn start() -> Recorder {
        let latency = std::env::var("AMUI_RECORD_LATENCY_MS").unwrap_or_else(|_| "10".to_string());
        let mut child = Command::new("parec")
            .args(["--device=amui-null.monitor", "--format=float32le", "--rate=48000", "--channels=2", "--raw"])
            .arg(format!("--latency-msec={latency}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("parec, the private server's recorder");
        let mut stdout = child.stdout.take().unwrap();
        let reader = thread::spawn(move || {
            let mut chunks = Vec::new();
            let mut buffer = vec![0u8; 8192];
            let mut pending = Vec::new();
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let now = Instant::now();
                        pending.extend_from_slice(&buffer[..count]);
                        let whole = pending.len() / 8 * 8;
                        let frames = pending[..whole]
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|bytes| {
                                let [a, b, c, d, e, f, g, h] = *bytes;
                                [f32::from_le_bytes([a, b, c, d]), f32::from_le_bytes([e, f, g, h])]
                            })
                            .collect();
                        pending.drain(..whole);
                        chunks.push((now, frames));
                    }
                }
            }
            chunks
        });
        sleep(Duration::from_millis(300)); // recording before anything plays
        Recorder { child, reader }
    }

    fn stop(mut self) -> Recording {
        sleep(Duration::from_millis(200));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let chunks = self.reader.join().unwrap();
        let mut recording = Recording { frames: Vec::new(), times: Vec::new() };
        for (arrived, frames) in chunks {
            let count = frames.len();
            for (index, frame) in frames.into_iter().enumerate() {
                let later = Duration::from_secs_f64((count - 1 - index) as f64 / RATE as f64);
                recording.frames.push(frame);
                recording.times.push(arrived.checked_sub(later).unwrap_or(arrived));
            }
        }
        recording
    }
}

impl Recording {
    /// The first frame heard at or after `at` for which `wanted` holds.
    fn first_after(&self, at: Instant, wanted: impl Fn([f32; 2]) -> bool) -> Option<usize> {
        let start = self.times.iter().position(|time| *time >= at)?;
        (start..self.frames.len()).find(|&index| wanted(self.frames[index]))
    }

    fn millis_after(&self, index: usize, at: Instant) -> f64 {
        self.times[index].saturating_duration_since(at).as_secs_f64() * 1000.0
    }

    /// The source frames heard in order (silence left out).
    fn sources(&self) -> Vec<(i64, f32)> {
        self.frames.iter().filter_map(|frame| decode(*frame)).collect()
    }
}

struct Player {
    output: PipeWireOutput,
    dir: tempfile::TempDir,
}

impl Player {
    fn new(buffer_ms: u32) -> Player {
        let server = private_server();
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new());
        let log = Arc::new(PlaybackLog::new(dir.path().join("playback.log")));
        let mut options = OutputOptions {
            server: Some(format!("unix:{}/pulse/native", server.display())),
            dsp: front_pair(),
            buffer_ms,
            log_path: Some(dir.path().join("output.log")),
            ..OutputOptions::default()
        };
        if std::env::var("AMUI_KEEP_PRIORITY").is_ok() {
            options.priority = Arc::new(amui::output::KeepPriority); // for a comparison
        }
        let output = PipeWireOutput::new(&config, log, options);
        output.start().expect("the private server takes the stream");
        Player { output, dir }
    }

    fn log(&self) -> String {
        let playback = fs::read_to_string(self.dir.path().join("playback.log")).unwrap_or_default();
        let own = fs::read_to_string(self.dir.path().join("output.log")).unwrap_or_default();
        format!("{playback}{own}")
    }

    fn report(&self, name: &str) {
        println!(
            "[{name}] feeder scheduling: {}",
            self.output.priority().map(|grant| grant.to_string()).unwrap_or_default()
        );
        for line in self.log().lines().filter(|line| line.contains("PulseAudio stream ready")).take(1) {
            println!("[{name}] {}", line.split_once(' ').map_or(line, |(_, rest)| rest));
        }
    }
}

fn ms(frames: u64) -> f64 {
    frames as f64 * 1000.0 / RATE as f64
}

fn percentile(values: &mut [f64], at: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * at).round() as usize]
}

/// The latency the stream reports (given but not heard yet) settles near the buffer asked for.
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn latency_settles_near_the_target() {
    let player = Player::new(amui::output::BUFFER_MS);
    player.report("latency");
    let source = index_source(player.dir.path(), "1000000001.stereo", 10);
    player.output.set_source(Some(source.clone()), 0);
    sleep(Duration::from_secs(1));
    let mut latencies = Vec::new();
    let mut delays = Vec::new();
    let begun = Instant::now();
    while begun.elapsed() < Duration::from_secs(4) {
        latencies.push(ms(player.output.latency_frames()));
        delays.push(ms(player.output.delay_frames()));
        sleep(Duration::from_millis(20));
    }
    let position = player.output.position_frames();
    player.output.stop();
    let (low, middle, high) =
        (percentile(&mut latencies, 0.0), percentile(&mut latencies, 0.5), percentile(&mut latencies, 1.0));
    println!(
        "[latency] buffer asked {} ms: latency min {low:.1} ms, median {middle:.1} ms, max {high:.1} ms over {} samples; \
         device delay {:.1} ms; underflows {}",
        amui::output::BUFFER_MS,
        latencies.len(),
        percentile(&mut delays, 0.5),
        player.output.underflows()
    );
    let target = f64::from(amui::output::BUFFER_MS);
    assert!((target - 50.0..=target + 60.0).contains(&middle), "median {middle} ms");
    assert!(position > 4 * RATE as u64, "{position}");
    assert_eq!(player.output.underflows(), 0);
}

/// Pause fades out at once and corks the stream: the server takes nothing more and positions
/// stand still; resume gives back what the pause took, so that every frame is heard once.
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn pause_stops_the_stream_and_resume_loses_no_frame() {
    let recorder = Recorder::start();
    let player = Player::new(amui::output::BUFFER_MS);
    player.report("pause");
    let source = index_source(player.dir.path(), "1000000001.stereo", 10);
    player.output.set_source(Some(source.clone()), 0);
    sleep(Duration::from_millis(1500));
    let heard = player.output.position_frames() as i64; // being heard when pause() is called
    let paused_at = Instant::now();
    player.output.pause();
    sleep(Duration::from_millis(300));
    assert!(player.output.corked(), "corked after the fade");
    assert_eq!(player.output.stream_state().as_deref(), Some("corked"));
    let (held, stream) = (player.output.position_frames(), player.output.audible_stream_frame());
    sleep(Duration::from_millis(700));
    assert_eq!((player.output.position_frames(), player.output.audible_stream_frame()), (held, stream));
    let resumed_at = Instant::now();
    player.output.resume();
    sleep(Duration::from_millis(1000));
    assert!(!player.output.corked());
    let moved = player.output.position_frames() - held;
    player.output.stop();
    let recording = recorder.stop();
    let underflows = player.output.underflows();

    // the fade begins where the stream was playing and ends in silence
    let silent = recording.first_after(paused_at, |frame| frame == [0.0, 0.0]).unwrap();
    let fade_start = (0..silent)
        .rev()
        .take_while(|&index| decode(recording.frames[index]).is_some_and(|(_, gain)| gain < 1.0))
        .last();
    let back = recording.first_after(resumed_at, |frame| frame != [0.0, 0.0]).unwrap();
    let pause_ms = recording.millis_after(silent, paused_at);
    let resume_ms = recording.millis_after(back, resumed_at);
    let quiet = recording.frames[silent..back].iter().all(|frame| *frame == [0.0, 0.0]);
    // by the stream's own counters: the old audio heard after pause(), then the fade
    let head = fade_start.and_then(|start| decode(recording.frames[start - 1])).map_or(heard, |(frame, _)| frame);
    let counted_ms = ms((head - heard).max(0) as u64 + 480);
    println!(
        "[pause] silent {pause_ms:.1} ms after pause() by the recording, {counted_ms:.1} ms by the stream's position \
         (fade of {} frames from frame {head}); heard again {resume_ms:.1} ms after resume() (the recording's times \
         include its own latency); moved {:.0} ms in 1 s after the resume; underflows {underflows}",
        fade_start.map_or(0, |start| silent - start),
        ms(moved)
    );
    assert!(quiet, "silence while paused");
    assert!(counted_ms < 150.0, "pause {counted_ms} ms");
    if std::env::var("AMUI_RECORD_LATENCY_MS").is_err() {
        assert!(pause_ms < 150.0 && resume_ms < 150.0, "pause {pause_ms} ms, resume {resume_ms} ms");
    }
    // every frame once, in order: the ones heard at gain 0 (the first of a fade-in) left out
    let sources = recording.sources();
    let jumps: Vec<(i64, i64)> =
        sources.windows(2).filter(|pair| pair[1].0 != pair[0].0 + 1).map(|pair| (pair[0].0, pair[1].0)).collect();
    println!("[pause] {} source frames heard, {} irregularities: {jumps:?}", sources.len(), jumps.len());
    assert!(jumps.iter().all(|(from, to)| to - from == 2), "only the frames at gain 0: {jumps:?}");
    assert!(jumps.len() <= 1, "{jumps:?}");
    assert_eq!(underflows, 0);
}

/// A seek is heard within a few tens of milliseconds: what the stream holds is replaced by a
/// fade of what it was about to play and the new position. Counted too: fades that do not
/// follow the old audio at once (a silence, a skip or a repeat between them would click; a
/// flush of the playing stream did that to a few of every twenty seeks).
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn a_seek_is_heard_at_once() {
    let recorder = Recorder::start();
    let player = Player::new(amui::output::BUFFER_MS);
    player.report("seek");
    let source = index_source(player.dir.path(), "1000000001.stereo", 60);
    player.output.set_source(Some(source.clone()), 0);
    let mut seeks = Vec::new();
    for round in 0..20u64 {
        sleep(Duration::from_millis(300 + 53 * (round % 7)));
        let target = ((2 + round * 13 % 50) as usize * RATE) as i64;
        let heard = player.output.position_frames() as i64; // the old position, being heard
        let at = Instant::now();
        player.output.seek(target as u64);
        assert_eq!(player.output.position_frames(), target as u64, "the new position at once");
        seeks.push((at, target, heard));
    }
    sleep(Duration::from_millis(700));
    player.output.stop();
    let recording = recorder.stop();
    let mut latencies = Vec::new();
    let mut played_on = Vec::new();
    let mut gaps = 0;
    for (at, target, heard) in &seeks {
        let new = recording
            .first_after(*at, |frame| {
                decode(frame).is_some_and(|(index, _)| index >= *target && index < target + RATE as i64 / 4)
            })
            .unwrap();
        let (index, _) = decode(recording.frames[new]).unwrap();
        // walk back over the fade: its frames below gain 1, then its first one at gain 1
        let mut start = new - 1; // the new position's first frame, heard at gain 0
        while decode(recording.frames[start - 1]).is_some_and(|(_, gain)| gain < 1.0) {
            start -= 1;
        }
        let faded = new - 1 - start;
        let (head, _) = decode(recording.frames[start - 1]).unwrap();
        let seamless = decode(recording.frames[start - 2]) == Some((head - 1, 1.0));
        gaps += usize::from(!seamless);
        if !seamless {
            let silent = (0..start - 1).rev().take_while(|&at| recording.frames[at] == [0.0, 0.0]).count();
            let before = decode(recording.frames[start - 2 - silent]);
            let around: Vec<Option<(i64, f32)>> =
                (start - 4 - silent..start + 2).map(|at| decode(recording.frames[at])).collect();
            println!(
                "[seek]   {silent} silent frames between the old audio (last {before:?}) and the fade from {head}: {around:?}"
            );
        }
        latencies.push(recording.millis_after(new, *at));
        // by the stream's own counters: the old audio heard after seek(), then the fade
        played_on.push(ms((head - heard).max(0) as u64 + 480));
        println!(
            "[seek] to {target}: heard {:.1} ms after seek() by the recording, {:.1} ms by the stream's position \
             (old audio from {heard} on, then a fade of {faded} frames from frame {head}), first frame {index} (+{}), {}",
            recording.millis_after(new, *at),
            played_on.last().unwrap(),
            index - target,
            if seamless { "right after the old audio" } else { "after a gap" }
        );
        assert!(index - target <= 2, "the new position from its start");
        assert_eq!(faded, 479, "the old position fades out first");
    }
    let (mut sorted, mut counted) = (latencies.clone(), played_on.clone());
    println!(
        "[seek] {} seeks: heard after median {:.1} ms, worst {:.1} ms by the recording (with its own latency); \
         median {:.1} ms, worst {:.1} ms by the stream's position; fades after a gap {gaps}; underflows {}",
        seeks.len(),
        percentile(&mut sorted, 0.5),
        percentile(&mut latencies, 1.0),
        percentile(&mut counted, 0.5),
        percentile(&mut played_on, 1.0),
        player.output.underflows()
    );
    assert!(percentile(&mut played_on, 1.0) < 150.0, "{played_on:?}");
    assert_eq!(gaps, 0, "every fade follows the old audio at once");
    assert_eq!(player.output.underflows(), 0);
}

/// A minute of playback while twice as many busy loops as there are processors run beside
/// it: no underflow, no starvation.
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn no_underflow_under_load() {
    let player = Player::new(amui::output::BUFFER_MS);
    player.report("load");
    let source = index_source(player.dir.path(), "1000000001.stereo", 70);
    let processors = thread::available_parallelism().map_or(4, |count| count.get());
    let running = Arc::new(AtomicBool::new(true));
    let loops: Vec<JoinHandle<u64>> = (0..2 * processors)
        .map(|_| {
            let running = running.clone();
            thread::spawn(move || {
                let mut spins = 0u64;
                while running.load(Ordering::Relaxed) {
                    spins = std::hint::black_box(spins.wrapping_add(1));
                }
                spins
            })
        })
        .collect();
    sleep(Duration::from_millis(200));
    player.output.set_source(Some(source.clone()), 0);
    let begun = Instant::now();
    let mut gaps = Vec::new();
    let mut latencies = Vec::new();
    while begun.elapsed() < Duration::from_secs(60) {
        sleep(Duration::from_millis(500));
        gaps.push(player.output.feeder_gap_ms());
        latencies.push(ms(player.output.latency_frames()));
    }
    let position = player.output.position_frames();
    running.store(false, Ordering::Relaxed);
    let spins: u64 = loops.into_iter().map(|handle| handle.join().unwrap()).sum();
    player.output.stop();
    println!(
        "[load] {} busy loops ({spins} spins) for 60 s: underflows {}, starved {}, buffer low {}, feeder gap max {:.1} ms \
         (median {:.1}), latency {:.0}..{:.0} ms, position {:.1} s",
        2 * processors,
        player.output.underflows(),
        player.output.underruns(),
        player.output.pipe_low_count(),
        percentile(&mut gaps.clone(), 1.0),
        percentile(&mut gaps, 0.5),
        percentile(&mut latencies.clone(), 0.0),
        percentile(&mut latencies, 1.0),
        position as f64 / RATE as f64
    );
    assert_eq!(player.output.underflows(), 0);
    assert_eq!(player.output.underruns(), 0);
    assert!(position > 55 * RATE as u64);
}

/// A feeder held up for 150 ms plays through; held up for a second, the server runs out once,
/// says so, and playback goes on where it was.
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn a_stalled_feeder() {
    let recorder = Recorder::start();
    let player = Player::new(amui::output::BUFFER_MS);
    player.report("stall");
    let source = index_source(player.dir.path(), "1000000001.stereo", 20);
    player.output.set_source(Some(source.clone()), 0);
    sleep(Duration::from_millis(1500));
    source.stall_ms.store(150, Ordering::SeqCst);
    sleep(Duration::from_millis(1500));
    let after_short = player.output.underflows();
    let before_long = player.output.position_frames();
    source.stall_ms.store(1000, Ordering::SeqCst);
    sleep(Duration::from_millis(2500));
    let after_long = player.output.underflows();
    let position = player.output.position_frames();
    player.output.stop();
    let recording = recorder.stop();
    let sources = recording.sources();
    let jumps: Vec<(i64, i64)> =
        sources.windows(2).filter(|pair| pair[1].0 != pair[0].0 + 1).map(|pair| (pair[0].0, pair[1].0)).collect();
    let silences: Vec<usize> = {
        let mut runs = Vec::new();
        let mut run = 0;
        for frame in &recording.frames {
            if *frame == [0.0, 0.0] {
                run += 1;
            } else {
                if run > 0 {
                    runs.push(run);
                }
                run = 0;
            }
        }
        runs.into_iter().filter(|run| *run > 48).collect()
    };
    println!(
        "[stall] 150 ms: underflows {after_short}; 1 s: underflows {after_long}, then {:.1} s more played; silences \
         heard (ms): {:?}; source frames heard {}, irregularities {jumps:?}",
        (position - before_long) as f64 / RATE as f64,
        silences.iter().map(|run| *run as f64 * 1000.0 / RATE as f64).collect::<Vec<_>>(),
        sources.len()
    );
    assert_eq!(after_short, 0, "150 ms is within the buffer");
    assert_eq!(after_long, 1, "a second is not, once");
    assert!(position > before_long + RATE as u64, "playback goes on");
    assert!(jumps.iter().all(|(from, to)| to - from <= 2), "nothing skipped: {jumps:?}");
    assert!(player.log().contains("ran out of audio (underflow 1)"));
}

/// The numbers behind the buffer's size: for some buffers, the latency the stream settles at and
/// the longest feeder stall it plays through.
#[test]
#[ignore = "needs the private audio server: crates/engine/tools/private-audio.sh"]
fn buffer_sizes() {
    for buffer_ms in [100, 150, 200, 250] {
        let player = Player::new(buffer_ms);
        let source = index_source(player.dir.path(), "1000000001.stereo", 30);
        player.output.set_source(Some(source.clone()), 0);
        sleep(Duration::from_millis(1000));
        let mut latencies = Vec::new();
        for _ in 0..25 {
            latencies.push(ms(player.output.latency_frames()));
            sleep(Duration::from_millis(20));
        }
        let mut survived = Vec::new();
        for stall in [75, 100, 125, 150, 175, 200, 225, 250, 300] {
            let before = player.output.underflows();
            source.stall_ms.store(stall, Ordering::SeqCst);
            sleep(Duration::from_millis(stall + 700));
            survived.push((stall, player.output.underflows() == before));
        }
        player.output.stop();
        let tolerated: Vec<u64> = survived.iter().filter(|(_, ok)| *ok).map(|(stall, _)| *stall).collect();
        let first_loss = survived.iter().find(|(_, ok)| !*ok).map(|(stall, _)| *stall);
        println!(
            "[buffer] {buffer_ms} ms: latency median {:.1} ms (min {:.1}, max {:.1}); stalls played through {tolerated:?}, \
             first underflow at {first_loss:?} ms",
            percentile(&mut latencies.clone(), 0.5),
            percentile(&mut latencies.clone(), 0.0),
            percentile(&mut latencies, 1.0),
        );
    }
}
