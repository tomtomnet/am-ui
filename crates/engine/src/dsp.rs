//! The downmix of the 16-channel render to stereo and the headphone equalizer, computed in the
//! application.
//!
//! This is the filter chain the PipeWire sink "Atmos 9.1.6 Downmix" ran
//! (`workspace/pipewire/atmos-916-downmix.conf`), with the same arithmetic, so that a song sounds
//! as it did through the sink. Per side:
//!
//! ```text
//! eight channels x their gains -> mixer -> delay --+
//!                                                   +-> mixer -> preamp -> EQ gain -> EQ filters
//! LFE -> low-pass -> low-pass -> x LFE gain --------+
//! ```
//!
//! The left mixer takes FL, FC, SL, RL, FLW, TFL, TSL and TRL with the gains of
//! [`downmix::GROUPS`] in that order, the right one FR, FC and the right channels. The LFE passes
//! one pair of low-pass filters (a fourth-order Butterworth) and goes to both sides; the delay
//! lines hold the other channels back for as long as that filter delays the LFE. The equalizer's
//! filters run in the order of their slots: low shelves, peaking filters, high shelves.
//!
//! What "the same arithmetic" means, taken from PipeWire's filter-graph plugin (1.6):
//!
//! * Samples, coefficients and filter states are 32-bit floats. A control value is the 32-bit
//!   float of its eight-digit spelling ([`downmix::node_value`]), as the sink read it from
//!   `pw-cli`.
//! * A biquad is `y = b0 x + s1; s1 = b1 x - a1 y + s2; s2 = b2 x - a2 y` (transposed direct
//!   form II) with the Audio EQ Cookbook's coefficients, computed in doubles from the frequency
//!   relative to half the sample rate (itself a 32-bit float) and rounded to floats.
//! * A low-pass at or above half the sample rate lets everything pass; a slot without gain is
//!   the identity, exactly, and is left out here.
//! * A mixer leaves out inputs with gain 0; when the remaining gains are all equal it sums first
//!   and applies the gain once, otherwise it sums the products in the order of its inputs.
//! * The delay is a whole number of samples: the seconds times the rate, rounded down.
//!
//! One thing is better than in the sink: a change of the settings is cross-faded over
//! [`FADE_FRAMES`] frames (the old and the new chain run side by side for that long). The sink
//! switched a control at once and cleared the state of a filter whose values changed, which can
//! click.
//!
//! [`Chain::process`] allocates nothing and takes no lock: it can run on the thread that feeds
//! the output.

use crate::config::CHANNELS;
use crate::downmix::{self, Controls, EQ_SLOTS, Filter, FilterType, Params};

/// Frames over which a change of the settings is cross-faded (20 ms at 48 kHz).
pub const FADE_FRAMES: usize = 960;
/// The longest delay a delay line holds, in seconds (the sink's `max-delay`).
pub const MAX_DELAY_S: f32 = 0.1;

/// Index of each input channel (the order of [`crate::config::CHANNEL_MAP`]).
const FC: usize = 2;
const LFE: usize = 3;
/// The channels of the left and the right mixer in the order of their inputs; the centre goes
/// to both.
const LEFT: [usize; 8] = [0, FC, 4, 6, 8, 10, 12, 14];
const RIGHT: [usize; 8] = [1, FC, 5, 7, 9, 11, 13, 15];

/// One filter of the equalizer, as the sink's slot had it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: FilterType,
    /// Hz.
    pub freq: f32,
    pub q: f32,
    /// dB.
    pub gain: f32,
}

/// Everything the chain is made of: the values of the sink's controls.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The gain of each channel group into its side's mixer, in the order of
    /// [`downmix::GROUPS`].
    pub gains: [f32; 8],
    /// The LFE's gain into both sides.
    pub lfe: f32,
    /// The LFE's low-pass in Hz; at or above half the sample rate: none.
    pub lfe_lowpass: f32,
    /// The Q of the two low-pass filters.
    pub lfe_q: [f32; 2],
    /// Seconds the mixed channels are held back.
    pub lfe_delay: f32,
    /// The preamp as a factor.
    pub preamp: f32,
    /// The equalizer's gain stage as a factor.
    pub eq_gain: f32,
    /// The equalizer's filters in the order of their slots; slots without gain are left out.
    pub bands: Vec<Band>,
}

/// A control value as the sink read it: its eight-digit spelling as a 32-bit float.
fn as_read(controls: &Controls, name: &str) -> Option<f32> {
    downmix::node_value(controls.get(name)?).parse().ok()
}

impl Settings {
    /// The settings of a downmix profile with an equalizer: `params` from
    /// [`downmix::resolve`], the equalizer's preamp in dB and its filters (none: flat).
    pub fn new(params: &Params, eq_preamp_db: f64, filters: &[Filter]) -> Settings {
        Settings::from_controls(&downmix::controls(params), &downmix::eq_controls(eq_preamp_db, filters))
    }

    /// The settings that the control values of the sink stand for: `matrix` as
    /// [`downmix::controls`] gives them, `eq` as [`downmix::eq_controls`] does. Both sides of
    /// the sink were always set alike; the left side's values are taken. A control that is
    /// missing has the value of a sink that was never set (unity gain, no filter).
    pub fn from_controls(matrix: &Controls, eq: &Controls) -> Settings {
        let value = |controls: &Controls, name: &str, default: f32| as_read(controls, name).unwrap_or(default);
        let mut gains = [1.0; 8];
        for (index, gain) in gains.iter_mut().enumerate() {
            *gain = value(matrix, &format!("dry_l:Gain {}", index + 1), 1.0);
        }
        let mut bands = Vec::new();
        for (kind, _, slots) in EQ_SLOTS {
            for slot in 1..=slots {
                let name = format!("eq_l_{}{slot}", kind.as_str().to_lowercase());
                let gain = value(eq, &format!("{name}:Gain"), 0.0);
                if gain != 0.0 {
                    let freq = value(eq, &format!("{name}:Freq"), 1000.0);
                    bands.push(Band { kind, freq, q: value(eq, &format!("{name}:Q"), 0.707), gain });
                }
            }
        }
        Settings {
            gains,
            lfe: value(matrix, "out_l:Gain 2", 0.0),
            lfe_lowpass: value(matrix, "lfe_lp1:Freq", 1e6),
            lfe_q: [value(matrix, "lfe_lp1:Q", 0.707), value(matrix, "lfe_lp2:Q", 0.707)],
            lfe_delay: value(matrix, "dly_l:Delay (s)", 0.0),
            preamp: value(matrix, "preamp_l:Mult", 1.0),
            eq_gain: value(eq, "eqgain_l:Mult", 1.0),
            bands,
        }
    }
}

// --- the filters -------------------------------------------------------------------------

/// The coefficients of a biquad, normalized by a0: `[b0, b1, b2, a1, a2]`.
type Coefficients = [f32; 5];

const IDENTITY: Coefficients = [1.0, 0.0, 0.0, 0.0, 0.0];

fn normalized(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coefficients {
    let a0_inv = 1.0 / a0;
    [(b0 * a0_inv) as f32, (b1 * a0_inv) as f32, (b2 * a0_inv) as f32, (a1 * a0_inv) as f32, (a2 * a0_inv) as f32]
}

/// The frequency relative to half the sample rate, as the sink's filters computed it: in
/// 32-bit floats.
fn relative(freq: f32, rate: u32) -> f64 {
    f64::from(freq * 2.0 / rate as f32)
}

/// A low-pass (`H(s) = 1 / (s^2 + s/Q + 1)`). `cutoff` is relative to half the sample rate.
fn lowpass(cutoff: f64, q: f64) -> Coefficients {
    let cutoff = cutoff.clamp(0.0, 1.0);
    if cutoff == 1.0 || cutoff == 0.0 {
        // everything passes at 1, nothing at 0
        return normalized(cutoff, 0.0, 0.0, 1.0, 0.0, 0.0);
    }
    let q = if q <= 0.0 { std::f64::consts::FRAC_1_SQRT_2 } else { q };
    let w0 = std::f64::consts::PI * cutoff;
    let alpha = w0.sin() / (2.0 * q);
    let k = w0.cos();
    normalized((1.0 - k) / 2.0, 1.0 - k, (1.0 - k) / 2.0, 1.0 + alpha, -2.0 * k, 1.0 - alpha)
}

fn lowshelf(frequency: f64, q: f64, db_gain: f64) -> Coefficients {
    let frequency = frequency.clamp(0.0, 1.0);
    let a = 10f64.powf(db_gain / 40.0);
    if frequency == 1.0 {
        return normalized(a * a, 0.0, 0.0, 1.0, 0.0, 0.0);
    }
    if frequency <= 0.0 {
        return IDENTITY;
    }
    let q = if q <= 0.0 { std::f64::consts::FRAC_1_SQRT_2 } else { q };
    let w0 = std::f64::consts::PI * frequency;
    let alpha = w0.sin() / (2.0 * q);
    let k = w0.cos();
    let k2 = 2.0 * a.sqrt() * alpha;
    let (plus, minus) = (a + 1.0, a - 1.0);
    normalized(
        a * (plus - minus * k + k2),
        2.0 * a * (minus - plus * k),
        a * (plus - minus * k - k2),
        plus + minus * k + k2,
        -2.0 * (minus + plus * k),
        plus + minus * k - k2,
    )
}

fn highshelf(frequency: f64, q: f64, db_gain: f64) -> Coefficients {
    let frequency = frequency.clamp(0.0, 1.0);
    let a = 10f64.powf(db_gain / 40.0);
    if frequency == 1.0 {
        return IDENTITY;
    }
    if frequency <= 0.0 {
        return normalized(a * a, 0.0, 0.0, 1.0, 0.0, 0.0);
    }
    let q = if q <= 0.0 { std::f64::consts::FRAC_1_SQRT_2 } else { q };
    let w0 = std::f64::consts::PI * frequency;
    let alpha = w0.sin() / (2.0 * q);
    let k = w0.cos();
    let k2 = 2.0 * a.sqrt() * alpha;
    let (plus, minus) = (a + 1.0, a - 1.0);
    normalized(
        a * (plus + minus * k + k2),
        -2.0 * a * (minus + plus * k),
        a * (plus + minus * k - k2),
        plus - minus * k + k2,
        2.0 * (minus - plus * k),
        plus - minus * k - k2,
    )
}

fn peaking(frequency: f64, q: f64, db_gain: f64) -> Coefficients {
    let frequency = frequency.clamp(0.0, 1.0);
    let q = q.max(0.0);
    let a = 10f64.powf(db_gain / 40.0);
    if frequency <= 0.0 || frequency >= 1.0 {
        return IDENTITY;
    }
    if q <= 0.0 {
        return normalized(a * a, 0.0, 0.0, 1.0, 0.0, 0.0);
    }
    let w0 = std::f64::consts::PI * frequency;
    let alpha = w0.sin() / (2.0 * q);
    let k = w0.cos();
    normalized(1.0 + alpha * a, -2.0 * k, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * k, 1.0 - alpha / a)
}

/// The coefficients of an equalizer filter at a sample rate: `[b0, b1, b2, a1, a2]` with a0 = 1.
pub fn band_coefficients(band: &Band, rate: u32) -> [f32; 5] {
    let (frequency, q, gain) = (relative(band.freq, rate), f64::from(band.q), f64::from(band.gain));
    match band.kind {
        FilterType::Lsc => lowshelf(frequency, q, gain),
        FilterType::Pk => peaking(frequency, q, gain),
        FilterType::Hsc => highshelf(frequency, q, gain),
    }
}

/// The coefficients of the LFE's low-pass with this Q.
pub fn lowpass_coefficients(freq: f32, q: f32, rate: u32) -> [f32; 5] {
    lowpass(relative(freq, rate), f64::from(q))
}

/// One biquad with its state.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    c: Coefficients,
    s1: f32,
    s2: f32,
}

impl Biquad {
    fn new(c: Coefficients) -> Biquad {
        Biquad { c, s1: 0.0, s2: 0.0 }
    }

    #[inline(always)]
    fn run(&mut self, x: f32) -> f32 {
        let [b0, b1, b2, a1, a2] = self.c;
        let y = b0 * x + self.s1;
        self.s1 = b1 * x - a1 * y + self.s2;
        self.s2 = b2 * x - a2 * y;
        y
    }

    /// What the sink did after every block: a state that is not an ordinary number (too small
    /// to be exact, infinite, not a number) starts again from nothing.
    fn settle(&mut self) {
        if !self.s1.is_normal() {
            self.s1 = 0.0;
        }
        if !self.s2.is_normal() {
            self.s2 = 0.0;
        }
    }
}

/// A delay by a whole number of frames.
#[derive(Debug, Clone)]
struct Delay {
    buffer: Vec<f32>,
    at: usize,
    frames: usize,
}

impl Delay {
    fn new(seconds: f32, rate: u32) -> Delay {
        // the sink's delay line: the longest delay in frames, rounded up to a multiple of 64
        let capacity = ((MAX_DELAY_S * rate as f32) as usize).max(1).next_multiple_of(64);
        let frames = ((seconds.max(0.0) * rate as f32) as usize).min(capacity - 1);
        Delay { buffer: vec![0.0; capacity], at: 0, frames }
    }

    #[inline(always)]
    fn run(&mut self, x: f32) -> f32 {
        if self.frames == 0 {
            return x;
        }
        let capacity = self.buffer.len();
        self.buffer[self.at] = x;
        let from = if self.at >= self.frames { self.at - self.frames } else { self.at + capacity - self.frames };
        self.at = if self.at + 1 >= capacity { 0 } else { self.at + 1 };
        self.buffer[from]
    }
}

/// The sink's mixer: what goes in with which gain, and whether the gains are all the same.
#[derive(Debug, Clone)]
struct Mixer<const N: usize> {
    /// (input, gain) of the inputs whose gain is not 0, in order.
    inputs: [(usize, f32); N],
    used: usize,
    /// The one gain when all used inputs have the same.
    common: Option<f32>,
}

impl<const N: usize> Mixer<N> {
    fn new(inputs: [usize; N], gains: [f32; N]) -> Mixer<N> {
        let mut kept = [(0, 0.0); N];
        let mut used = 0;
        for (input, gain) in inputs.into_iter().zip(gains) {
            if gain != 0.0 {
                kept[used] = (input, gain);
                used += 1;
            }
        }
        let common = kept[..used].iter().all(|(_, gain)| *gain == kept[0].1).then_some(kept[0].1);
        Mixer { inputs: kept, used, common: common.filter(|_| used > 0) }
    }

    #[inline(always)]
    fn run(&self, sample: impl Fn(usize) -> f32) -> f32 {
        let inputs = &self.inputs[..self.used];
        let Some(((first, first_gain), rest)) = inputs.split_first() else { return 0.0 };
        match self.common {
            // the sum, then the gain once
            Some(gain) => {
                let sum = rest.iter().fold(sample(*first), |sum, (input, _)| sum + sample(*input));
                if gain == 1.0 { sum } else { sum * gain }
            }
            None => rest.iter().fold(sample(*first) * first_gain, |sum, (input, gain)| sum + sample(*input) * gain),
        }
    }
}

/// One side after its mixer: the delay, the preamp, the equalizer.
#[derive(Debug, Clone)]
struct Side {
    delay: Delay,
    bands: Vec<Biquad>,
}

/// The chain for one set of settings, with its state.
#[derive(Debug, Clone)]
struct Stage {
    left_mix: Mixer<8>,
    right_mix: Mixer<8>,
    /// The mixer after the delay: the delayed channels with gain 1, the LFE with its gain.
    out_mix: Mixer<2>,
    lowpass: [Biquad; 2],
    preamp: f32,
    eq_gain: f32,
    left: Side,
    right: Side,
}

impl Stage {
    fn new(settings: &Settings, rate: u32) -> Stage {
        let bands: Vec<Biquad> = settings.bands.iter().map(|band| Biquad::new(band_coefficients(band, rate))).collect();
        let side = || Side { delay: Delay::new(settings.lfe_delay, rate), bands: bands.clone() };
        Stage {
            left_mix: Mixer::new(LEFT, settings.gains),
            right_mix: Mixer::new(RIGHT, settings.gains),
            out_mix: Mixer::new([0, 1], [1.0, settings.lfe]),
            lowpass: [
                Biquad::new(lowpass_coefficients(settings.lfe_lowpass, settings.lfe_q[0], rate)),
                Biquad::new(lowpass_coefficients(settings.lfe_lowpass, settings.lfe_q[1], rate)),
            ],
            preamp: settings.preamp,
            eq_gain: settings.eq_gain,
            left: side(),
            right: side(),
        }
    }

    /// One frame to a stereo frame.
    #[inline(always)]
    fn frame(&mut self, input: &[f32; CHANNELS]) -> (f32, f32) {
        let first = self.lowpass[0].run(input[LFE]);
        let lfe = self.lowpass[1].run(first);
        let left = self.left_mix.run(|channel| input[channel]);
        let right = self.right_mix.run(|channel| input[channel]);
        (self.finish(left, lfe, true), self.finish(right, lfe, false))
    }

    #[inline(always)]
    fn finish(&mut self, mixed: f32, lfe: f32, left: bool) -> f32 {
        let (out_mix, preamp, eq_gain) = (&self.out_mix, self.preamp, self.eq_gain);
        let side = if left { &mut self.left } else { &mut self.right };
        let delayed = side.delay.run(mixed);
        let summed = out_mix.run(|input| if input == 0 { delayed } else { lfe });
        let gained = |value: f32, gain: f32| if gain == 1.0 { value } else { value * gain };
        let mut sample = gained(gained(summed, preamp), eq_gain);
        for band in &mut side.bands {
            sample = band.run(sample);
        }
        sample
    }

    fn settle(&mut self) {
        let sides = self.left.bands.iter_mut().chain(&mut self.right.bands);
        self.lowpass.iter_mut().chain(sides).for_each(Biquad::settle);
    }
}

/// The downmix and the equalizer for one output stream.
#[derive(Debug, Clone)]
pub struct Chain {
    rate: u32,
    settings: Settings,
    stage: Stage,
    /// The stage that is being faded in, and how many frames of the fade are done.
    incoming: Option<(Stage, usize)>,
    /// Settings that arrived during a fade: they follow it.
    waiting: Option<Settings>,
}

impl Chain {
    pub fn new(settings: Settings, rate: u32) -> Chain {
        let stage = Stage::new(&settings, rate);
        Chain { rate, settings, stage, incoming: None, waiting: None }
    }

    /// The settings in effect, or on their way in.
    pub fn settings(&self) -> &Settings {
        self.waiting.as_ref().unwrap_or(&self.settings)
    }

    /// Change the settings: the new chain is faded in over [`FADE_FRAMES`] frames while the old
    /// one fades out. A change that arrives during a fade follows it; of several, the last.
    pub fn set(&mut self, settings: Settings) {
        if &settings == self.settings() {
            return;
        }
        if self.incoming.is_some() {
            self.waiting = Some(settings);
        } else {
            self.begin(settings);
        }
    }

    fn begin(&mut self, settings: Settings) {
        let mut stage = Stage::new(&settings, self.rate);
        // what the delay lines hold goes along, so that the new chain does not start in silence
        if stage.left.delay.buffer.len() == self.stage.left.delay.buffer.len() {
            for (new, old) in [(&mut stage.left, &self.stage.left), (&mut stage.right, &self.stage.right)] {
                new.delay.buffer.copy_from_slice(&old.delay.buffer);
                new.delay.at = old.delay.at;
            }
        }
        self.settings = settings;
        self.incoming = Some((stage, 0));
    }

    /// Whether a change of settings is still being faded in.
    pub fn fading(&self) -> bool {
        self.incoming.is_some()
    }

    /// Forget what the filters and the delay lines hold, and finish a fade at once (a new stream
    /// starts).
    pub fn reset(&mut self) {
        let settings = self.waiting.take().unwrap_or_else(|| self.settings.clone());
        self.incoming = None;
        self.stage = Stage::new(&settings, self.rate);
        self.settings = settings;
    }

    /// Downmix `input` (frames of [`CHANNELS`] interleaved samples) into `output` (interleaved
    /// stereo frames, as many).
    ///
    /// # Panics
    /// When `output` does not hold two samples for each frame of `input`.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let (input, odd) = input.as_chunks::<CHANNELS>();
        let (output, spare) = output.as_chunks_mut::<2>();
        assert!(odd.is_empty() && spare.is_empty() && input.len() == output.len(), "frames in, stereo frames out");
        let mut done = 0;
        while done < input.len() {
            let Some((incoming, faded)) = &mut self.incoming else {
                // the usual case: one chain
                for (frame, out) in input[done..].iter().zip(&mut output[done..]) {
                    (out[0], out[1]) = self.stage.frame(frame);
                }
                break;
            };
            // both chains, the old one fading out
            let count = (input.len() - done).min(FADE_FRAMES - *faded);
            for (frame, out) in input[done..done + count].iter().zip(&mut output[done..done + count]) {
                let (old_left, old_right) = self.stage.frame(frame);
                let (new_left, new_right) = incoming.frame(frame);
                *faded += 1;
                // the last frame of the fade is the new chain's alone
                let weight = *faded as f32 / FADE_FRAMES as f32;
                out[0] = old_left * (1.0 - weight) + new_left * weight;
                out[1] = old_right * (1.0 - weight) + new_right * weight;
            }
            done += count;
            if *faded >= FADE_FRAMES {
                let (stage, _) = self.incoming.take().expect("a fade was running");
                self.stage = stage;
                if let Some(next) = self.waiting.take() {
                    self.begin(next);
                }
            }
        }
        self.stage.settle();
        if let Some((incoming, _)) = &mut self.incoming {
            incoming.settle();
        }
    }
}

#[cfg(test)]
// The numbers in here are the sink's control values in their eight-digit spelling (0.70710678
// is one of them, not the square root): they are written as the sink had them.
#[allow(clippy::approx_constant, clippy::excessive_precision)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn peqdb() -> Settings {
        Settings::new(&downmix::resolve("peqdb", Some(-9.0)).unwrap(), 0.0, &[])
    }

    fn filter(kind: FilterType, freq: f64, gain: f64, q: f64) -> Filter {
        Filter { kind, freq, gain, q }
    }

    /// Frames of one channel carrying `signal`, the others silent.
    fn on_channel(channel: usize, signal: &[f32]) -> Vec<f32> {
        let mut frames = vec![0.0; signal.len() * CHANNELS];
        for (frame, sample) in frames.as_chunks_mut::<CHANNELS>().0.iter_mut().zip(signal) {
            frame[channel] = *sample;
        }
        frames
    }

    fn run(chain: &mut Chain, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0; input.len() / CHANNELS * 2];
        chain.process(input, &mut output);
        output
    }

    fn sine(freq: f32, frames: usize) -> Vec<f32> {
        (0..frames).map(|n| (2.0 * std::f32::consts::PI * freq * n as f32 / RATE as f32).sin() * 0.5).collect()
    }

    fn peak(samples: impl Iterator<Item = f32>) -> f32 {
        samples.fold(0.0, |peak, sample| peak.max(sample.abs()))
    }

    #[test]
    fn settings_are_the_sink_s_control_values() {
        let settings = peqdb();
        assert_eq!(settings.gains, [1.0, 0.70710678, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!((settings.lfe, settings.lfe_lowpass), (2.26464, 125.0));
        assert_eq!(settings.lfe_q, [0.5411961, 1.306563]);
        assert_eq!(settings.lfe_delay, 0.0033375519);
        assert_eq!(settings.preamp, 0.35481339);
        assert_eq!((settings.eq_gain, settings.bands.len()), (1.0, 0));

        let apple = Settings::new(&downmix::resolve("apple", Some(0.0)).unwrap(), 0.0, &[]);
        assert_eq!((apple.lfe, apple.lfe_lowpass, apple.lfe_delay, apple.preamp), (0.0, 1e6, 0.0, 1.0));

        // an equalizer: its preamp on the gain stage, its filters in the order of the slots
        let filters = [
            filter(FilterType::Pk, 105.0, -2.6, 0.7),
            filter(FilterType::Hsc, 9000.0, -1.5, 0.7),
            filter(FilterType::Lsc, 100.0, 2.0, 0.7),
            filter(FilterType::Pk, 3200.0, 1.25, 2.0),
        ];
        let with_eq = Settings::new(&downmix::resolve("peqdb", Some(-9.0)).unwrap(), -6.2, &filters);
        assert_eq!(with_eq.eq_gain, 0.48977882);
        let kinds: Vec<(FilterType, f32)> = with_eq.bands.iter().map(|band| (band.kind, band.freq)).collect();
        let expected =
            [(FilterType::Lsc, 100.0), (FilterType::Pk, 105.0), (FilterType::Pk, 3200.0), (FilterType::Hsc, 9000.0)];
        assert_eq!(kinds, expected);
        assert_eq!(with_eq.bands[1], Band { kind: FilterType::Pk, freq: 105.0, q: 0.7, gain: -2.6 });

        // a sink that was never set
        let unset = Settings::from_controls(&Controls::new(), &Controls::new());
        assert_eq!((unset.gains, unset.lfe, unset.preamp, unset.eq_gain), ([1.0; 8], 0.0, 1.0, 1.0));
        assert!(unset.bands.is_empty());
    }

    #[test]
    fn a_flat_slot_is_the_identity_and_a_high_cutoff_passes_everything() {
        let flat = |kind| band_coefficients(&Band { kind, freq: 1000.0, q: 0.707, gain: 0.0 }, RATE);
        for kind in FilterType::ALL {
            let [b0, b1, b2, a1, a2] = flat(kind);
            assert_eq!((b0, b1 == a1, b2 == a2), (1.0, true, true), "{kind:?}");
        }
        assert_eq!(lowpass_coefficients(1e6, 0.5411961, RATE), IDENTITY);
        assert_eq!(lowpass_coefficients(24_000.0, 0.5411961, RATE), IDENTITY);
        assert_ne!(lowpass_coefficients(125.0, 0.5411961, RATE), IDENTITY);
    }

    #[test]
    fn coefficients_are_the_cookbook_s() {
        // a peaking filter at a quarter of the sample rate: w0 = pi/2, alpha = 1 / (2 Q)
        let band = Band { kind: FilterType::Pk, freq: 12_000.0, q: 1.0, gain: 6.0 };
        let a = 10f64.powf(6.0 / 40.0);
        let [b0, b1, b2, a1, a2] = band_coefficients(&band, RATE);
        let a0 = 1.0 + 0.5 / a;
        assert!((f64::from(b0) - (1.0 + 0.5 * a) / a0).abs() < 1e-7);
        assert!(b1.abs() < 1e-7 && a1.abs() < 1e-7);
        assert!((f64::from(b2) - (1.0 - 0.5 * a) / a0).abs() < 1e-7);
        assert!((f64::from(a2) - (1.0 - 0.5 / a) / a0).abs() < 1e-7);
        // the low-pass at 125 Hz has unity gain at 0 Hz
        let [b0, b1, b2, a1, a2] = lowpass_coefficients(125.0, 0.5411961, RATE).map(f64::from);
        assert!(((b0 + b1 + b2) / (1.0 + a1 + a2) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn each_channel_goes_to_its_side_with_its_gain() {
        let settings = Settings::new(&downmix::resolve("itu", Some(0.0)).unwrap(), 0.0, &[]);
        let expected: [(f32, f32); 16] = [
            (1.0, 0.0),
            (0.0, 1.0),
            (0.70710678, 0.70710678),
            (0.35355339, 0.35355339),
            (0.70710678, 0.0),
            (0.0, 0.70710678),
            (0.70710678, 0.0),
            (0.0, 0.70710678),
            (0.866, 0.0),
            (0.0, 0.866),
            (0.5, 0.0),
            (0.0, 0.5),
            (0.5, 0.0),
            (0.0, 0.5),
            (0.5, 0.0),
            (0.0, 0.5),
        ];
        for (channel, (left, right)) in expected.into_iter().enumerate() {
            let mut chain = Chain::new(settings.clone(), RATE);
            let output = run(&mut chain, &on_channel(channel, &[0.25; 8]));
            assert_eq!((output[14], output[15]), (0.25 * left, 0.25 * right), "channel {channel}");
        }
    }

    #[test]
    fn the_lfe_is_low_passed_and_the_other_channels_wait_for_it() {
        let mut chain = Chain::new(peqdb(), RATE);
        // an impulse on the front left channel arrives 160 frames later, at the preamp's level
        let mut impulse = vec![0.0f32; 400];
        impulse[0] = 1.0;
        let output = run(&mut chain, &on_channel(0, &impulse));
        let left: Vec<f32> = output.iter().step_by(2).copied().collect();
        assert_eq!(left.iter().position(|sample| *sample != 0.0), Some(160));
        assert_eq!(left[160], 0.35481339);
        assert!(left[161..].iter().all(|sample| *sample == 0.0));
        assert!(output.iter().skip(1).step_by(2).all(|sample| *sample == 0.0), "nothing on the right");

        // a 40 Hz tone on the LFE passes with its gain and the preamp, an 8 kHz tone does not
        let level = |freq: f32| {
            let mut chain = Chain::new(peqdb(), RATE);
            let output = run(&mut chain, &on_channel(LFE, &sine(freq, 48_000)));
            peak(output[2 * 24_000..].iter().copied())
        };
        let low = level(40.0);
        assert!((low - 0.5 * 2.26464 * 0.35481339).abs() < 0.01, "{low}");
        assert!(level(8000.0) < 1e-4);
        // and it goes to both sides alike
        let mut chain = Chain::new(peqdb(), RATE);
        let output = run(&mut chain, &on_channel(LFE, &sine(60.0, 4800)));
        assert!(output.as_chunks::<2>().0.iter().all(|frame| frame[0] == frame[1]));
    }

    /// The gain in dB a biquad with these coefficients has at a frequency.
    fn response_db(c: [f32; 5], freq: f32) -> f32 {
        let [b0, b1, b2, a1, a2] = c.map(f64::from);
        let w = 2.0 * std::f64::consts::PI * f64::from(freq) / f64::from(RATE);
        let (re1, im1, re2, im2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
        let numerator = (b0 + b1 * re1 + b2 * re2).hypot(b1 * im1 + b2 * im2);
        let denominator = (1.0 + a1 * re1 + a2 * re2).hypot(a1 * im1 + a2 * im2);
        (20.0 * (numerator / denominator).log10()) as f32
    }

    #[test]
    fn the_equalizer_shapes_the_sound() {
        let params = downmix::resolve("peqdb", Some(0.0)).unwrap();
        // the level of a tone on the left once the filters have settled: the root mean square
        // over half a second, a whole number of periods for the whole frequencies used here
        let level = |filters: &[Filter], preamp: f64, freq: f32| {
            let mut chain = Chain::new(Settings::new(&params, preamp, filters), RATE);
            let output = run(&mut chain, &on_channel(0, &sine(freq, 48_000)));
            let squares: f64 = output[2 * 24_000..].iter().step_by(2).map(|sample| f64::from(*sample).powi(2)).sum();
            (squares / 24_000.0).sqrt() as f32
        };
        let db = |ratio: f32| 20.0 * ratio.log10();
        let boost = [filter(FilterType::Pk, 1000.0, 6.0, 1.0)];
        assert!((db(level(&boost, 0.0, 1000.0) / level(&[], 0.0, 1000.0)) - 6.0).abs() < 0.05);
        assert!(db(level(&boost, 0.0, 60.0) / level(&[], 0.0, 60.0)).abs() < 0.1);
        // the equalizer's preamp is a plain gain
        assert!((db(level(&[], -6.0, 1000.0) / level(&[], 0.0, 1000.0)) + 6.0).abs() < 0.01);
        // shelves: far below a low shelf and far above a high one, their gain
        let shelves = [filter(FilterType::Lsc, 200.0, -4.0, 0.7), filter(FilterType::Hsc, 6000.0, 3.0, 0.7)];
        assert!((db(level(&shelves, 0.0, 20.0) / level(&[], 0.0, 20.0)) + 4.0).abs() < 0.1);
        assert!((db(level(&shelves, 0.0, 23_000.0) / level(&[], 0.0, 23_000.0)) - 3.0).abs() < 0.1);

        // what comes out is what the filters' coefficients say, for each filter and all together
        let set = [
            filter(FilterType::Lsc, 105.0, 5.5, 0.7),
            filter(FilterType::Pk, 200.0, -3.1, 0.5),
            filter(FilterType::Pk, 2200.0, 4.0, 2.5),
            filter(FilterType::Pk, 6500.0, -6.0, 4.0),
            filter(FilterType::Hsc, 10_000.0, -2.5, 0.7),
        ];
        let bands = Settings::new(&params, 0.0, &set).bands;
        assert_eq!(bands.len(), set.len());
        for freq in [30.0, 105.0, 190.0, 1000.0, 2200.0, 6400.0, 12_000.0, 19_000.0] {
            let expected: f32 = bands.iter().map(|band| response_db(band_coefficients(band, RATE), freq)).sum();
            let measured = db(level(&set, 0.0, freq) / level(&[], 0.0, freq));
            assert!((measured - expected).abs() < 0.02, "{freq} Hz: {measured} dB, expected {expected} dB");
        }
    }

    #[test]
    fn blocks_of_any_length_give_the_same_stream() {
        let filters = [filter(FilterType::Pk, 800.0, -3.0, 1.4), filter(FilterType::Lsc, 90.0, 4.0, 0.7)];
        let settings = Settings::new(&downmix::resolve("peqdb", Some(-9.0)).unwrap(), -4.0, &filters);
        // every channel with its own tone
        let frames = 4800;
        let mut input = vec![0.0f32; frames * CHANNELS];
        for channel in 0..CHANNELS {
            for (n, sample) in sine(50.0 + 97.0 * channel as f32, frames).into_iter().enumerate() {
                input[n * CHANNELS + channel] = sample * 0.2;
            }
        }
        let mut whole = Chain::new(settings.clone(), RATE);
        let expected = run(&mut whole, &input);
        let mut pieces = Chain::new(settings, RATE);
        let mut output = Vec::new();
        let mut at = 0;
        for length in [1, 479, 480, 7, 1024, 333, 2000, 476] {
            output.extend(run(&mut pieces, &input[at * CHANNELS..(at + length) * CHANNELS]));
            at += length;
        }
        assert_eq!(at, frames);
        assert_eq!(output, expected);
    }

    #[test]
    fn a_change_of_settings_is_faded_in() {
        let quiet = Settings::new(&downmix::resolve("peqdb", Some(-20.0)).unwrap(), 0.0, &[]);
        let mut chain = Chain::new(peqdb(), RATE);
        let tone = on_channel(0, &vec![0.5; FADE_FRAMES + 2400]);
        let before = run(&mut chain, &tone[..1200 * CHANNELS])[2 * 1199];
        chain.set(quiet.clone());
        assert!(chain.fading());
        assert_eq!(chain.settings(), &quiet);
        let during = run(&mut chain, &tone[..FADE_FRAMES * CHANNELS]);
        assert!(!chain.fading());
        let after = run(&mut chain, &tone[..600 * CHANNELS])[2 * 599];
        assert_eq!((before, after), (0.5 * 0.35481339, 0.5 * 0.1));
        // from one level to the other without a step
        let left: Vec<f32> = during.iter().step_by(2).copied().collect();
        assert!(
            left.windows(2)
                .all(|pair| pair[1] <= pair[0] && pair[0] - pair[1] < 2.0 * (before - after) / FADE_FRAMES as f32)
        );
        assert_eq!(left[FADE_FRAMES - 1], after);

        // the same settings again start no fade
        chain.set(quiet);
        assert!(!chain.fading());
    }

    #[test]
    fn changes_during_a_fade_follow_it_and_the_last_one_wins() {
        let at = |preamp: f64| Settings::new(&downmix::resolve("peqdb", Some(preamp)).unwrap(), 0.0, &[]);
        let mut chain = Chain::new(at(0.0), RATE);
        let tone = on_channel(0, &vec![0.5; 4 * FADE_FRAMES]);
        chain.set(at(-6.0));
        run(&mut chain, &tone[..100 * CHANNELS]);
        chain.set(at(-12.0));
        chain.set(at(-20.0));
        assert_eq!(chain.settings(), &at(-20.0));
        // the first fade ends, the last settings fade in, the ones in between are never heard
        let output = run(&mut chain, &tone[..(3 * FADE_FRAMES) * CHANNELS]);
        assert!(!chain.fading());
        assert_eq!(output[2 * (3 * FADE_FRAMES - 1)], 0.5 * 0.1);
        let lowest = output.iter().step_by(2).skip(200).fold(f32::MAX, |low, sample| low.min(*sample));
        assert!((lowest - 0.5 * 0.1).abs() < 1e-7, "{lowest}");

        // a reset takes the latest settings at once and forgets what was in the delay lines
        chain.set(at(0.0));
        chain.set(at(-6.0));
        chain.reset();
        assert!(!chain.fading());
        let output = run(&mut chain, &tone[..400 * CHANNELS]);
        assert!(output[..2 * 160].iter().all(|sample| *sample == 0.0));
        assert_eq!(output[2 * 399], 0.5 * 0.50118723);
    }

    /// The SHA-256 of what the chain makes of the reference signal, in blocks of the sink's
    /// quantum: sample n of channel c is ((7 n + 13 c) mod 251) / 251 - 0.5, with a silent
    /// stretch in which the filters' states decay.
    fn fingerprint(settings: Settings) -> String {
        use sha2::{Digest, Sha256};
        const FRAMES: usize = 48_000;
        let mut input = vec![0.0f32; FRAMES * CHANNELS];
        for (n, frame) in input.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            if !(20_000..30_000).contains(&n) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = (((n * 7 + channel * 13) % 251) as f64 / 251.0 - 0.5) as f32;
                }
            }
        }
        let mut chain = Chain::new(settings, RATE);
        let mut hash = Sha256::new();
        for block in input.chunks(1024 * CHANNELS) {
            let output = run(&mut chain, block);
            hash.update(output.iter().flat_map(|sample| sample.to_le_bytes()).collect::<Vec<u8>>());
        }
        format!("{:x}", hash.finalize())
    }

    /// The chain gives, bit for bit, what PipeWire's own filter code gives in the sink's graph:
    /// `tests/reference/pipewire_chain.sh` builds a program around that code (version 1.6.11)
    /// and prints these numbers.
    #[test]
    fn matches_pipewire_s_own_code() {
        let peqdb = downmix::resolve("peqdb", Some(-9.0)).unwrap();
        assert_eq!(
            fingerprint(Settings::new(&peqdb, 0.0, &[])),
            "584d7b219624b2b894ade6743c10951b3e1a2527cd6d3bc253de5dbd2a6242e0"
        );
        // fifteen filters, as a headphone preset has them
        let filters = [
            filter(FilterType::Lsc, 105.0, 5.5, 0.7),
            filter(FilterType::Lsc, 60.0, -1.25, 0.9),
            filter(FilterType::Pk, 20.0, -3.9, 0.5),
            filter(FilterType::Pk, 105.0, 2.1, 0.8),
            filter(FilterType::Pk, 210.0, -1.2, 1.4),
            filter(FilterType::Pk, 480.0, 0.9, 1.1),
            filter(FilterType::Pk, 900.0, -2.4, 2.0),
            filter(FilterType::Pk, 1500.0, 1.6, 1.8),
            filter(FilterType::Pk, 2200.0, 4.0, 2.5),
            filter(FilterType::Pk, 3100.0, -3.3, 3.0),
            filter(FilterType::Pk, 4300.0, 2.2, 4.0),
            filter(FilterType::Pk, 6500.0, -6.0, 4.0),
            filter(FilterType::Pk, 8200.0, 1.1, 5.0),
            filter(FilterType::Pk, 11_000.0, -1.7, 3.5),
            filter(FilterType::Hsc, 10_000.0, -2.5, 0.7),
        ];
        let with_equalizer = Settings::new(&peqdb, -10.4, &filters);
        assert_eq!((with_equalizer.eq_gain, with_equalizer.bands.len()), (0.30199517, 15));
        assert_eq!(fingerprint(with_equalizer), "409446307b239ecc521e94eb89537213216c60a8d81394497676457d0db44d55");
        // no LFE, no delay
        assert_eq!(
            fingerprint(Settings::new(&downmix::resolve("apple", Some(0.0)).unwrap(), 0.0, &[])),
            "53ea024f3951ebb444f5c5f5322319bc074357bc1b9c689aab7bdfa4a6988388"
        );
        // an LFE that is not low-passed, a preamp that is not a round number
        let itu = Settings::new(&downmix::resolve("itu", Some(-7.4)).unwrap(), 0.0, &[]);
        assert_eq!(itu.preamp, 0.42657952);
        assert_eq!(fingerprint(itu), "861f026a6ab480f4f709be4de108675fb3a6575594b344df7f3d0a545a1389a5");
        // mixers whose gains are all the same sum first, a longer delay, another low-pass
        let equal = Settings {
            gains: [0.5; 8],
            lfe: 0.5,
            lfe_lowpass: 80.0,
            lfe_q: [0.5411961, 1.306563],
            lfe_delay: 0.01,
            preamp: 0.75,
            eq_gain: 1.25,
            bands: vec![Band { kind: FilterType::Pk, freq: 1000.0, q: 1.0, gain: 3.0 }],
        };
        assert_eq!(fingerprint(equal), "4f6e25610c56b1d0b9783bbd58bb7e5bad2111e89333d94a5cf30f645e17b5f7");
    }

    #[test]
    #[should_panic(expected = "frames in, stereo frames out")]
    fn the_output_has_to_fit() {
        let mut chain = Chain::new(peqdb(), RATE);
        chain.process(&[0.0; 2 * CHANNELS], &mut [0.0; 3]);
    }
}
