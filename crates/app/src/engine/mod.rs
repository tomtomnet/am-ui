//! The application on the real engine (`amui`): what `data::Music`, `playback::Playback` and
//! `account::AccountService` ask for is answered by Apple's service and played through the
//! engine's player.

pub mod account;
pub mod equalizer;
pub mod music;
pub mod playback;
