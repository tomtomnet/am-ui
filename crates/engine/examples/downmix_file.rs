//! Downmix a 16-channel render to stereo with the application's own chain (`amui::dsp`), for
//! comparisons with a reference recording and for listening tests away from the player.
//!
//! ```text
//! cargo run --release -p amui --example downmix_file -- IN.f32le OUT.f32le [PROFILE [PREAMP_DB]]
//! ```
//!
//! `IN`: raw little-endian 32-bit floats at 48 kHz, 16 interleaved channels in the order of
//! `config::CHANNEL_MAP` (what the renderer writes, `atmos-916.f32le`). `OUT`: stereo frames in
//! the same format. `PROFILE`: `peqdb` (the default), `apple` or `itu`; `PREAMP_DB`: the
//! profile's own preamp when left out. The equalizer is flat.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::ExitCode;

use amui::config::CHANNELS;
use amui::{downmix, dsp};

const RATE: u32 = 48_000;
/// Frames read and written at a time.
const BLOCK: usize = 4_800;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=4).contains(&args.len()) {
        eprintln!("usage: downmix_file IN.f32le OUT.f32le [PROFILE [PREAMP_DB]]");
        return ExitCode::from(2);
    }
    match run(&args) {
        Ok(frames) => {
            eprintln!("{frames} frames ({:.3} s)", frames as f64 / f64::from(RATE));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("downmix_file: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<usize, String> {
    let profile = args.get(2).map_or("peqdb", String::as_str);
    let preamp = match args.get(3) {
        Some(text) => Some(text.parse::<f64>().map_err(|_| format!("not a number of dB: {text}"))?),
        None => None,
    };
    let params = downmix::resolve(profile, preamp).ok_or_else(|| format!("no profile called {profile}"))?;
    let mut chain = dsp::Chain::new(dsp::Settings::new(&params, 0.0, &[]), RATE);

    let input = File::open(&args[0]).map_err(|error| format!("{}: {error}", args[0]))?;
    let output = File::create(&args[1]).map_err(|error| format!("{}: {error}", args[1]))?;
    let mut input = BufReader::new(input);
    let mut output = BufWriter::new(output);

    let mut bytes = vec![0_u8; BLOCK * CHANNELS * 4];
    let mut samples = vec![0_f32; BLOCK * CHANNELS];
    let mut stereo = vec![0_f32; BLOCK * 2];
    let mut total = 0;
    loop {
        let filled = read_up_to(&mut input, &mut bytes).map_err(|error| format!("{}: {error}", args[0]))?;
        let frames = filled / (CHANNELS * 4);
        if frames == 0 {
            break;
        }
        let used = frames * CHANNELS;
        for (sample, raw) in samples[..used].iter_mut().zip(bytes.chunks_exact(4)) {
            *sample = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        }
        chain.process(&samples[..used], &mut stereo[..frames * 2]);
        for sample in &stereo[..frames * 2] {
            output.write_all(&sample.to_le_bytes()).map_err(|error| format!("{}: {error}", args[1]))?;
        }
        total += frames;
        if filled < bytes.len() {
            break;
        }
    }
    output.flush().map_err(|error| format!("{}: {error}", args[1]))?;
    Ok(total)
}

/// Fill `buffer` as far as the file goes; the count read.
fn read_up_to(input: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match input.read(&mut buffer[filled..])? {
            0 => break,
            count => filled += count,
        }
    }
    Ok(filled)
}
