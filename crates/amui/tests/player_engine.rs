//! The player on the engine's own pipeline (`EnginePipeline`) and caches, in a scratch folder.
//!
//! Cut off from the world: the HTTP layer is a stand-in that fails every request and counts
//! them, the programs of the downmix sink are not there, the output is a stand-in and no
//! renderer is started. Where a test needs audio, ffmpeg encodes two seconds of a tone and the
//! engine decodes them again; without ffmpeg that test says so and ends.

#[path = "player/support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use amui::cache::{MediaCache, PcmCache};
use amui::loudness;
use amui::player::{Output, Pipeline, Player, PlayerEvent, PlayerOptions};
use amui::types::{AudioFormat, AudioMode, PlaybackSnapshot, PlayerState, RepeatMode, Track};
use amui::util::Event;
use serde_json::json;

use support::{Engine, FakeOutput, PATIENCE, lock, name_of, patient};

const SONG: &str = "1000000001";

fn song() -> Track {
    Track::new(Some(SONG), "Song A", "Artist A")
}

/// A player on an engine's pipeline and caches, with a stand-in output.
struct Rig {
    player: Player,
    output: Arc<FakeOutput>,
    events: Arc<Mutex<Vec<PlayerEvent>>>,
    media: Arc<MediaCache>,
    engine: Arc<Engine>,
}

impl Rig {
    fn new() -> Rig {
        Rig::on(Arc::new(Engine::new()))
    }

    /// A player in the folder of `engine` (one at a time: the earlier one has shut down).
    fn on(engine: Arc<Engine>) -> Rig {
        let output = Arc::new(FakeOutput::new(1.0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let media = Arc::new(MediaCache::new(engine.config.clone()));
        let options = PlayerOptions {
            media_cache: Some(media.clone()),
            output: Some(output.clone() as Arc<dyn Output>),
            tuning: patient(),
            ..PlayerOptions::default()
        };
        let heard = events.clone();
        let listener = move |event: PlayerEvent| lock(&heard).push(event);
        let (config, pipeline, log) = (engine.config.clone(), engine.pipeline.clone(), engine.log.clone());
        let player = Player::new(config, pipeline, log, listener, options).unwrap();
        Rig { player, output, events, media, engine }
    }

    fn wait(&self, what: &str, wanted: impl Fn(&PlaybackSnapshot) -> bool) -> PlaybackSnapshot {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let snapshot = self.player.snapshot();
            if wanted(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {snapshot:?}; {:?}", self.errors());
            sleep(Duration::from_millis(5));
        }
    }

    fn errors(&self) -> Vec<(String, Option<String>, bool)> {
        let error = |event: &PlayerEvent| match event {
            PlayerEvent::Error { message, catalog_id, fatal } => Some((message.clone(), catalog_id.clone(), *fatal)),
            _ => None,
        };
        lock(&self.events).iter().filter_map(error).collect()
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.engine.log.path()).unwrap_or_default()
    }
}

fn ran(program: &mut Command) -> bool {
    program.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    program.status().is_ok_and(|status| status.success())
}

/// Two seconds of a 440 Hz tone as AAC; `None` when this machine cannot make that (no ffmpeg or
/// ffprobe, or no such encoder).
fn tone(folder: &Path) -> Option<PathBuf> {
    if !ran(Command::new("ffmpeg").arg("-version")) || !ran(Command::new("ffprobe").arg("-version")) {
        return None;
    }
    let path = folder.join("tone.m4a");
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg.args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
    ffmpeg.args(["-t", "2", "-ac", "2", "-c:a", "aac"]).arg(&path);
    ran(&mut ffmpeg).then_some(path)
}

#[test]
fn the_engine_pipeline_offers_what_this_engine_has() {
    let engine = Engine::new();
    let pipeline = &engine.pipeline;
    // the matrix profiles of the downmix sink, and no binaural renderer
    let profiles = pipeline.downmix_profiles();
    assert_eq!(profiles.iter().map(|profile| profile.key.as_str()).collect::<Vec<_>>(), ["peqdb", "apple", "itu"]);
    assert!(profiles.iter().all(|profile| profile.kind == "matrix"));
    assert!(!pipeline.binaural_available());
    let refused = pipeline.render_binaural(Path::new("song.m4a"), Path::new("out")).err().unwrap();
    assert_eq!(refused.to_string(), "this engine has no binaural renderer");
    // nothing is built or signed in here
    assert!(!pipeline.lossless_available());
    pipeline.shutdown_lossless();
    // the sink's saved state (its defaults: nothing was ever saved in this folder)
    assert_eq!(pipeline.downmix_current().as_deref(), Some("peqdb"));
    assert_eq!(pipeline.downmix_preamp(), Some(-11.0));
    assert_eq!(engine.programs.load(Ordering::SeqCst), 0);
    // a change needs the sink, which is not there
    let error = pipeline.downmix_apply("apple").unwrap_err();
    assert!(error.to_string().contains("not running"), "{error}");
    assert!(pipeline.downmix_set_preamp(-9.0).is_err());
    assert!(engine.programs.load(Ordering::SeqCst) > 0);
    // sources of the PCM cache
    let missing = engine.dir().join("missing");
    assert!(pipeline.complete_render(&missing).is_err());
    assert!(!pipeline.render_is_current(&missing));
    assert!(pipeline.pcm_file(&missing.join("stereo.f32le"), 2).is_err());
    let folder = engine.dir().join("pcm");
    fs::create_dir_all(&folder).unwrap();
    let pcm_path = folder.join("stereo.f32le");
    fs::write(&pcm_path, vec![0u8; 480 * 8]).unwrap();
    let file = pipeline.pcm_file(&pcm_path, 2).unwrap();
    assert_eq!((file.channels(), file.frames_available(), file.total_frames(), file.done()), (2, 480, Some(480), true));
    assert_eq!((file.summary(), file.running(), file.paused(), file.started_at()), (None, None, false, None));
    assert!(file.wait_for(480, 0.0) && file.wait_for(10_000, 0.0), "a complete file has every frame there is");
    let silence = pipeline.measure_loudness(&pcm_path, 2, None, &Event::new()).unwrap();
    assert_eq!((silence.lufs, silence.frames), (None, 480));
    // a render that is not started is no process; the output the pipeline makes is not started
    let session = pipeline.render_session(Path::new("song.m4a"), &folder.join("render"), Some(2.0));
    assert_eq!((session.channels(), session.total_frames(), session.done()), (16, Some(96_000), false));
    assert_eq!((session.running(), session.started_at()), (Some(false), None));
    assert!(session.start().is_err(), "there is no render script in this folder");
    let output = pipeline.make_output(0.5).unwrap();
    assert!(!output.alive());
    assert_eq!(engine.network.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn a_song_that_cannot_be_acquired_is_reported() {
    let rig = Rig::new();
    let snapshot = rig.wait("the sink's state", |snapshot| snapshot.preamp.is_some());
    assert_eq!((snapshot.downmix_profile.as_deref(), snapshot.preamp), (Some("peqdb"), Some(-11.0)));
    assert!(!snapshot.lossless_available);
    rig.player.play(vec![song()], 0);
    let snapshot = rig.wait("the failure", |snapshot| snapshot.state == PlayerState::Error);
    let expected = "could not play Song A: DRM helper binary missing (build the workspace's amdl-linux harness)";
    assert_eq!(snapshot.error.as_deref(), Some(expected));
    assert_eq!(rig.errors(), [(expected.to_string(), Some(SONG.to_string()), false)]);
    assert_eq!(rig.output.st().starts, 0);
    assert_eq!(rig.engine.network.asked.load(Ordering::SeqCst), 0, "nothing asked the network");
}

#[test]
fn settings_are_kept_in_the_file_the_python_engine_reads() {
    let rig = Rig::new();
    rig.wait("the sink's state", |snapshot| snapshot.downmix_profile.is_some());
    rig.player.set_volume(0.3);
    rig.wait("the volume", |snapshot| snapshot.volume == 0.3);
    rig.player.shutdown();
    let path = rig.engine.config.state_dir.join("player.json");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // sorted keys, one space of indentation, a final line end: `json.dump(..., indent=1, sort_keys=True)`
    let expected = [
        "{",
        " \"bass_feed\": false,",
        " \"downmix_profile\": \"peqdb\",",
        " \"lossless\": true,",
        " \"mode\": \"atmos\",",
        " \"repeat\": \"off\",",
        " \"shuffle\": false,",
        " \"sound_check\": true,",
        " \"stereo_fallback\": true,",
        " \"volume\": 0.3",
        "}",
        "",
    ];
    assert_eq!(fs::read_to_string(&path).unwrap(), expected.join("\n"));

    // a file with things this engine does not know or does not accept
    let saved = json!({
        "mode": "stereo", "downmix_profile": "apple", "volume": 7, "stereo_fallback": "no", "repeat": "twice",
        "shuffle": true, "sound_check": false, "bass_feed": true, "lossless": false, "later": [1, 2],
    });
    fs::write(&path, saved.to_string()).unwrap();
    assert!(!rig.engine.config.bass_manage());
    let again = Rig::on(rig.engine.clone());
    let snapshot = again.player.snapshot();
    assert_eq!((snapshot.mode, snapshot.volume, snapshot.shuffle), (AudioMode::Stereo, 1.0, true));
    assert_eq!((snapshot.sound_check, snapshot.bass_manage, snapshot.lossless), (false, true, false));
    assert_eq!(snapshot.repeat, RepeatMode::Off, "an unknown repeat mode keeps the default");
    assert!(again.player.stereo_fallback(), "a value of another type keeps the default");
    assert!(again.engine.config.bass_manage(), "the renderer's setting follows the saved one");

    // the profile the sink really runs is reported, whatever was saved
    again.wait("the live profile", |snapshot| snapshot.downmix_profile.as_deref() == Some("peqdb"));
    // the switch saved under its old name, when the feed was on by default, is not taken over
    again.player.shutdown();
    fs::write(&path, json!({"bass_manage": true}).to_string()).unwrap();
    let fresh = Arc::new(Engine::new());
    fs::create_dir_all(&fresh.config.state_dir).unwrap();
    fs::copy(&path, fresh.config.state_dir.join("player.json")).unwrap();
    let old = Rig::on(fresh);
    assert!(!old.player.snapshot().bass_manage);
    assert!(!old.engine.config.bass_manage());
}

#[test]
fn a_cached_song_is_decoded_played_and_measured() {
    let rig = Rig::new();
    let Some(tone) = tone(rig.engine.dir()) else {
        eprintln!("note: no ffmpeg with an AAC encoder here: nothing to decode, the test ends");
        return;
    };
    let key = format!("{SONG}.stereo");
    let meta = json!({"audio_group": "audio-stereo-256", "codecs": "mp4a.40.2", "bandwidth": "256000"});
    rig.media.put(&key, &tone, meta.as_object().unwrap()).unwrap();
    rig.player.set_mode(AudioMode::Stereo);
    rig.player.play(vec![song()], 0);
    let snapshot = rig.wait("playing", |snapshot| snapshot.state == PlayerState::Playing);
    assert!(rig.errors().is_empty(), "{:?}", rig.errors());
    assert_eq!((snapshot.format, snapshot.format_detail.as_str()), (Some(AudioFormat::Stereo), "AAC 256 kbps"));
    // the decode is in the PCM cache, marked as in use by this process
    let folder = rig.engine.config.pcm_cache_dir.join(&key);
    let pcm_path = folder.join("stereo.f32le");
    let active = rig.output.active().unwrap();
    assert_eq!((active.pcm_path(), active.channels()), (pcm_path.as_path(), 2));
    let frames = active.frames_available();
    assert!((90_000..110_000).contains(&frames), "two seconds of audio: {frames}");
    assert!((snapshot.duration.unwrap() - frames as f64 / 48_000.0).abs() < 1e-9);
    let marker = folder.join(format!(".inuse-{}", std::process::id()));
    assert!(marker.is_file());
    assert_eq!(PcmCache::new(&rig.engine.config).complete(&key).unwrap(), Some(folder.clone()));
    // measured while the start waited for it, and stored next to the PCM
    let stored = loudness::stored_loudness(&pcm_path).expect("the measurement is stored");
    assert_eq!(stored.frames, Some(frames));
    let lufs = stored.lufs.expect("a tone is not silence");
    let trim = loudness::trim_db(Some(lufs), loudness::TARGET_LUFS);
    assert_eq!(snapshot.track_gain_db, Some(trim));
    assert_eq!(rig.output.trim_at_start(&name_of(&active)), Some(trim), "the trim was there at the start");
    let log = rig.log_text();
    assert!(log.contains(&format!("load {key} @ 0")), "{log}");
    assert!(log.contains(&format!("loudness {key}: {lufs:.1} LUFS, peak ")), "{log}");
    assert!(log.contains(&format!("play {key} @ 0 (AAC 256 kbps; trim {trim:+.1} dB)")), "{log}");
    assert!(!log.contains("no value after"), "{log}");
    // the end of the song ends the queue; shutdown takes the marker away and leaves the decode
    rig.output.finish_current(false);
    rig.wait("the end of the queue", |snapshot| snapshot.state == PlayerState::Idle);
    rig.player.shutdown();
    assert!(!marker.exists());
    let decoded = fs::metadata(&pcm_path).unwrap().modified().unwrap();

    // another player in the same folder uses the complete decode and its stored measurement
    let again = Rig::on(rig.engine.clone());
    again.player.set_mode(AudioMode::Stereo);
    again.player.play(vec![song()], 0);
    let snapshot = again.wait("playing again", |snapshot| snapshot.state == PlayerState::Playing);
    assert_eq!(snapshot.track_gain_db, Some(trim));
    assert_eq!(fs::metadata(&pcm_path).unwrap().modified().unwrap(), decoded, "nothing was decoded again");
    let log = again.log_text();
    assert_eq!(log.matches(&format!("loudness {key}: ")).count(), 1, "one measurement: {log}");
    assert_eq!(log.matches(&format!("play {key} @ 0 (AAC 256 kbps; trim {trim:+.1} dB)")).count(), 2, "{log}");
    assert!(marker.is_file());
    again.player.shutdown();
    assert!(!marker.exists());
    assert_eq!(rig.engine.network.asked.load(Ordering::SeqCst), 0, "nothing asked the network");
    assert_eq!(rig.engine.programs.load(Ordering::SeqCst), 0, "no program of the sink was asked for");
}

/// With the engine's own downmix, the output the pipeline makes runs the downmix's values: the
/// saved profile at once, and every change after (no sink, no program, no sound).
#[test]
fn the_output_runs_the_engine_s_own_downmix() {
    use amui::downmix::{Downmix, Paths};
    use amui::dsp;

    let engine = Engine::new();
    let config = engine.config.clone();
    let downmix = Arc::new(Downmix::in_app_with(config.clone(), Paths::new(&config)));
    let network = engine.network.clone();
    let lossless = Arc::new(amui::lossless::Lossless::new(config.clone(), network.clone()));
    let acquirer = Arc::new(amui::acquire::Acquirer::new(config.clone(), network, lossless.clone()));
    let pipeline =
        amui::player::EnginePipeline::new(config.clone(), acquirer, lossless, downmix.clone(), engine.log.clone());
    let output = pipeline.make_output(0.5).unwrap();
    assert!(!output.alive(), "made, not started");
    let set = |preamp_db: f64| format!("set_dsp preamp {:.4}", 10f64.powf(preamp_db / 20.0));
    let logged = || fs::read_to_string(engine.log.path()).unwrap_or_default();
    // at first the saved profile (none saved here: the workspace's default, PEQdB)
    let start = downmix.live_controls().unwrap();
    let expected = dsp::Settings::from_controls(&start, &start);
    assert!(logged().contains(&format!("set_dsp preamp {:.4}", expected.preamp)), "{}", logged());
    // a profile and a preamp
    downmix.apply("itu", Some(-7.0), Some(false)).unwrap();
    assert!(logged().contains(&set(-7.0)), "{}", logged());
    // the equalizer on the same downmix
    let eq = amui::eq::Equalizer::new(downmix.clone()).with_path(Some(engine.dir().join("eq.json")));
    let preset =
        eq.add_preset("Preset A", "Preamp: -4.0 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3.0 dB Q 1.0", "pasted").unwrap();
    eq.set_active(Some(&preset.id)).unwrap();
    eq.set_enabled(true).unwrap();
    eq.apply(0.0).unwrap();
    assert!(logged().contains("eq gain 0.6310 filters 1"), "{}", logged());
}
