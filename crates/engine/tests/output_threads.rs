//! `test_import_starts_no_threads` of `tests/test_output.py`: importing the output module (and
//! the player and command line built on it) must not start a thread.
//!
//! A Rust library runs nothing when it is linked, so the port checks what the rule means here:
//! the playback log, an output that has not been started and a loudness meter start no thread;
//! the feeder appears with `start()` and is gone after `stop()`. The test is alone in this test
//! program, so the threads it sees are its own. (The player's part of the Python test belongs
//! to the player's port.)

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use amui::config::{Config, SAMPLE_RATE};
use amui::loudness::Meter;
use amui::output::{KeepPriority, OutputOptions, PipeWireOutput, PlaybackLog};

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

#[test]
fn import_starts_no_threads() {
    let before = thread_names();
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path().join("am-ui"), &dir.path().join("home"), &HashMap::new());
    let log = Arc::new(PlaybackLog::for_config(&config));
    let (reader, writer) = std::io::pipe().unwrap();
    let options = OutputOptions {
        consumer_fd: Some(OwnedFd::from(writer)),
        priority: Arc::new(KeepPriority), // the scheduler is not the test's business
        ..OutputOptions::default()
    };
    let output = PipeWireOutput::new(&config, log.clone(), options);
    output.on_source_started(|_| {});
    output.on_source_ended(|_| {});
    output.on_error(|_| {});
    output.set_volume(0.5);
    output.set_source(None, 0);
    output.set_sound_check(false);
    log.write("nothing is running");
    assert_eq!((output.position_frames(), output.frames_written(), output.alive()), (0, 0, false));
    let meter = Meter::new(SAMPLE_RATE);
    assert_eq!(meter.frames(), 0);
    assert_eq!(thread_names(), before, "creating and using an output that is not started starts no thread");
    assert!(!before.iter().any(|name| name.starts_with("am-ui")));

    // The check sees a feeder when there is one. The thread list follows a moment behind: a
    // thread names itself once it runs, and a joined thread is still on its way out.
    output.start().unwrap();
    let feeder = "am-ui-feeder".to_string();
    assert!(soon(|| thread_names().contains(&feeder)), "{:?}", thread_names());
    drop(reader); // the consumer goes away: the feeder's write fails and the thread ends
    output.stop();
    assert!(soon(|| thread_names() == before), "stop() leaves no thread behind: {:?}", thread_names());
}

/// True as soon as `condition` holds, false when it does not within five seconds.
fn soon(condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() && Instant::now() < deadline {
        sleep(Duration::from_millis(5));
    }
    condition()
}
