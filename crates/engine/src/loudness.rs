//! Integrated loudness (ITU-R BS.1770-4 / EBU R128) of prepared PCM sources.
//!
//! K-weighting is the two-biquad cascade of the recommendation (the +4 dB high shelf and the RLB
//! high-pass, 48 kHz coefficients). [`Meter`] does not run the recursion sample by sample: the
//! cascade's impulse response (8192 taps: the slowest pole, radius 0.995, is below -200 dB there)
//! is convolved with the signal by FFT overlap-add, chunk by chunk, exactly as the Python meter
//! does, so both report the same numbers. Gating blocks are 400 ms with a 100 ms hop (built from
//! 100 ms sub-block energies), absolute gate -70 LUFS, relative gate -10 LU, channel weights 1.0
//! for the stereo pair.
//!
//! 16-channel renders are measured through the PEQdB stereo fold (what the user's downmix sink
//! plays): FL/FR straight, FC at -3 dB into both sides, every other channel of a side at 1.0,
//! LFE at +7.1 dB into both (without the sink's 125 Hz low-pass; the E-AC-3 LFE channel is
//! band-limited by the encoder anyway).
//!
//! Sound Check: [`trim_db`] is the attenuation that brings a track to [`TARGET_LUFS`]; never a
//! boost.
//!
//! [`StreamMeter`] is the production path: the same measurement through ffmpeg's `ebur128`
//! filter, fed progressively while a render grows, so a running integrated value is known after
//! the first seconds of audio and the final one the moment the file is complete. [`Meter`]
//! remains the reference (tests) and the fallback without ffmpeg.
//!
//! Differences from `amui/loudness.py`:
//!
//! * numpy arrays are interleaved slices with a channel count: `Meter::feed(block, channels)`,
//!   `integrated_loudness(signal, channels, rate)`, `write_wav_float32(path, data, channels,
//!   rate)`; a fold is a slice of `[left, right]` rows, one per input channel. Results are
//!   structs ([`Measurement`], [`StoredLoudness`]) instead of dicts; `Measurement::to_json()` is
//!   the dict that is stored. Default arguments are explicit ([`TARGET_LUFS`], [`SETTLE`],
//!   [`FINISH_TIMEOUT`]), and what Python reads as attributes of a meter are methods (`frames()`,
//!   `peak()`, `fed()`).
//! * The FFT is written out here (radix 2; the two channels of a pair are filtered as one complex
//!   signal) instead of coming from numpy. On the test signals the meter agrees with the Python
//!   one to better than 1e-9 LU (a test holds the numbers of a Python run, printed by
//!   `tests/reference/loudness_values.py`). The filter tables are built once per process; Python
//!   keeps the impulse response and rebuilds the spectrum for every meter. A release build
//!   measures stereo at about 100 times real time (numpy: about 230), a five-minute track in
//!   under three seconds.
//! * Errors are [`LoudnessError`] with the Python texts (Python raises `RuntimeError`, and
//!   `OSError` for a file that cannot be read).
//! * [`StreamMeter`] reads the file at the offset of the frames it has fed (the Python meter
//!   relies on the file position, which a read ending inside a frame would shift), keeps ffmpeg's
//!   error output in an anonymous memory file instead of a temporary file, and is closed when it
//!   is dropped.
//! * [`stored_loudness`] treats a stored value that is not a number as no measurement (Python
//!   raises and its caller ignores the file), and reads the peak that [`store_loudness`] wrote
//!   into meta.json (Python wrote `loudness_peak` and read `peak`).
//! * [`fold_matrix`] puts the left wide channel (FLW) on the left, where the downmix sink plays
//!   it; the Python matrix went by the last letter of a channel's name and put it on the right.
//!   A 16-channel render therefore measures a little differently (a few hundredths of a LU on
//!   the test signal) from the value the Python engine stored for it.
//!
//! [`ffmpeg_available`] looks along the process's `PATH`, as `shutil.which` does: the one place
//! this module reads the environment (it is where `ffmpeg` is run from as well).

use std::f64::consts::PI;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{CHANNEL_MAP, CHANNELS, SAMPLE_RATE};
use crate::util::{self, Event};

/// Apple Music's Sound Check target (the user's old client had it on).
pub const TARGET_LUFS: f64 = -16.0;
pub const ABSOLUTE_GATE_LUFS: f64 = -70.0;
pub const RELATIVE_GATE_LU: f64 = -10.0;
pub const BLOCK_S: f64 = 0.400;
pub const HOP_S: f64 = 0.100;
pub const IR_TAPS: usize = 8192;
/// 2.73 s per FFT chunk (FFT size 2^18 with the impulse response).
pub const CHUNK_FRAMES: usize = 1 << 17;
/// Next to render-summary.json (stereo decodes keep the values in meta.json).
pub const LOUDNESS_FILE: &str = "loudness.json";

/// BS.1770-4 K-weighting at 48 kHz, first stage (high shelf): (b0, b1, b2, a1, a2).
pub const STAGE1: [f64; 5] =
    [1.53512485958697, -2.69169618940638, 1.19839281085285, -1.69065929318241, 0.73248077421585];
/// Second stage (RLB high-pass): (b0, b1, b2, a1, a2).
pub const STAGE2: [f64; 5] = [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621];

/// What a measurement can fail with; the texts are the Python module's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoudnessError {
    /// The caller's cancel flag was set.
    #[error("loudness measurement cancelled")]
    Cancelled,
    /// ffmpeg could not be started, stopped early, failed or did not finish.
    #[error("{0}")]
    Ffmpeg(String),
    /// The PCM file could not be opened or read.
    #[error("{0}")]
    Io(String),
}

/// Direct-form biquad, sample by sample (the reference implementation; the meter uses it for the
/// impulse response only).
pub fn biquad(x: &[f64], coefficients: &[f64; 5]) -> Vec<f64> {
    let [b0, b1, b2, a1, a2] = *coefficients;
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&x0| {
            let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            (x2, x1, y2, y1) = (x1, x0, y1, y0);
            y0
        })
        .collect()
}

/// Impulse response of the K-weighting cascade ([`IR_TAPS`] taps is what the meter uses).
pub fn k_weighting_ir(taps: usize) -> Vec<f64> {
    let mut impulse = vec![0.0; taps];
    if let Some(first) = impulse.first_mut() {
        *first = 1.0;
    }
    biquad(&biquad(&impulse, &STAGE1), &STAGE2)
}

/// Fold gains of the centre (-3 dB) and the LFE (+7.1 dB): float32 of the downmix preset's own
/// decimals, as in the Python module.
#[allow(clippy::approx_constant)]
const FOLD_CENTRE: f32 = 0.70710678_f64 as f32;
const FOLD_LFE: f32 = 2.26464_f64 as f32;

/// The PEQdB stereo fold in the output channel order ([`CHANNEL_MAP`]): one `[left, right]` row
/// per channel.
///
/// A channel goes to the side its name says, as the downmix sink plays it. (The Python module
/// went by the last letter of the name, which put FLW on the right.)
pub fn fold_matrix() -> [[f32; 2]; CHANNELS] {
    let mut matrix = [[0.0; 2]; CHANNELS];
    for (row, name) in matrix.iter_mut().zip(CHANNEL_MAP) {
        *row = match name {
            "FL" => [1.0, 0.0],
            "FR" => [0.0, 1.0],
            "FC" => [FOLD_CENTRE, FOLD_CENTRE],
            "LFE" => [FOLD_LFE, FOLD_LFE],
            "FLW" => [1.0, 0.0],
            "FRW" => [0.0, 1.0],
            side if side.ends_with('L') => [1.0, 0.0],
            _ => [0.0, 1.0],
        };
    }
    matrix
}

/// Interleaved frames of `fold.len()` channels folded to interleaved stereo (float32 arithmetic,
/// as the Python fold).
fn fold_frames(samples: &[f32], fold: &[[f32; 2]], out: &mut Vec<f32>) {
    out.clear();
    for frame in samples.chunks_exact(fold.len()) {
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for (sample, gains) in frame.iter().zip(fold) {
            left += sample * gains[0];
            right += sample * gains[1];
        }
        out.push(left);
        out.push(right);
    }
}

// --- FFT filter ------------------------------------------------------------------------------

/// Radix-2 transforms of one size without the reordering passes: `forward` leaves the spectrum
/// in bit-reversed order and `inverse` expects it in that order, which is all a convolution
/// needs.
struct Fft {
    size: usize,
    /// cos and sin of 2 pi k / size for k below size / 2.
    cos: Vec<f64>,
    sin: Vec<f64>,
}

impl Fft {
    fn new(size: usize) -> Fft {
        assert!(size.is_power_of_two() && size >= 2, "an FFT size is a power of two");
        let (sin, cos): (Vec<f64>, Vec<f64>) =
            (0..size / 2).map(|k| (2.0 * PI * k as f64 / size as f64).sin_cos()).unzip();
        Fft { size, cos, sin }
    }

    /// The discrete Fourier transform of `re + i im`, in place; the bins come out in bit-reversed
    /// order.
    fn forward(&self, re: &mut [f64], im: &mut [f64]) {
        assert!(re.len() == self.size && im.len() == self.size);
        let mut half = self.size / 2;
        let mut stride = 1;
        while half >= 1 {
            for start in (0..self.size).step_by(2 * half) {
                for j in 0..half {
                    let (wr, wi) = (self.cos[j * stride], -self.sin[j * stride]);
                    let (a, b) = (start + j, start + j + half);
                    let (dr, di) = (re[a] - re[b], im[a] - im[b]);
                    re[a] += re[b];
                    im[a] += im[b];
                    re[b] = dr * wr - di * wi;
                    im[b] = dr * wi + di * wr;
                }
            }
            half /= 2;
            stride *= 2;
        }
    }

    /// Undoes `forward` up to a factor of `size`: bins in bit-reversed order in, samples out.
    fn inverse(&self, re: &mut [f64], im: &mut [f64]) {
        assert!(re.len() == self.size && im.len() == self.size);
        let mut half = 1;
        let mut stride = self.size / 2;
        while half < self.size {
            for start in (0..self.size).step_by(2 * half) {
                for j in 0..half {
                    let (wr, wi) = (self.cos[j * stride], self.sin[j * stride]);
                    let (a, b) = (start + j, start + j + half);
                    let (tr, ti) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            half *= 2;
            stride /= 2;
        }
    }
}

/// The K-weighting impulse response as an FFT filter for chunks of up to [`CHUNK_FRAMES`].
struct KWeighting {
    fft: Fft,
    /// Spectrum of the impulse response in the order `Fft::forward` produces, divided by the FFT
    /// size (so `Fft::inverse` needs no scaling).
    response_re: Vec<f64>,
    response_im: Vec<f64>,
}

impl KWeighting {
    fn new() -> KWeighting {
        let mut size = 1;
        while size < CHUNK_FRAMES + IR_TAPS - 1 {
            size *= 2;
        }
        let fft = Fft::new(size);
        let mut response_re = k_weighting_ir(IR_TAPS);
        response_re.resize(size, 0.0);
        let mut response_im = vec![0.0; size];
        fft.forward(&mut response_re, &mut response_im);
        for value in response_re.iter_mut().chain(response_im.iter_mut()) {
            *value /= size as f64;
        }
        KWeighting { fft, response_re, response_im }
    }

    /// Built on first use and kept: every meter of the process shares the tables.
    fn shared() -> &'static KWeighting {
        static TABLES: OnceLock<KWeighting> = OnceLock::new();
        TABLES.get_or_init(KWeighting::new)
    }

    /// Convolve two real signals (`re` and `im`, zero-padded to the FFT size) with the impulse
    /// response in one pass: the filter is real, so the two never mix.
    fn apply(&self, re: &mut [f64], im: &mut [f64]) {
        self.fft.forward(re, im);
        let response = self.response_re.iter().zip(&self.response_im);
        for ((re, im), (hr, hi)) in re.iter_mut().zip(im.iter_mut()).zip(response) {
            (*re, *im) = (*re * hr - *im * hi, *re * hi + *im * hr);
        }
        self.fft.inverse(re, im);
    }
}

/// Feed stereo (or any channel count with unit weights) blocks in order; read the integrated
/// loudness.
pub struct Meter {
    rate: u32,
    /// Frames per 100 ms sub-block.
    sub_frames: usize,
    /// Sub-blocks per gating block (4).
    block_subs: usize,
    filter: &'static KWeighting,
    /// Overlap-add carry per channel.
    tails: Vec<Vec<f64>>,
    /// Weighted squares waiting for a full sub-block: their sum and count.
    pending_sum: f64,
    pending_count: usize,
    /// Sum over channels of the sum of squares per 100 ms sub-block.
    sub_energies: Vec<f64>,
    frames: u64,
    peak: f64,
    re: Vec<f64>,
    im: Vec<f64>,
    squares: Vec<f64>,
}

impl Meter {
    pub fn new(rate: u32) -> Meter {
        Meter {
            rate,
            sub_frames: (HOP_S * f64::from(rate)).round_ties_even() as usize,
            block_subs: (BLOCK_S / HOP_S).round_ties_even() as usize,
            filter: KWeighting::shared(),
            tails: Vec::new(),
            pending_sum: 0.0,
            pending_count: 0,
            sub_energies: Vec::new(),
            frames: 0,
            peak: 0.0,
            re: Vec::new(),
            im: Vec::new(),
            squares: Vec::new(),
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Frames fed so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Largest absolute sample fed so far.
    pub fn peak(&self) -> f64 {
        self.peak
    }

    /// Sum over channels of the K-weighted sum of squares of every complete 100 ms sub-block.
    pub fn sub_energies(&self) -> &[f64] {
        &self.sub_energies
    }

    /// `block`: interleaved frames of `channels` channels, which are summed with weight 1.0
    /// (stereo after the fold). Every block of one meter has the same channel count.
    pub fn feed<S: Copy + Into<f64>>(&mut self, block: &[S], channels: usize) {
        assert!(channels > 0, "a block has at least one channel");
        let frames = block.len() / channels;
        if frames == 0 {
            return;
        }
        let block = &block[..frames * channels];
        self.frames += frames as u64;
        self.peak = block.iter().fold(self.peak, |peak, &sample| peak.max(sample.into().abs()));
        for chunk in block.chunks(CHUNK_FRAMES * channels) {
            self.feed_chunk(chunk, channels);
        }
    }

    fn feed_chunk<S: Copy + Into<f64>>(&mut self, chunk: &[S], channels: usize) {
        let n = chunk.len() / channels;
        if self.tails.is_empty() {
            self.tails = vec![vec![0.0; IR_TAPS - 1]; channels];
            self.re = vec![0.0; self.filter.fft.size];
            self.im = vec![0.0; self.filter.fft.size];
            self.squares = vec![0.0; CHUNK_FRAMES];
        }
        assert_eq!(self.tails.len(), channels, "a meter is fed one channel count");
        self.squares[..n].fill(0.0);
        for first in (0..channels).step_by(2) {
            let paired = first + 1 < channels;
            for (index, frame) in chunk.chunks_exact(channels).enumerate() {
                self.re[index] = frame[first].into();
                self.im[index] = if paired { frame[first + 1].into() } else { 0.0 };
            }
            self.re[n..].fill(0.0);
            self.im[n..].fill(0.0);
            self.filter.apply(&mut self.re, &mut self.im);
            Self::collect(&mut self.re, &mut self.tails[first], &mut self.squares[..n]);
            if paired {
                Self::collect(&mut self.im, &mut self.tails[first + 1], &mut self.squares[..n]);
            }
        }
        for &square in &self.squares[..n] {
            self.pending_sum += square;
            self.pending_count += 1;
            if self.pending_count == self.sub_frames {
                self.sub_energies.push(self.pending_sum);
                self.pending_sum = 0.0;
                self.pending_count = 0;
            }
        }
    }

    /// Overlap-add for one channel: add the carry of the previous chunk, add the squares of the
    /// chunk's own frames to `squares`, keep what reaches into the next chunk.
    fn collect(filtered: &mut [f64], tail: &mut [f64], squares: &mut [f64]) {
        for (value, carried) in filtered.iter_mut().zip(tail.iter()) {
            *value += carried;
        }
        for (square, value) in squares.iter_mut().zip(filtered.iter()) {
            *square += value * value;
        }
        let n = squares.len();
        tail.copy_from_slice(&filtered[n..n + tail.len()]);
    }

    /// Gated integrated loudness in LUFS, `None` when nothing passes the absolute gate.
    pub fn integrated(&self) -> Option<f64> {
        if self.block_subs == 0 || self.sub_energies.len() < self.block_subs {
            return None;
        }
        let block_frames = (self.block_subs * self.sub_frames) as f64;
        let level = |power: f64| -0.691 + 10.0 * power.max(1e-30).log10();
        let mean = |powers: &[f64]| powers.iter().sum::<f64>() / powers.len() as f64;
        // mean square, summed over channels, of every 400 ms block (100 ms apart)
        let above: Vec<f64> = self
            .sub_energies
            .windows(self.block_subs)
            .map(|block| block.iter().sum::<f64>() / block_frames)
            .filter(|&power| level(power) > ABSOLUTE_GATE_LUFS)
            .collect();
        if above.is_empty() {
            return None;
        }
        let relative_gate = -0.691 + 10.0 * mean(&above).log10() + RELATIVE_GATE_LU;
        let gated: Vec<f64> = above.into_iter().filter(|&power| level(power) > relative_gate).collect();
        if gated.is_empty() {
            return None;
        }
        Some(-0.691 + 10.0 * mean(&gated).log10())
    }
}

/// Convenience: the integrated loudness of interleaved frames of `channels` channels.
pub fn integrated_loudness<S: Copy + Into<f64>>(signal: &[S], channels: usize, rate: u32) -> Option<f64> {
    let mut meter = Meter::new(rate);
    meter.feed(signal, channels);
    meter.integrated()
}

/// A finished measurement: what `measure_file()` and `StreamMeter::finish()` return and what is
/// stored next to the PCM file.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Integrated loudness, `None` when nothing passes the absolute gate (silence).
    pub lufs: Option<f64>,
    /// Largest absolute sample: of the folded signal for `measure_file()`, of the file's own
    /// channels for the stream meter.
    pub peak: f64,
    pub frames: u64,
    /// "peqdb" when the channels were folded to stereo first.
    pub fold: Option<&'static str>,
    /// Seconds since the Unix epoch.
    pub measured_at: f64,
}

impl Measurement {
    pub fn to_json(&self) -> Value {
        json!({
            "lufs": self.lufs,
            "peak": self.peak,
            "frames": self.frames,
            "fold": self.fold,
            "measured_at": self.measured_at,
        })
    }
}

fn decode_f32le(bytes: &[u8], out: &mut Vec<f32>) {
    out.clear();
    out.extend(bytes.as_chunks::<4>().0.iter().map(|sample| f32::from_le_bytes(*sample)));
}

/// Integrated loudness of an interleaved f32le file; 16-channel files are folded with `fold`
/// (default: the PEQdB matrix). `frames` limits the measurement to the first frames of the file.
pub fn measure_file(
    pcm_path: &Path,
    channels: usize,
    frames: Option<u64>,
    fold: Option<&[[f32; 2]]>,
    cancel: Option<&Event>,
) -> Result<Measurement, LoudnessError> {
    let failed = |error: io::Error| LoudnessError::Io(format!("{}: '{}'", os_error_text(&error), pcm_path.display()));
    if channels == 0 {
        return Err(LoudnessError::Io("a PCM file has at least one channel".into()));
    }
    let frame_bytes = channels * 4;
    let default_fold = fold_matrix();
    let fold = fold.or((channels == CHANNELS).then_some(&default_fold[..]));
    if let Some(fold) = fold
        && fold.len() != channels
    {
        return Err(LoudnessError::Io(format!("a fold of {} channels cannot fold {channels}", fold.len())));
    }
    let mut meter = Meter::new(SAMPLE_RATE);
    let total = match frames {
        Some(frames) => frames,
        None => fs::metadata(pcm_path).map_err(failed)?.len() / frame_bytes as u64,
    };
    let mut file = File::open(pcm_path).map_err(failed)?;
    let mut bytes = vec![0u8; (CHUNK_FRAMES as u64).min(total) as usize * frame_bytes];
    let (mut samples, mut folded) = (Vec::new(), Vec::new());
    let mut remaining = total;
    while remaining > 0 {
        if cancel.is_some_and(Event::is_set) {
            return Err(LoudnessError::Cancelled);
        }
        let count = (CHUNK_FRAMES as u64).min(remaining) as usize;
        let got = read_full(&mut file, &mut bytes[..count * frame_bytes]).map_err(failed)? / frame_bytes;
        if got == 0 {
            break;
        }
        decode_f32le(&bytes[..got * frame_bytes], &mut samples);
        match fold {
            Some(fold) => {
                fold_frames(&samples, fold, &mut folded);
                meter.feed(&folded, 2);
            }
            None => meter.feed(&samples, channels),
        }
        remaining -= got as u64;
    }
    Ok(Measurement {
        lufs: meter.integrated(),
        peak: meter.peak(),
        frames: meter.frames(),
        fold: fold.map(|_| "peqdb"),
        measured_at: util::unix_now(),
    })
}

// --- ffmpeg ebur128 stream meter -------------------------------------------------------------

pub const FFMPEG: &str = "ffmpeg";
/// ebur128 reports one metadata frame per 100 ms of audio.
const FRAME_S: f64 = 0.1;
/// How long `StreamMeter::feed()` gives ffmpeg to report the frames it was fed.
pub const SETTLE: Duration = Duration::from_millis(250);
/// How long `StreamMeter::finish()` waits for ffmpeg unless told otherwise.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(60);
const R128_KEY: &[u8] = b"lavfi.r128.I=";

/// True when an `ffmpeg` program is on the search path.
pub fn ffmpeg_available() -> bool {
    which(FFMPEG).is_some()
}

/// The first executable called `program` on the process's search path (what the shell would
/// run).
fn which(program: &str) -> Option<PathBuf> {
    let search = std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
    std::env::split_paths(&search).map(|folder| folder.join(program)).find(|candidate| {
        let Ok(name) = std::ffi::CString::new(candidate.as_os_str().as_bytes()) else { return false };
        // SAFETY: `name` is a valid C string for the duration of the call.
        candidate.is_file() && unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0
    })
}

/// ffmpeg `pan` filter folding `fold.len()` input channels to stereo with the matrix
/// (`fold_matrix()` for 16).
pub fn pan_expression(fold: &[[f32; 2]]) -> String {
    let outputs: Vec<String> = (0..2)
        .map(|out| {
            let terms: Vec<String> = fold
                .iter()
                .enumerate()
                .filter(|(_, gains)| f64::from(gains[out]).abs() > 1e-9)
                .map(|(channel, gains)| format!("{:.6}*c{channel}", gains[out]))
                .collect();
            format!("c{out}={}", if terms.is_empty() { "0*c0".to_string() } else { terms.join("+") })
        })
        .collect();
    format!("pan=stereo|{}", outputs.join("|"))
}

#[derive(Default)]
struct StreamState {
    /// Metadata frames reported by ffmpeg.
    lines: u64,
    last: Option<f64>,
    /// The reader thread reached the end of ffmpeg's output.
    ended: bool,
}

#[derive(Default)]
struct StreamShared {
    state: Mutex<StreamState>,
    ready: Condvar,
}

/// Running integrated loudness of a growing (or complete) interleaved f32le file through
/// ffmpeg's ebur128 filter: BS.1770-4 K-weighting and gating like [`Meter`] (agreement within
/// 0.01 LU on test signals), 16-channel renders folded with the PEQdB matrix by `pan`.
/// `feed(frames)` pushes the file up to that frame and returns the integrated loudness of
/// everything fed so far (`None` before 0.4 s or below the absolute gate); `finish()` ends the
/// stream and returns the `measure_file()`-shaped result. `close()` kills a running ffmpeg.
pub struct StreamMeter {
    pcm_path: PathBuf,
    channels: usize,
    rate: u32,
    frame_bytes: usize,
    folded: bool,
    /// Frames written to ffmpeg so far.
    fed: u64,
    peak: f64,
    file: Option<File>,
    stderr: Option<File>,
    child: Child,
    stdin: Option<ChildStdin>,
    shared: Arc<StreamShared>,
    reader: Option<JoinHandle<()>>,
    buffer: Vec<u8>,
}

impl StreamMeter {
    /// Start ffmpeg for the file. `fold`: one `[left, right]` row per channel; `None` folds 16
    /// channels with the PEQdB matrix and measures any other channel count as it is.
    pub fn new(
        pcm_path: &Path,
        channels: usize,
        fold: Option<&[[f32; 2]]>,
        rate: u32,
    ) -> Result<StreamMeter, LoudnessError> {
        let default_fold = fold_matrix();
        let fold = fold.or((channels == CHANNELS).then_some(&default_fold[..]));
        let file = File::open(pcm_path)
            .map_err(|error| LoudnessError::Io(format!("{}: '{}'", os_error_text(&error), pcm_path.display())))?;
        let stderr = anonymous_file().map_err(|error| LoudnessError::Io(os_error_text(&error)))?;
        let child_stderr = stderr.try_clone().map_err(|error| LoudnessError::Io(os_error_text(&error)))?;
        let filters = format!(
            "{}ebur128=peak=none:metadata=1,ametadata=mode=print:key=lavfi.r128.I:file=-:direct=1",
            fold.map(|fold| pan_expression(fold) + ",").unwrap_or_default()
        );
        let mut child = Command::new(FFMPEG)
            .args(["-nostdin", "-v", "error", "-f", "f32le", "-ar", &rate.to_string(), "-ac", &channels.to_string()])
            .args(["-i", "pipe:0", "-af", &filters, "-f", "null", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(child_stderr)
            .spawn()
            .map_err(|error| {
                LoudnessError::Ffmpeg(format!("ffmpeg could not be started: {}: '{FFMPEG}'", os_error_text(&error)))
            })?;
        let stdin = child.stdin.take();
        let shared = Arc::new(StreamShared::default());
        let reader = child.stdout.take().and_then(|stdout| {
            let shared = shared.clone();
            thread::Builder::new().name("am-ui-ebur128".into()).spawn(move || read_stdout(stdout, &shared)).ok()
        });
        let mut meter = StreamMeter {
            pcm_path: pcm_path.to_path_buf(),
            channels,
            rate,
            frame_bytes: channels.max(1) * 4,
            folded: fold.is_some(),
            fed: 0,
            peak: 0.0,
            file: Some(file),
            stderr: Some(stderr),
            child,
            stdin,
            shared,
            reader,
            buffer: Vec::new(),
        };
        if meter.reader.is_none() {
            meter.close();
            return Err(LoudnessError::Ffmpeg("ffmpeg could not be started: no reader thread".into()));
        }
        Ok(meter)
    }

    pub fn pcm_path(&self) -> &Path {
        &self.pcm_path
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames written to ffmpeg so far.
    pub fn fed(&self) -> u64 {
        self.fed
    }

    /// Largest absolute sample fed so far (of the file's own channels, before the fold).
    pub fn peak(&self) -> f64 {
        self.peak
    }

    /// The end of ffmpeg's error output.
    fn error_text(&mut self) -> String {
        let Some(stderr) = self.stderr.as_mut() else { return String::new() };
        let mut text = Vec::new();
        if stderr.seek(SeekFrom::Start(0)).is_err() || stderr.read_to_end(&mut text).is_err() {
            return String::new();
        }
        String::from_utf8_lossy(&text[text.len().saturating_sub(400)..]).trim().to_string()
    }

    /// Push the file up to `frames_available` frames (what the writer has produced) and return
    /// the running integrated loudness. Returns early when the file is shorter than announced
    /// (the writer is mid-flush).
    pub fn feed(&mut self, frames_available: u64) -> Result<Option<f64>, LoudnessError> {
        let target = frames_available.max(self.fed);
        while self.fed < target {
            let count = (CHUNK_FRAMES as u64).min(target - self.fed) as usize;
            if self.buffer.len() < count * self.frame_bytes {
                self.buffer.resize(count * self.frame_bytes, 0);
            }
            let Some(file) = self.file.as_ref() else { break };
            let offset = self.fed * self.frame_bytes as u64;
            let read = read_full_at(file, &mut self.buffer[..count * self.frame_bytes], offset)
                .map_err(|error| LoudnessError::Io(os_error_text(&error)))?;
            let got = read / self.frame_bytes;
            if got == 0 {
                break;
            }
            let data = &self.buffer[..got * self.frame_bytes];
            let samples = data.as_chunks::<4>().0.iter();
            self.peak = samples.fold(self.peak, |peak, sample| peak.max(f64::from(f32::from_le_bytes(*sample)).abs()));
            let written = match self.stdin.as_mut() {
                Some(stdin) => stdin.write_all(data),
                None => Err(io::ErrorKind::BrokenPipe.into()),
            };
            if written.is_err() {
                let text = self.error_text();
                let text = if text.is_empty() { "no error output" } else { &text };
                return Err(LoudnessError::Ffmpeg(format!("ffmpeg ebur128 stopped: {text}")));
            }
            self.fed += got as u64;
        }
        Ok(self.running(SETTLE))
    }

    /// The latest integrated value, after giving ffmpeg a moment (`settle` at most) to report
    /// the frames fed so far.
    pub fn running(&mut self, settle: Duration) -> Option<f64> {
        // the last frames may still sit in ffmpeg's input
        let expected = (self.fed as f64 / (f64::from(self.rate) * FRAME_S)) as i64 - 2;
        let deadline = Instant::now() + settle;
        let mut state = lock(&self.shared.state);
        while (state.lines as i64) < expected && matches!(self.child.try_wait(), Ok(None)) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self.shared.ready.wait_timeout(state, deadline - now).unwrap_or_else(|error| error.into_inner()).0;
        }
        state.last
    }

    /// Close the stream, wait for ffmpeg (`timeout` at most) and return the measurement.
    pub fn finish(&mut self, timeout: Duration) -> Result<Measurement, LoudnessError> {
        self.stdin = None;
        let Some(code) = poll_until(timeout, || exit_code(&mut self.child)) else {
            self.close();
            return Err(LoudnessError::Ffmpeg("ffmpeg ebur128 did not finish in time".into()));
        };
        self.join_reader(Duration::from_secs(5));
        self.file = None;
        if code != 0 {
            let text = self.error_text();
            self.stderr = None;
            let text = if text.is_empty() { "no error output" } else { &text };
            return Err(LoudnessError::Ffmpeg(format!("ffmpeg ebur128 exited with {code}: {text}")));
        }
        self.stderr = None;
        Ok(Measurement {
            lufs: lock(&self.shared.state).last,
            peak: self.peak,
            frames: self.fed,
            fold: self.folded.then_some("peqdb"),
            measured_at: util::unix_now(),
        })
    }

    /// Wait for the reader thread to see the end of ffmpeg's output.
    fn join_reader(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        while !state.ended {
            let now = Instant::now();
            if now >= deadline {
                return; // still reading: leave the thread to end on its own
            }
            state = self.shared.ready.wait_timeout(state, deadline - now).unwrap_or_else(|error| error.into_inner()).0;
        }
        drop(state);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    /// Abandon the measurement (cancel): kill ffmpeg, close the files. Does nothing the second
    /// time.
    pub fn close(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            poll_until(Duration::from_secs(5), || exit_code(&mut self.child));
        }
        self.stdin = None;
        self.file = None;
        self.stderr = None;
    }
}

impl Drop for StreamMeter {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reader thread: the latest `lavfi.r128.I` value of ffmpeg's metadata output.
fn read_stdout(stdout: ChildStdout, shared: &StreamShared) {
    let mut lines = BufReader::new(stdout);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match lines.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Some(value) = raw.strip_prefix(R128_KEY) else { continue };
        let Some(value) = std::str::from_utf8(value).ok().and_then(|text| text.trim().parse::<f64>().ok()) else {
            continue;
        };
        let mut state = lock(&shared.state);
        state.lines += 1;
        state.last = (value > ABSOLUTE_GATE_LUFS).then_some(value);
        shared.ready.notify_all();
    }
    lock(&shared.state).ended = true;
    shared.ready.notify_all();
}

/// An unnamed file in memory (what Python's `tempfile.TemporaryFile()` is used for).
fn anonymous_file() -> io::Result<File> {
    // SAFETY: the name is a valid C string; the call has no other requirements.
    let fd = unsafe { libc::memfd_create(c"am-ui-ebur128".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor this call just created and nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Sound Check trim: attenuation toward the target, never a boost, 0 when unknown.
pub fn trim_db(lufs: Option<f64>, target: f64) -> f64 {
    let Some(lufs) = lufs.filter(|lufs| lufs.is_finite()) else { return 0.0 };
    // two decimals, rounded as Python's round(x, 2) does: through the shortest decimal text
    let rounded = format!("{:.2}", target - lufs).parse::<f64>().unwrap_or(0.0);
    if rounded < 0.0 { rounded } else { 0.0 }
}

// --- persistence next to the PCM files -------------------------------------------------------

/// A previous measurement as it was stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredLoudness {
    /// `None`: measured, nothing above the absolute gate.
    pub lufs: Option<f64>,
    pub peak: Option<f64>,
    /// Frames the measurement covered.
    pub frames: Option<u64>,
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn as_frames(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| value.as_f64().filter(|frames| *frames >= 0.0).map(|frames| frames as u64))
}

/// A previous measurement: loudness.json next to a render, or the loudness keys of meta.json.
pub fn stored_loudness(pcm_path: &Path) -> Option<StoredLoudness> {
    for name in [LOUDNESS_FILE, "meta.json"] {
        let Some(data) = util::read_json_object(&pcm_path.with_file_name(name)) else { continue };
        let own_file = name == LOUDNESS_FILE;
        if !data.contains_key("lufs") || !(own_file || truthy(data.get("loudness_frames"))) {
            continue;
        }
        let lufs = match data.get("lufs") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_f64()?),
        };
        return Some(StoredLoudness {
            lufs,
            // meta.json keeps the peak under its own name
            peak: data.get(if own_file { "peak" } else { "loudness_peak" }).and_then(Value::as_f64),
            frames: as_frames(data.get(if own_file { "frames" } else { "loudness_frames" })),
        });
    }
    None
}

/// Renders: loudness.json next to render-summary.json. Stereo/binaural: merged into meta.json
/// (atomic replace, 0600; the frame/channel keys the validators check are left untouched).
pub fn store_loudness(pcm_path: &Path, result: &Measurement) -> io::Result<()> {
    let name = pcm_path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    if name.starts_with("atmos-") {
        return util::write_private_json(&pcm_path.with_file_name(LOUDNESS_FILE), &result.to_json());
    }
    let meta_path = pcm_path.with_file_name("meta.json");
    let Some(mut meta) = util::read_json_object(&meta_path) else { return Ok(()) };
    meta.insert("lufs".into(), json!(result.lufs));
    meta.insert("loudness_peak".into(), json!(result.peak));
    meta.insert("loudness_frames".into(), json!(result.frames));
    meta.insert("loudness_measured_at".into(), json!(result.measured_at));
    util::write_private_json(&meta_path, &Value::Object(meta))
}

/// IEEE-float WAV (format 3) with a fact chunk, created 0600. `data`: interleaved frames of
/// `channels` channels.
pub fn write_wav_float32(path: &Path, data: &[f32], channels: usize, rate: u32) -> io::Result<PathBuf> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "too much audio for a WAV file");
    let channels = channels.max(1);
    let frames = u32::try_from(data.len() / channels).map_err(|_| too_large())?;
    let data = &data[..frames as usize * channels];
    let payload_bytes = u32::try_from(data.len() * 4).map_err(|_| too_large())?;
    let channel_count = u16::try_from(channels).map_err(|_| too_large())?;
    let block_align = u16::try_from(channels * 4).map_err(|_| too_large())?;
    let byte_rate = rate.checked_mul(u32::from(block_align)).ok_or_else(too_large)?;

    let mut format = Vec::with_capacity(16);
    format.extend_from_slice(&3u16.to_le_bytes());
    format.extend_from_slice(&channel_count.to_le_bytes());
    format.extend_from_slice(&rate.to_le_bytes());
    format.extend_from_slice(&byte_rate.to_le_bytes());
    format.extend_from_slice(&block_align.to_le_bytes());
    format.extend_from_slice(&32u16.to_le_bytes());
    let mut body = Vec::with_capacity(48 + data.len() * 4);
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&(format.len() as u32).to_le_bytes());
    body.extend_from_slice(&format);
    body.extend_from_slice(b"fact");
    body.extend_from_slice(&4u32.to_le_bytes());
    body.extend_from_slice(&frames.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&payload_bytes.to_le_bytes());
    let riff_bytes = u32::try_from(body.len() + data.len() * 4).map_err(|_| too_large())?;
    for sample in data {
        body.extend_from_slice(&sample.to_le_bytes());
    }

    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        util::create_private_dirs(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_bytes.to_le_bytes())?;
    file.write_all(&body)?;
    Ok(path.to_path_buf())
}

// --- small helpers shared with the output module ---------------------------------------------

/// A lock that a panic in another thread does not take away.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An operating system error the way Python prints it ("[Errno 32] Broken pipe"); other errors
/// in their own words.
pub(crate) fn os_error_text(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => {
            let reason = text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text);
            format!("[Errno {code}] {reason}")
        }
        None => text,
    }
}

/// What Python calls a process's return code: the exit status, or minus the signal that ended
/// it.
pub(crate) fn return_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(0))
}

/// The return code of a child that has exited, `None` while it runs. A child that cannot be
/// waited for counts as gone.
pub(crate) fn exit_code(child: &mut Child) -> Option<i32> {
    match child.try_wait() {
        Ok(status) => status.map(return_code),
        Err(_) => Some(0),
    }
}

/// Ask `poll` until it has an answer or `timeout` passes, with the pauses Python's
/// `Popen.wait(timeout)` makes (1 ms doubling up to 50 ms).
pub(crate) fn poll_until<T>(timeout: Duration, mut poll: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_micros(500);
    loop {
        if let Some(answer) = poll() {
            return Some(answer);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        delay = (delay * 2).min(deadline - now).min(Duration::from_millis(50));
        thread::sleep(delay);
    }
}

/// Fill `buffer` from the file's position; fewer bytes only at the end of the file.
fn read_full(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Fill `buffer` from `offset` of the file; fewer bytes only at the end of the file.
pub(crate) fn read_full_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read_at(&mut buffer[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    //! BS.1770/R128 integrated loudness (FFT K-weighting against the reference biquads), gating,
    //! the PEQdB fold, the ffmpeg ebur128 stream meter against the FFT meter, Sound Check trims,
    //! persistence next to PCM files and the float WAV writer.
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const RATE: usize = SAMPLE_RATE as usize;

    #[track_caller]
    fn assert_close(actual: f64, expected: f64, delta: f64) {
        assert!((actual - expected).abs() <= delta, "{actual} is not within {delta} of {expected}");
    }

    /// Interleaved stereo: the same tone in both channels.
    fn stereo_sine(seconds: f64, hz: f64, dbfs: f64) -> Vec<f64> {
        let amplitude = 10f64.powf(dbfs / 20.0);
        (0..(seconds * RATE as f64) as usize)
            .flat_map(|index| {
                let tone = amplitude * (2.0 * PI * hz * (index as f64 / RATE as f64)).sin();
                [tone, tone]
            })
            .collect()
    }

    /// Uniform numbers in [-0.5, 0.5) from a 64-bit linear congruential generator. Integer
    /// steps and exact float operations only, so a Python script can rebuild every signal made
    /// from it bit for bit (see `matches_the_python_implementation`).
    struct Uniform(u64);

    impl Uniform {
        fn new(seed: u64, channel: u64) -> Uniform {
            let start = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            Uniform(start.wrapping_add(channel.wrapping_mul(0xBF58_476D_1CE4_E5B9)))
        }

        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / 9_007_199_254_740_992.0 - 0.5
        }
    }

    /// Interleaved stereo noise with a pink spectrum (white noise through Paul Kellet's pinking
    /// filter), both channels scaled to an RMS of 0.05. The Python tests shape numpy's Gaussian
    /// noise in the frequency domain; this is the same kind of signal from a generator both
    /// languages share.
    fn pinkish_noise(seconds: f64, seed: u64) -> Vec<f64> {
        let frames = (seconds * RATE as f64) as usize;
        let mut out = vec![0.0; 2 * frames];
        for channel in 0..2 {
            let mut white_noise = Uniform::new(seed, channel as u64);
            let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for frame in 0..frames {
                let white = white_noise.next();
                b0 = 0.99886 * b0 + white * 0.0555179;
                b1 = 0.99332 * b1 + white * 0.0750759;
                b2 = 0.96900 * b2 + white * 0.1538520;
                b3 = 0.86650 * b3 + white * 0.3104856;
                b4 = 0.55000 * b4 + white * 0.5329522;
                b5 = -0.7616 * b5 - white * 0.0168980;
                out[2 * frame + channel] = b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362;
                b6 = white * 0.115926;
            }
        }
        let mut total = 0.0;
        for value in &out {
            total += value * value;
        }
        let scale = 0.05 / (total / out.len() as f64).sqrt();
        out.iter().map(|value| value * scale).collect()
    }

    /// Interleaved stereo white noise with the given RMS.
    fn white_noise(frames: usize, seed: u64, rms: f64) -> Vec<f64> {
        let mut noise = Uniform::new(seed, 0);
        (0..2 * frames).map(|_| noise.next() * 12f64.sqrt() * rms).collect()
    }

    /// Sample-by-sample biquads and literal 400 ms / 100 ms gating blocks (slow; short signals
    /// only). `signal`: interleaved stereo.
    fn reference_loudness(signal: &[f64]) -> f64 {
        let frames = signal.len() / 2;
        let mut squares = vec![0.0; frames];
        for channel in 0..2 {
            let samples: Vec<f64> = signal.iter().skip(channel).step_by(2).copied().collect();
            let weighted = biquad(&biquad(&samples, &STAGE1), &STAGE2);
            for (square, value) in squares.iter_mut().zip(&weighted) {
                *square += value * value;
            }
        }
        let (hop, block) = ((0.1 * RATE as f64) as usize, (0.4 * RATE as f64) as usize);
        let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
        let level = |power: f64| -0.691 + 10.0 * power.log10();
        let powers: Vec<f64> =
            (0..=frames - block).step_by(hop).map(|start| mean(&squares[start..start + block])).collect();
        let above: Vec<f64> = powers.iter().copied().filter(|&power| level(power) > -70.0).collect();
        let gate = level(mean(&above)) - 10.0;
        let gated: Vec<f64> = above.into_iter().filter(|&power| level(power) > gate).collect();
        level(mean(&gated))
    }

    fn loudness_of(signal: &[f64]) -> Option<f64> {
        integrated_loudness(signal, 2, SAMPLE_RATE)
    }

    fn as_f32(signal: &[f64]) -> Vec<f32> {
        signal.iter().map(|&sample| sample as f32).collect()
    }

    fn write_f32le(path: &Path, samples: &[f32]) {
        let bytes: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        fs::write(path, bytes).unwrap();
    }

    fn append_f32le(path: &Path, samples: &[f32]) {
        let bytes: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        OpenOptions::new().append(true).open(path).unwrap().write_all(&bytes).unwrap();
    }

    /// Sixteen channels built from a stereo signal: every channel a rotated copy of one of the
    /// two at its own level.
    fn wide_from(noise: &[f64]) -> Vec<f32> {
        let frames = noise.len() / 2;
        let mut wide = vec![0.0f32; frames * 16];
        for channel in 0..16 {
            let gain = 0.3 + 0.05 * channel as f64;
            for frame in 0..frames {
                let from = (frame + frames - (97 * channel) % frames) % frames;
                wide[frame * 16 + channel] = (noise[from * 2 + channel % 2] * gain) as f32;
            }
        }
        wide
    }

    #[test]
    fn reference_sine_levels() {
        // EBU Tech 3341: a 1 kHz stereo sine at -23 dBFS reads -23.0 LUFS (+-0.1)
        assert_close(loudness_of(&stereo_sine(6.0, 1000.0, -23.0)).unwrap(), -23.0, 0.1);
        assert_close(loudness_of(&stereo_sine(6.0, 997.0, -20.0)).unwrap(), -20.0, 0.1);
        let mut mono = stereo_sine(6.0, 1000.0, -23.0);
        mono.iter_mut().skip(1).step_by(2).for_each(|right| *right = 0.0);
        assert_close(loudness_of(&mono).unwrap(), -26.0, 0.1); // one channel: -3 dB
    }

    #[test]
    fn fft_filter_matches_the_reference_biquads() {
        let noise = pinkish_noise(3.0, 1);
        let fast = loudness_of(&noise).unwrap();
        assert_close(fast, reference_loudness(&noise), 0.01);
        let gain = 10f64.powf((-23.0 - fast) / 20.0);
        let scaled: Vec<f64> = noise.iter().map(|sample| sample * gain).collect();
        assert_close(loudness_of(&scaled).unwrap(), -23.0, 0.05);
    }

    #[test]
    fn gating_ignores_quiet_passages_and_silence() {
        let loud = stereo_sine(5.0, 1000.0, -23.0);
        let quiet = white_noise(5 * RATE, 2, 1e-4); // about -80 dBFS
        let mut with_tail = loud.clone();
        with_tail.extend_from_slice(&quiet);
        with_tail.extend(std::iter::repeat_n(0.0, 2 * RATE * 2));
        assert_close(loudness_of(&with_tail).unwrap(), -23.0, 0.15);
        assert_eq!(loudness_of(&vec![0.0; 3 * RATE * 2]), None, "silence has no loudness");
        assert_eq!(loudness_of(&quiet), None, "below the absolute gate");
    }

    #[test]
    fn chunked_feeding_is_exact() {
        let noise = pinkish_noise(4.0, 1);
        let whole = loudness_of(&noise).unwrap();
        let mut meter = Meter::new(SAMPLE_RATE);
        for chunk in noise.chunks(7001 * 2) {
            // odd chunk sizes across FFT and sub-block boundaries
            meter.feed(chunk, 2);
        }
        assert_close(meter.integrated().unwrap(), whole, 1e-6);
    }

    #[test]
    #[allow(clippy::approx_constant)] // 0.70710678 is the fold's own constant, not 1/sqrt(2)
    fn fold_matrix_and_16_channel_measurement() {
        let matrix = fold_matrix();
        assert_eq!(matrix.len(), 16);
        let index = |name: &str| CHANNEL_MAP.iter().position(|channel| *channel == name).unwrap();
        assert_eq!(matrix[index("FL")], [1.0, 0.0]);
        assert_eq!(matrix[index("FR")], [0.0, 1.0]);
        assert_close(f64::from(matrix[index("FC")][0]), 0.70710678, 5e-7);
        assert_close(f64::from(matrix[index("LFE")][1]), 2.26464, 5e-6);
        assert_eq!(matrix[index("TSL")], [1.0, 0.0]);
        assert_eq!(matrix[index("TRR")], [0.0, 1.0]);
        // the wide channels go to their own sides
        assert_eq!(matrix[index("FLW")], [1.0, 0.0]);
        assert_eq!(matrix[index("FRW")], [0.0, 1.0]);
        let (left, right): (Vec<f32>, Vec<f32>) = matrix.iter().map(|row| (row[0], row[1])).unzip();
        assert_eq!(left.iter().sum::<f32>(), right.iter().sum::<f32>(), "the fold is symmetric");
        let tone = stereo_sine(5.0, 1000.0, -23.0);
        let mut signal = vec![0.0f32; 5 * RATE * 16];
        for (frame, sample) in signal.as_chunks_mut::<16>().0.iter_mut().zip(tone.iter().step_by(2)) {
            frame[index("FC")] = *sample as f32;
        }
        let mut folded = Vec::new();
        fold_frames(&signal, &matrix, &mut folded);
        let expected = -23.0 + 20.0 * 0.70710678f64.log10();
        assert_close(integrated_loudness(&folded, 2, SAMPLE_RATE).unwrap(), expected, 0.1);
    }

    #[test]
    fn measure_file_stereo_and_atmos_with_persistence() {
        let root = tempfile::tempdir().unwrap();
        let stereo_dir = root.path().join("1.stereo");
        fs::create_dir(&stereo_dir).unwrap();
        let tone = as_f32(&stereo_sine(5.0, 1000.0, -13.0)); // a loud master: 3 dB above the target
        let frames = (tone.len() / 2) as u64;
        let stereo_pcm = stereo_dir.join("stereo.f32le");
        write_f32le(&stereo_pcm, &tone);
        let meta = json!({"channels": 2, "frames": frames, "sample_rate": RATE});
        fs::write(stereo_dir.join("meta.json"), meta.to_string()).unwrap();
        let result = measure_file(&stereo_pcm, 2, None, None, None).unwrap();
        assert_close(result.lufs.unwrap(), -13.0, 0.1);
        assert_eq!(result.frames, frames);
        assert_eq!(result.fold, None);
        assert_eq!(stored_loudness(&stereo_pcm), None);
        store_loudness(&stereo_pcm, &result).unwrap();
        let stored = stored_loudness(&stereo_pcm).unwrap();
        assert_close(stored.lufs.unwrap(), -13.0, 0.1);
        assert_eq!(stored.frames, Some(frames));
        let meta = util::read_json(&stereo_dir.join("meta.json")).unwrap();
        assert_eq!((&meta["channels"], &meta["frames"]), (&json!(2), &json!(frames)), "validator keys untouched");
        assert_close(trim_db(result.lufs, TARGET_LUFS), -3.0, 0.1);
        // a 16-channel render: FL/FR carry the tone, measured through the fold, stored as loudness.json
        let atmos_dir = root.path().join("1.atmos");
        fs::create_dir(&atmos_dir).unwrap();
        let mut render = vec![0.0f32; frames as usize * 16];
        for (frame, pair) in render.as_chunks_mut::<16>().0.iter_mut().zip(tone.as_chunks::<2>().0) {
            frame[..2].copy_from_slice(pair);
        }
        let atmos_pcm = atmos_dir.join("atmos-916.f32le");
        write_f32le(&atmos_pcm, &render);
        let result = measure_file(&atmos_pcm, 16, None, None, None).unwrap();
        assert_close(result.lufs.unwrap(), -13.0, 0.1);
        assert_eq!(result.fold, Some("peqdb"));
        store_loudness(&atmos_pcm, &result).unwrap();
        assert!(atmos_dir.join("loudness.json").is_file());
        assert_close(stored_loudness(&atmos_pcm).unwrap().lufs.unwrap(), -13.0, 0.1);
    }

    #[test]
    fn trim_is_attenuation_only() {
        assert_eq!(trim_db(Some(-9.0), TARGET_LUFS), -7.0);
        assert_eq!(trim_db(Some(-16.0), TARGET_LUFS), 0.0);
        assert_eq!(trim_db(Some(-20.0), TARGET_LUFS), 0.0);
        assert_eq!(trim_db(None, TARGET_LUFS), 0.0);
        assert_eq!(trim_db(Some(f64::NAN), TARGET_LUFS), 0.0);
    }

    #[test]
    fn float_wav_writer() {
        let root = tempfile::tempdir().unwrap();
        let data = as_f32(&stereo_sine(0.1, 1000.0, -6.0));
        let path = write_wav_float32(&root.path().join("x.wav"), &data, 2, SAMPLE_RATE).unwrap();
        let raw = fs::read(&path).unwrap();
        let u16_at = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let find = |tag: &[u8]| raw.windows(4).position(|window| window == tag).unwrap();
        assert_eq!(&raw[..4], b"RIFF");
        assert_eq!(&raw[8..12], b"WAVE");
        assert_eq!(u32_at(4) as usize, raw.len() - 8);
        let fmt_at = find(b"fmt ");
        let format = (
            u16_at(fmt_at + 8),
            u16_at(fmt_at + 10),
            u32_at(fmt_at + 12),
            u32_at(fmt_at + 16),
            u16_at(fmt_at + 20),
            u16_at(fmt_at + 22),
        );
        assert_eq!(format, (3, 2, SAMPLE_RATE, SAMPLE_RATE * 8, 8, 32));
        let data_at = find(b"data");
        let payload = &raw[data_at + 8..];
        assert_eq!(payload.len(), u32_at(data_at + 4) as usize);
        let mut back = Vec::new();
        decode_f32le(payload, &mut back);
        assert_eq!(back, data);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // --- the production meter (ffmpeg ebur128 fed progressively) agrees with the FFT reference

    /// False (with a note) on a machine without ffmpeg: the stream meter tests need it.
    fn have_ffmpeg() -> bool {
        if !ffmpeg_available() {
            eprintln!("ffmpeg is not installed: stream meter test skipped");
        }
        ffmpeg_available()
    }

    /// `meter.feed(frames)`. When the call used up the time it gives ffmpeg (a busy machine),
    /// the value is read again once ffmpeg has caught up, so that the tests compare what the
    /// Python tests compare.
    fn feed(meter: &mut StreamMeter, frames: u64) -> Option<f64> {
        let begun = Instant::now();
        let running = meter.feed(frames).unwrap();
        if begun.elapsed() < SETTLE { running } else { meter.running(Duration::from_secs(30)) }
    }

    #[test]
    fn stereo_matches_the_numpy_meter() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut noise = pinkish_noise(6.0, 3);
        for sample in &mut noise[2 * RATE * 2..(2.5 * RATE as f64) as usize * 2] {
            *sample *= 1e-4; // a gated-out gap
        }
        let frames = (noise.len() / 2) as u64;
        let path = dir.path().join("stereo.f32le");
        write_f32le(&path, &as_f32(&noise));
        let mut meter = StreamMeter::new(&path, 2, None, SAMPLE_RATE).unwrap();
        let running = feed(&mut meter, frames);
        let result = meter.finish(FINISH_TIMEOUT).unwrap();
        let reference = loudness_of(&noise).unwrap();
        assert_close(result.lufs.unwrap(), reference, 0.05);
        assert_close(running.unwrap(), reference, 0.05);
        assert_eq!((result.frames, result.fold), (frames, None));
        let peak = noise.iter().fold(0.0f64, |peak, sample| peak.max(sample.abs()));
        assert_close(result.peak, peak, 5e-6);
    }

    /// Python: `test_16_channels_fold_like_measure_file` (a Rust name cannot start with a digit,
    /// hence the underscore).
    #[test]
    fn _16_channels_fold_like_measure_file() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wide = wide_from(&pinkish_noise(4.0, 4));
        let frames = (wide.len() / 16) as u64;
        let path = dir.path().join("atmos-916.f32le");
        write_f32le(&path, &wide);
        let reference = measure_file(&path, 16, None, None, None).unwrap();
        let mut meter = StreamMeter::new(&path, 16, None, SAMPLE_RATE).unwrap();
        meter.feed(frames).unwrap();
        let result = meter.finish(FINISH_TIMEOUT).unwrap();
        assert_close(result.lufs.unwrap(), reference.lufs.unwrap(), 0.05);
        assert_eq!(result.fold, Some("peqdb"));
        assert!(pan_expression(&fold_matrix()).contains("pan=stereo|c0=1.000000*c0+"));
    }

    #[test]
    fn progressive_feeding_reports_a_running_value() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let noise = pinkish_noise(8.0, 5);
        let frames = (noise.len() / 2) as u64;
        let half = frames / 2;
        let first = &noise[..half as usize * 2];
        let path = dir.path().join("grow.f32le");
        write_f32le(&path, &as_f32(first));
        let mut meter = StreamMeter::new(&path, 2, None, SAMPLE_RATE).unwrap();
        let early = meter.feed((0.2 * RATE as f64) as u64).unwrap();
        assert_eq!(early, None, "nothing before the first 400 ms block");
        let running = feed(&mut meter, half).expect("a running value");
        assert_close(running, loudness_of(first).unwrap(), 0.1);
        assert_eq!(meter.fed(), half);
        assert_close(feed(&mut meter, frames).unwrap(), running, 0.01); // the file has not grown yet
        assert_eq!(meter.fed(), half, "nothing more was read");
        append_f32le(&path, &as_f32(&noise[half as usize * 2..])); // the writer appends the rest
        meter.feed(frames).unwrap();
        let result = meter.finish(FINISH_TIMEOUT).unwrap();
        assert_eq!(result.frames, frames);
        assert_close(result.lufs.unwrap(), loudness_of(&noise).unwrap(), 0.05);
    }

    #[test]
    fn silence_and_close() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silence.f32le");
        write_f32le(&path, &vec![0.0f32; 2 * RATE * 2]);
        let mut meter = StreamMeter::new(&path, 2, None, SAMPLE_RATE).unwrap();
        assert_eq!(meter.feed(2 * RATE as u64).unwrap(), None);
        assert_eq!(meter.finish(FINISH_TIMEOUT).unwrap().lufs, None, "below the absolute gate: no loudness");
        let mut meter = StreamMeter::new(&path, 2, None, SAMPLE_RATE).unwrap();
        meter.feed(RATE as u64).unwrap();
        meter.close();
        assert!(meter.child.try_wait().unwrap().is_some(), "ffmpeg was killed");
        meter.close(); // idempotent
    }

    // --- beyond the Python tests

    /// The numbers `amui/loudness.py` reports for the same signals (rebuilt there sample for
    /// sample from `Uniform` and the pinking filter): the two meters are one measurement.
    /// `tests/reference/loudness_values.py` prints them.
    #[test]
    fn matches_the_python_implementation() {
        let noise = pinkish_noise(6.0, 7);
        let samples = (noise[0], noise[1], noise[2 * 287_999 + 1]);
        assert_eq!(samples, (0.033223263203355745, 0.025850218058036112, 0.0333436693178811));
        assert_close(loudness_of(&noise).unwrap(), -23.372406312543667, 1e-9);
        let mut gapped = noise.clone();
        for sample in &mut gapped[2 * RATE * 2..(2.5 * RATE as f64) as usize * 2] {
            *sample *= 1e-4;
        }
        assert_close(loudness_of(&gapped).unwrap(), -23.61712999103792, 1e-9);
        let left: Vec<f64> = noise.iter().step_by(2).copied().collect();
        assert_close(integrated_loudness(&left, 1, SAMPLE_RATE).unwrap(), -26.375278963511757, 1e-9);
        let short = pinkish_noise(3.0, 1);
        assert_close(loudness_of(&short).unwrap(), -23.389702081829338, 1e-9);
        assert_close(reference_loudness(&short), -23.38970208183417, 1e-9);

        let mut meter = Meter::new(SAMPLE_RATE);
        for chunk in noise.chunks(70_001 * 2) {
            meter.feed(chunk, 2);
        }
        assert_close(meter.integrated().unwrap(), -23.372406312543667, 1e-9);
        assert_eq!((meter.frames(), meter.sub_energies().len(), meter.rate()), (288_000, 60, SAMPLE_RATE));
        assert_close(meter.sub_energies()[0], 26.43854428582434, 1e-9);
        assert_close(meter.sub_energies()[59], 25.126402362638647, 1e-9);
        assert_eq!(meter.peak(), 0.23813016199515857);

        let dir = tempfile::tempdir().unwrap();
        let wide_path = dir.path().join("atmos-916.f32le");
        write_f32le(&wide_path, &wide_from(&noise));
        let wide = measure_file(&wide_path, 16, None, None, None).unwrap();
        // the fold is float32 arithmetic: numpy's matrix product may round a sample differently
        assert_close(wide.lufs.unwrap(), -16.618348632852197, 1e-6);
        assert_close(wide.peak, 0.6981214284896851, 1e-6);
        assert_eq!((wide.frames, wide.fold), (288_000, Some("peqdb")));
        assert_eq!(trim_db(wide.lufs, TARGET_LUFS), 0.0);
        let first = measure_file(&wide_path, 16, Some(100_000), None, None).unwrap();
        assert_close(first.lufs.unwrap(), -16.639925980460088, 1e-6);
        assert_eq!(first.frames, 100_000);
        let stereo_path = dir.path().join("stereo.f32le");
        write_f32le(&stereo_path, &as_f32(&noise));
        let stereo = measure_file(&stereo_path, 2, None, None, None).unwrap();
        assert_close(stereo.lufs.unwrap(), -23.372406312749167, 1e-9);
        assert_eq!((stereo.peak, stereo.frames, stereo.fold), (0.23813016712665558, 288_000, None));

        let response = k_weighting_ir(IR_TAPS);
        let taps = (response[0], response[1], response[8191]);
        assert_eq!(taps, (1.53512485958697, -0.11160147885084637, 2.4353662261709746e-19));
        assert_eq!(
            pan_expression(&fold_matrix()),
            "pan=stereo|c0=1.000000*c0+0.707107*c2+2.264640*c3+1.000000*c4+1.000000*c6+1.000000*c8+1.000000*c10\
             +1.000000*c12+1.000000*c14|c1=1.000000*c1+0.707107*c2+2.264640*c3+1.000000*c5+1.000000*c7\
             +1.000000*c9+1.000000*c11+1.000000*c13+1.000000*c15"
        );
        let trims = [(-13.004999, -3.0), (-12.345, -3.65), (-15.995, -0.01), (-16.004, 0.0), (-3.25, -12.75)];
        for (lufs, trim) in trims {
            assert_eq!(trim_db(Some(lufs), TARGET_LUFS), trim, "{lufs}");
        }
        assert_eq!(trim_db(Some(f64::INFINITY), TARGET_LUFS), 0.0);
    }

    #[test]
    fn fft_is_the_discrete_fourier_transform() {
        let size = 64;
        let fft = Fft::new(size);
        let mut noise = Uniform::new(11, 0);
        let signal: Vec<(f64, f64)> = (0..size).map(|_| (noise.next(), noise.next())).collect();
        let (mut re, mut im): (Vec<f64>, Vec<f64>) = signal.iter().copied().unzip();
        fft.forward(&mut re, &mut im);
        let bits = size.trailing_zeros();
        for bin in 0..size {
            let (mut sum_re, mut sum_im) = (0.0, 0.0);
            for (index, (x_re, x_im)) in signal.iter().enumerate() {
                let (sin, cos) = (-2.0 * PI * (bin * index) as f64 / size as f64).sin_cos();
                sum_re += x_re * cos - x_im * sin;
                sum_im += x_re * sin + x_im * cos;
            }
            let at = bin.reverse_bits() >> (usize::BITS - bits);
            assert_close(re[at], sum_re, 1e-12);
            assert_close(im[at], sum_im, 1e-12);
        }
        fft.inverse(&mut re, &mut im);
        for (index, (x_re, x_im)) in signal.iter().enumerate() {
            assert_close(re[index] / size as f64, *x_re, 1e-14);
            assert_close(im[index] / size as f64, *x_im, 1e-14);
        }
    }

    #[test]
    fn stored_values_are_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let pcm = dir.path().join("stereo.f32le");
        let meta = dir.path().join("meta.json");
        let put = |text: &str| fs::write(&meta, text).unwrap();
        put(r#"{"channels": 2, "frames": 10, "lufs": -12.5}"#);
        assert_eq!(stored_loudness(&pcm), None, "meta.json without a measured frame count");
        put(r#"{"channels": 2, "frames": 10, "lufs": -12.5, "loudness_frames": 0}"#);
        assert_eq!(stored_loudness(&pcm), None);
        put(r#"{"frames": 10, "lufs": -12.5, "loudness_peak": 0.9, "loudness_frames": 10}"#);
        let stored = stored_loudness(&pcm).unwrap();
        assert_eq!(stored, StoredLoudness { lufs: Some(-12.5), peak: Some(0.9), frames: Some(10) });
        put(r#"{"lufs": null, "loudness_frames": 10.0}"#);
        assert_eq!(stored_loudness(&pcm), Some(StoredLoudness { lufs: None, peak: None, frames: Some(10) }));
        put(r#"{"lufs": "loud", "loudness_frames": 10}"#);
        assert_eq!(stored_loudness(&pcm), None);
        put("[1, 2]");
        assert_eq!(stored_loudness(&pcm), None);
        // loudness.json comes first and needs no frame count
        fs::write(dir.path().join(LOUDNESS_FILE), r#"{"lufs": -17.25, "peak": 0.5, "frames": 480}"#).unwrap();
        let stored = stored_loudness(&pcm).unwrap();
        assert_eq!(stored, StoredLoudness { lufs: Some(-17.25), peak: Some(0.5), frames: Some(480) });

        // what store_loudness() writes is what Python writes: sorted keys, one space, 0600
        let render = dir.path().join("atmos-916.f32le");
        let result = Measurement { lufs: Some(-17.5), peak: 0.25, frames: 480, fold: Some("peqdb"), measured_at: 1.5 };
        store_loudness(&render, &result).unwrap();
        let written = dir.path().join(LOUDNESS_FILE);
        assert_eq!(
            fs::read_to_string(&written).unwrap(),
            "{\n \"fold\": \"peqdb\",\n \"frames\": 480,\n \"lufs\": -17.5,\n \"measured_at\": 1.5,\n \"peak\": 0.25\n}\n"
        );
        assert_eq!(fs::metadata(&written).unwrap().permissions().mode() & 0o777, 0o600);
        put(r#"{"channels": 2}"#);
        let silent = Measurement { lufs: None, peak: 0.0, frames: 96, fold: None, measured_at: 2.0 };
        store_loudness(&pcm, &silent).unwrap();
        let merged = util::read_json(&meta).unwrap();
        let expected = json!({
            "channels": 2, "lufs": null, "loudness_peak": 0.0, "loudness_frames": 96, "loudness_measured_at": 2.0
        });
        assert_eq!(merged, expected);
        fs::remove_file(&meta).unwrap();
        store_loudness(&pcm, &silent).unwrap(); // no meta.json: nothing to merge into
        assert!(!meta.exists());
    }

    #[test]
    fn failures_keep_the_python_texts() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.f32le");
        let error = measure_file(&missing, 2, None, None, None).unwrap_err();
        let expected = format!("[Errno 2] No such file or directory: '{}'", missing.display());
        assert_eq!(error, LoudnessError::Io(expected.clone()));
        assert_eq!(StreamMeter::new(&missing, 2, None, SAMPLE_RATE).err(), Some(LoudnessError::Io(expected)));
        let path = dir.path().join("tone.f32le");
        write_f32le(&path, &as_f32(&stereo_sine(1.0, 1000.0, -23.0)));
        let cancel = Event::new();
        cancel.set();
        let error = measure_file(&path, 2, None, None, Some(&cancel)).unwrap_err();
        assert_eq!(error.to_string(), "loudness measurement cancelled");
        assert_eq!(os_error_text(&io::Error::from_raw_os_error(32)), "[Errno 32] Broken pipe");
        assert_eq!(os_error_text(&io::Error::other("no reader")), "no reader");
        assert_eq!(poll_until(Duration::from_millis(20), || None::<u8>), None);
        if have_ffmpeg() {
            // a stream ffmpeg cannot read (no channels) fails with what ffmpeg said
            let mut meter = StreamMeter::new(&path, 0, None, SAMPLE_RATE).unwrap();
            let fed = meter.feed(RATE as u64);
            let message = match fed.and_then(|_| meter.finish(FINISH_TIMEOUT)) {
                Err(LoudnessError::Ffmpeg(message)) => message,
                other => panic!("expected an ffmpeg failure, got {other:?}"),
            };
            assert!(message.starts_with("ffmpeg ebur128 "), "{message}");
            assert!(!message.ends_with("no error output"), "{message}");
        }
    }
}
