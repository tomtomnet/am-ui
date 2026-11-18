//! am-ui's engine: Apple Music library and catalog access, song acquisition (Dolby Atmos, AAC and
//! lossless), rendering, PipeWire output and the playback state machine. No user interface here.
//!
//! Everything is synchronous: callers run network and file work on worker threads. A [`Config`]
//! names every path the engine touches, so nothing in here reads the process environment or a
//! global after start-up, and tests run against scratch folders.
//!
//! [`Config`]: config::Config

pub mod acquire;
pub mod api;
pub mod cache;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod downmix;
pub mod dsp;
pub mod eq;
pub mod lossless;
pub mod loudness;
pub mod lyrics;
pub mod output;
pub mod player;
pub mod render;
pub mod session;
pub mod types;
pub mod util;
pub mod web;
pub mod wrapper;
