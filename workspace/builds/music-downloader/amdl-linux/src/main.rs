// Private experimental Linux harness of Kyant0/AMDL's GPL-3.0 Rust modules.
// Pipe messages carry challenges/licenses only between the local controller
// and this process. Never log them or write content keys to disk.
#[path = "../../amdl-android-src/rust/src/drm.rs"]
mod drm;
// decrypt.rs with tools/drm-helper/patches applied; build.sh writes it from the pinned source.
#[path = "../../amdl-patched/decrypt.rs"]
mod decrypt;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

// Sent with the challenge so the controller knows what this build understands.
// 2: "prefetch_key" in the first message (false: the playlist lists the song's key alone).
const PROTOCOL: u64 = 2;

fn read_message(input: &mut impl BufRead) -> Result<Value, &'static str> {
    let mut line = String::new();
    input.read_line(&mut line).map_err(|_| "IPC input failed")?;
    serde_json::from_str(&line).map_err(|_| "Invalid IPC message")
}

fn send(value: Value) -> Result<(), &'static str> {
    println!("{}", value);
    io::stdout().flush().map_err(|_| "IPC output failed")
}

fn run() -> Result<(), &'static str> {
    let mut input = io::stdin().lock();
    let start = read_message(&mut input)?;
    let pssh = STANDARD.decode(start["pssh"].as_str().ok_or("Missing PSSH")?)
        .map_err(|_| "Invalid PSSH base64")?;
    let cdm = drm::create_cdm().ok_or("Bundled CDM/device initialization failed")?;
    let request = drm::create_cdm_license_request(&cdm, &pssh).ok_or("License challenge creation failed")?;
    let challenge = drm::get_challenge(&request).ok_or("License challenge serialization failed")?;
    let prefetch = start["prefetch_key"].as_bool().unwrap_or(true);
    send(json!({"challenge": STANDARD.encode(challenge), "protocol": PROTOCOL}))?;
    let response = read_message(&mut input)?;
    let license = STANDARD.decode(response["license"].as_str().ok_or("Missing license")?)
        .map_err(|_| "Invalid license base64")?;
    let key = drm::get_content_key(&request, &license).ok_or("License contains no usable content key")?;
    let key: [u8; 16] = key.try_into().map_err(|_| "Unexpected content key size")?;
    send(json!({"license_accepted": true}))?;
    let next = read_message(&mut input)?;
    if next["decrypt"].as_bool() != Some(true) {
        return Err("Missing decryption instruction");
    }
    let encrypted = start["encrypted_file"].as_str().ok_or("Missing input path")?;
    let output = start["output_file"].as_str().ok_or("Missing output path")?;
    let data = std::fs::read(encrypted).map_err(|_| "Cannot read encrypted media")?;
    let plaintext = decrypt::decrypt_m4a(&key, data, prefetch).map_err(|_| "MP4 decryption failed")?;
    let bytes = plaintext.len();
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(output)
        .map_err(|_| "Cannot create output file (existing output is preserved)")?;
    file.write_all(&plaintext).map_err(|_| "Output write failed")?;
    file.sync_all().map_err(|_| "Output sync failed")?;
    send(json!({"decrypted": true, "output_bytes": bytes}))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{}", error);
        std::process::exit(1);
    }
}
