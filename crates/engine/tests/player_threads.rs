//! The player's part of `test_import_starts_no_threads` of `tests/test_output.py`: importing the
//! player module must not start a thread.
//!
//! A Rust library runs nothing when it is linked, so the port checks what the rule means here:
//! everything a player is built from (the engine's pipeline with its acquirer, lossless route
//! and downmix sink, the caches, the playback log, an output that has not been started) starts
//! no thread and no program; a player has exactly one thread, its worker, until something is
//! played; and `shutdown()` leaves none behind. The test is alone in this test program, so the
//! threads it sees are its own.

#[path = "player/support.rs"]
mod support;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant};

use amui::cache::{MediaCache, PcmCache};
use amui::player::{Output, Player, PlayerOptions};
use amui::types::PlayerState;

use support::{Engine, FakeOutput};

/// The names of this process's threads, sorted.
fn thread_names() -> Vec<String> {
    let tasks = fs::read_dir("/proc/self/task").unwrap();
    let mut names: Vec<String> = tasks
        .filter_map(|task| fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .map(|name| name.trim().to_string())
        .collect();
    names.sort();
    names
}

/// True as soon as `condition` holds, false when it does not within ten seconds. (The thread
/// list follows a moment behind: a thread names itself once it runs, and a joined thread is
/// still on its way out.)
fn soon(condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() && Instant::now() < deadline {
        sleep(Duration::from_millis(5));
    }
    condition()
}

#[test]
fn import_starts_no_threads() {
    let before = thread_names();
    let engine = Engine::new();
    let config = engine.config.clone();
    let output = Arc::new(FakeOutput::new(1.0));
    let options = PlayerOptions {
        media_cache: Some(Arc::new(MediaCache::new(config.clone()))),
        pcm_cache: Some(Arc::new(PcmCache::new(&config))),
        output: Some(output.clone() as Arc<dyn Output>),
        ..PlayerOptions::default()
    };
    assert_eq!(thread_names(), before, "what a player is built from starts no thread");

    let player = Player::new(config, engine.pipeline.clone(), engine.log.clone(), |_| {}, options).unwrap();
    let mut with_worker = before.clone();
    with_worker.push("am-ui-player".to_string());
    with_worker.sort();
    assert!(soon(|| thread_names() == with_worker), "{:?}", thread_names());
    // looking at the player and changing a setting takes no further thread and no program
    let snapshot = player.snapshot();
    assert_eq!((snapshot.state, snapshot.queue_length, snapshot.index), (PlayerState::Idle, 0, -1));
    assert!(player.queue().is_empty());
    assert_eq!(player.downmix_profiles().len(), 3);
    player.set_volume(0.5);
    player.set_sound_check(false);
    assert!(soon(|| player.snapshot().volume == 0.5 && !player.snapshot().sound_check));
    assert_eq!(thread_names(), with_worker);
    assert_eq!(output.st().starts, 0, "the output starts on the first play");

    player.shutdown();
    assert!(soon(|| thread_names() == before), "shutdown() leaves no thread behind: {:?}", thread_names());
    assert_eq!(engine.network.asked.load(Ordering::SeqCst), 0, "nothing asked the network");
    assert_eq!(engine.programs.load(Ordering::SeqCst), 0, "no program of the sink was asked for");
    assert!(!before.iter().any(|name| name.starts_with("am-ui")));
}
