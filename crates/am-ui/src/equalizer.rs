//! The equalizer as the window sees it: the presets, which of them is active and whether it is
//! on. The engine's `eq` serves it in the real application (`engine::equalizer`), an equalizer
//! in memory in the demonstration.
//!
//! Every method blocks (the state file, the output's programs) and is called on worker threads
//! (`signal::background`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use amui::downmix::{self, Controls};
pub use amui::eq::{EqError, Preset, Snapshot};
use amui::eq::{Equalizer, LiveState};
use gtk::gio;
use gtk::prelude::*;

/// The largest file taken for a preset: far more than any of the formats needs.
pub const PRESET_FILE_MAX_BYTES: usize = 1024 * 1024;
/// How many files that could not be imported are named one by one.
const NAMED_FAILURES: usize = 3;

pub trait EqualizerService: Send + Sync + 'static {
    /// What is saved: the presets in their order, which of them is active, on or off. It is read
    /// again from where it is kept; nothing is applied.
    fn snapshot(&self) -> Snapshot;
    /// Switch the equalizer on or off and apply that.
    fn set_enabled(&self, on: bool) -> Result<(), EqError>;
    /// Make a preset the active one, switch the equalizer on and apply that.
    fn activate(&self, id: &str) -> Result<(), EqError>;
    /// Store EqualizerAPO / AutoEQ / PEQdB text as a new preset, the last of the list. The error
    /// names the line that is not understood.
    fn add(&self, name: &str, text: &str) -> Result<Preset, EqError>;
    /// The same for the text of a file: `name` is the file's, made different from the presets'.
    fn add_file(&self, name: &str, text: &str) -> Result<Preset, EqError>;
    fn rename(&self, id: &str, name: &str) -> Result<(), EqError>;
    /// Put a preset at `index` of the list.
    fn move_to(&self, id: &str, index: usize) -> Result<(), EqError>;
    /// Remove a preset. An equalizer that is on has nothing to run without its active preset:
    /// removing that one applies a flat response.
    fn delete(&self, id: &str) -> Result<(), EqError>;
    /// One line about what the output really runs now ("Sink: EQ on, 15 bands, -10.4 dB"), or
    /// nothing to say. Only reads. It goes away with the shared sink: see `live_note` below.
    fn live_note(&self) -> Option<String>;
}

/// "-10.4", "+0.0": dB with one decimal, and no minus sign on a zero.
pub fn signed(db: f64) -> String {
    let rounded = (db * 10.0).round() / 10.0 + 0.0;
    format!("{rounded:+.1}")
}

// --- presets from files ----------------------------------------------------------------------

/// What a preset from a file is called: the file's name without its extension.
pub fn preset_name(file_name: &str) -> String {
    let path = Path::new(file_name);
    path.file_stem().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

/// The text of a preset file: UTF-8 (with or without its mark), UTF-16 with its mark, or any other
/// 8-bit encoding (a comment in one does no harm: only the ASCII parts are read). `None` for what
/// is not text.
pub fn preset_text(bytes: &[u8]) -> Option<String> {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|pair| unit(*pair)).collect();
        String::from_utf16(&units).ok()
    };
    match bytes {
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xff, 0xfe, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xfe, 0xff, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ if bytes.contains(&0) => None,
        _ => Some(match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => bytes.iter().map(|byte| char::from(*byte)).collect(),
        }),
    }
}

/// What a file holds, at most `PRESET_FILE_MAX_BYTES`. Blocks: for a worker thread.
fn read_file(file: &gio::File) -> Result<Vec<u8>, String> {
    if file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE) == gio::FileType::Directory {
        return Err("it is a folder".to_string());
    }
    let reason = |error: gtk::glib::Error| error.message().to_string();
    let stream = file.read(gio::Cancellable::NONE).map_err(reason)?;
    let mut bytes = Vec::new();
    loop {
        let chunk = stream.read_bytes(64 * 1024, gio::Cancellable::NONE).map_err(reason)?;
        if chunk.is_empty() {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > PRESET_FILE_MAX_BYTES {
            return Err(format!("it is larger than {} MiB", PRESET_FILE_MAX_BYTES / (1024 * 1024)));
        }
    }
}

/// Why text is no preset, short enough for a message that also names the file: the parser's
/// reason without the line it quotes.
fn parse_problem(error: &EqError) -> String {
    let text = error.to_string();
    let mut parts = text.splitn(3, ": ");
    match (error.line(), parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(line), Some(reason), Some(_quoted)) => format!("{line}: {reason}"),
        _ => text,
    }
}

/// One file into a preset, by the parser pasted text goes through.
fn import_file(service: &dyn EqualizerService, name: &str, file: &gio::File) -> Result<Preset, String> {
    let bytes = read_file(file)?;
    let text = preset_text(&bytes).ok_or("it is not a text file")?;
    let (preamp, filters) = amui::eq::parse(&text).map_err(|error| parse_problem(&error))?;
    if filters.is_empty() && preamp == 0.0 {
        return Err("it has no Preamp or Filter lines".to_string());
    }
    service.add_file(&preset_name(name), &text).map_err(|error| error.to_string())
}

/// What became of some files: the names of the presets they gave, and each other file's name with
/// what is wrong with it.
#[derive(Debug, Default, PartialEq)]
pub struct FileImport {
    pub added: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl FileImport {
    /// What to tell the user, one message per line of news.
    pub fn messages(&self) -> Vec<String> {
        let mut messages = Vec::new();
        match self.added.as_slice() {
            [] => {}
            [name] => messages.push(format!("“{name}” added")),
            names => messages.push(format!("{} presets added", names.len())),
        }
        for (name, reason) in self.failed.iter().take(NAMED_FAILURES) {
            messages.push(format!("“{name}” was not imported: {reason}"));
        }
        match self.failed.len().saturating_sub(NAMED_FAILURES) {
            0 => {}
            1 => messages.push("1 more file was not imported".to_string()),
            more => messages.push(format!("{more} more files were not imported")),
        }
        messages
    }
}

/// Turn files into presets, each on its own: one that cannot be read or parsed does not keep the
/// others out. Blocks: for a worker thread.
pub fn import_files(service: &dyn EqualizerService, files: &[gio::File]) -> FileImport {
    let mut outcome = FileImport::default();
    for file in files {
        let name = file.basename().map_or_else(|| file.uri().to_string(), |name| name.to_string_lossy().into_owned());
        match import_file(service, &name, file) {
            Ok(preset) => outcome.added.push(preset.name),
            Err(reason) => outcome.failed.push((name, reason)),
        }
    }
    outcome
}

// --- the changes on the engine's equalizer -------------------------------------------------
//
// The same steps for the real output (`engine::equalizer`) and for the demonstration's
// stand-in. The gain stage gets the preset's preamp alone: loudness is applied by the player.

pub fn switch(equalizer: &Equalizer, on: bool) -> Result<(), EqError> {
    equalizer.set_enabled(on)?;
    equalizer.apply(0.0).map(drop)
}

pub fn activate(equalizer: &Equalizer, id: &str) -> Result<(), EqError> {
    equalizer.set_active(Some(id))?;
    if !equalizer.enabled() {
        equalizer.set_enabled(true)?;
    }
    equalizer.apply(0.0).map(drop)
}

pub fn delete(equalizer: &Equalizer, id: &str) -> Result<(), EqError> {
    let before = equalizer.snapshot();
    equalizer.delete_preset(id)?;
    if before.enabled && before.active.as_deref() == Some(id) {
        equalizer.apply(0.0)?;
    }
    Ok(())
}

// --- the note about the sink -----------------------------------------------------------------
//
// For as long as the equalizer runs in a sink that other applications set too, the dialog says
// in one line what that sink has. All of it is this section, `EqualizerService::live_note`, and
// the label `note` with its timer in `widgets/equalizer.rs`: it goes when the equalizer is
// computed inside the application.

/// What the sink of `equalizer` runs, and a hint when that is not what the equalizer set.
pub fn live_note(equalizer: &Equalizer) -> Option<String> {
    let snapshot = equalizer.snapshot();
    let active = snapshot.presets.iter().find(|preset| Some(&preset.id) == snapshot.active.as_ref());
    // the bands the equalizer asks for: a filter without gain leaves its band flat
    let wanted = match active {
        Some(preset) if snapshot.enabled => preset.filters.iter().filter(|filter| filter.gain.abs() > 1e-6).count(),
        _ => 0,
    };
    Some(note(&equalizer.live_state(), snapshot.enabled, wanted))
}

fn note(live: &Result<LiveState, EqError>, enabled: bool, wanted: usize) -> String {
    let live = match live {
        Ok(live) => live,
        Err(error) => return format!("Sink: {error}"),
    };
    let bands = if live.bands == 1 { "1 band".to_string() } else { format!("{} bands", live.bands) };
    let mut note = format!("Sink: EQ {}, {bands}", if live.enabled_in_sink { "on" } else { "off" });
    if let Some(gain) = live.gain_db {
        note.push_str(&format!(", {} dB", signed(gain)));
    }
    if live.bands == wanted {
        // as the equalizer left it, as far as the bands tell
    } else if enabled && live.bands == 0 {
        note.push_str(" · flat now: another app changed it, toggle to apply again");
    } else {
        note.push_str(" · set by another app");
    }
    note
}

// --- the demonstration ---------------------------------------------------------------------

/// Made-up presets (EqualizerAPO text): nobody's headphones.
const SAMPLES: [(&str, &str); 4] = [
    (
        "Sample: Reference",
        "Preamp: -3.0 dB\n\
         Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.70\n\
         Filter 2: ON PK Fc 3000 Hz Gain -2.0 dB Q 1.50\n\
         Filter 3: ON HSC Fc 10000 Hz Gain 1.5 dB Q 0.70",
    ),
    (
        "Sample: Warm",
        "Preamp: -4.5 dB\n\
         Filter 1: ON LSC Fc 150 Hz Gain 4.5 dB Q 0.80\n\
         Filter 2: ON PK Fc 250 Hz Gain 1.5 dB Q 1.00\n\
         Filter 3: ON PK Fc 2500 Hz Gain -3.0 dB Q 1.40\n\
         Filter 4: ON PK Fc 6000 Hz Gain -2.5 dB Q 2.00\n\
         Filter 5: ON HSC Fc 9000 Hz Gain -3.0 dB Q 0.70",
    ),
    (
        "Sample: Bright",
        "Preamp: -5.0 dB\n\
         Filter 1: ON PK Fc 60 Hz Gain -2.0 dB Q 0.80\n\
         Filter 2: ON PK Fc 1200 Hz Gain -1.5 dB Q 1.20\n\
         Filter 3: ON PK Fc 4500 Hz Gain 3.0 dB Q 1.80\n\
         Filter 4: ON PK Fc 8000 Hz Gain 2.5 dB Q 2.00\n\
         Filter 5: ON HSC Fc 10000 Hz Gain 4.0 dB Q 0.70",
    ),
    (
        // fifteen filters, and a preamp that takes the dB axis past its ±12 dB
        "Sample: Detailed",
        "Preamp: -10.4 dB\n\
         Filter 1: ON LSC Fc 105 Hz Gain 6.2 dB Q 0.70\n\
         Filter 2: ON PK Fc 38 Hz Gain -2.1 dB Q 0.90\n\
         Filter 3: ON PK Fc 160 Hz Gain -3.4 dB Q 0.60\n\
         Filter 4: ON PK Fc 320 Hz Gain 1.2 dB Q 1.40\n\
         Filter 5: ON PK Fc 640 Hz Gain -1.8 dB Q 1.10\n\
         Filter 6: ON PK Fc 1100 Hz Gain 1.5 dB Q 1.80\n\
         Filter 7: ON PK Fc 1900 Hz Gain -2.6 dB Q 1.30\n\
         Filter 8: ON PK Fc 2800 Hz Gain 4.1 dB Q 1.60\n\
         Filter 9: ON PK Fc 4100 Hz Gain -3.2 dB Q 2.40\n\
         Filter 10: ON PK Fc 5600 Hz Gain 2.4 dB Q 3.00\n\
         Filter 11: ON PK Fc 7400 Hz Gain -4.5 dB Q 2.80\n\
         Filter 12: ON PK Fc 9200 Hz Gain 2.2 dB Q 2.00\n\
         Filter 13: ON PK Fc 12500 Hz Gain -1.6 dB Q 1.50\n\
         Filter 14: ON PK Fc 15500 Hz Gain 3.0 dB Q 1.20\n\
         Filter 15: ON HSC Fc 10000 Hz Gain 5.5 dB Q 0.70",
    ),
];

/// What the demonstration starts as. The default is the equalizer in use; the others are the
/// states the dialog has to show as well (`--show equalizer-off`, `--show equalizer-empty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DemoStart {
    /// On, with the second preset active.
    #[default]
    On,
    /// Off, with that preset still the chosen one.
    Off,
    /// No presets at all.
    Empty,
}

impl DemoStart {
    /// The start that a `--show` name asks for.
    pub fn of_show(name: Option<&str>) -> DemoStart {
        match name {
            Some("equalizer-off") => DemoStart::Off,
            Some("equalizer-empty") => DemoStart::Empty,
            _ => DemoStart::On,
        }
    }
}

/// An equalizer in memory whose output is a stand-in that remembers what was applied: nothing
/// is read from or written to a file, and no sink is reached.
pub struct DemoEqualizer {
    equalizer: Equalizer,
}

impl DemoEqualizer {
    pub fn new(start: DemoStart) -> DemoEqualizer {
        let applied: Arc<Mutex<Option<Controls>>> = Arc::default();
        let (taking, giving) = (applied.clone(), applied);
        let equalizer = Equalizer::detached(
            move |values: &Controls| {
                taking.lock().unwrap().replace(values.clone());
                Ok(())
            },
            move || Ok(giving.lock().unwrap().clone().unwrap_or_else(|| downmix::eq_controls(0.0, &[]))),
        );
        if start != DemoStart::Empty {
            let added: Vec<Preset> =
                SAMPLES.iter().filter_map(|(name, text)| equalizer.add_preset(name, text, "sample").ok()).collect();
            // an equalizer in memory has no file that could fail to be written
            let _ = equalizer.set_active(added.get(1).map(|preset| preset.id.as_str()));
            if start == DemoStart::On {
                let _ = switch(&equalizer, true);
            }
        }
        DemoEqualizer { equalizer }
    }
}

impl EqualizerService for DemoEqualizer {
    fn snapshot(&self) -> Snapshot {
        self.equalizer.snapshot()
    }

    fn set_enabled(&self, on: bool) -> Result<(), EqError> {
        switch(&self.equalizer, on)
    }

    fn activate(&self, id: &str) -> Result<(), EqError> {
        activate(&self.equalizer, id)
    }

    fn add(&self, name: &str, text: &str) -> Result<Preset, EqError> {
        self.equalizer.add_preset(name, text, "pasted")
    }

    fn add_file(&self, name: &str, text: &str) -> Result<Preset, EqError> {
        self.equalizer.add_file_preset(name, text)
    }

    fn rename(&self, id: &str, name: &str) -> Result<(), EqError> {
        self.equalizer.rename_preset(id, name).map(drop)
    }

    fn move_to(&self, id: &str, index: usize) -> Result<(), EqError> {
        self.equalizer.move_preset(id, i64::try_from(index).unwrap_or(i64::MAX)).map(drop)
    }

    fn delete(&self, id: &str) -> Result<(), EqError> {
        delete(&self.equalizer, id)
    }

    fn live_note(&self) -> Option<String> {
        live_note(&self.equalizer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(demo: &DemoEqualizer) -> Vec<String> {
        demo.snapshot().presets.into_iter().map(|preset| preset.name).collect()
    }

    fn active_name(snapshot: &Snapshot) -> Option<&str> {
        snapshot.presets.iter().find(|preset| Some(&preset.id) == snapshot.active.as_ref()).map(|p| p.name.as_str())
    }

    /// What the stand-in output was given last: how many bands, and the gain in dB.
    fn applied(demo: &DemoEqualizer) -> (usize, f64) {
        let live = demo.equalizer.live_state().expect("the stand-in answers");
        (live.bands, (live.gain_db.unwrap_or(0.0) * 10.0).round() / 10.0)
    }

    #[test]
    fn the_demonstration_starts_in_use() {
        let demo = DemoEqualizer::new(DemoStart::default());
        assert_eq!(names(&demo), ["Sample: Reference", "Sample: Warm", "Sample: Bright", "Sample: Detailed"]);
        let snapshot = demo.snapshot();
        assert!(snapshot.enabled);
        assert_eq!(active_name(&snapshot), Some("Sample: Warm"));
        assert!(snapshot.presets.iter().all(|preset| preset.source == "sample"));
        assert_eq!(snapshot.presets[3].filters.len(), 15);
        assert_eq!(applied(&demo), (5, -4.5));
    }

    #[test]
    fn every_made_up_preset_is_a_preset() {
        for (name, text) in &SAMPLES {
            assert!(name.starts_with("Sample: "), "{name}");
            let (preamp, filters) = amui::eq::parse(text).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(preamp < 0.0 && !filters.is_empty(), "{name}");
        }
    }

    #[test]
    fn the_other_starts_are_what_they_say() {
        let off = DemoEqualizer::new(DemoStart::Off);
        assert!(!off.snapshot().enabled);
        assert_eq!(active_name(&off.snapshot()), Some("Sample: Warm"));
        assert_eq!(applied(&off), (0, 0.0));

        let empty = DemoEqualizer::new(DemoStart::Empty).snapshot();
        assert_eq!((empty.presets.len(), empty.active, empty.enabled), (0, None, false));

        assert_eq!(DemoStart::of_show(Some("equalizer")), DemoStart::On);
        assert_eq!(DemoStart::of_show(Some("equalizer-add")), DemoStart::On);
        assert_eq!(DemoStart::of_show(Some("equalizer-off")), DemoStart::Off);
        assert_eq!(DemoStart::of_show(Some("equalizer-empty")), DemoStart::Empty);
        assert_eq!(DemoStart::of_show(None), DemoStart::On);
    }

    #[test]
    fn activating_a_preset_switches_the_equalizer_on() {
        let demo = DemoEqualizer::new(DemoStart::Off);
        let detailed = demo.snapshot().presets[3].id.clone();
        demo.activate(&detailed).unwrap();
        let snapshot = demo.snapshot();
        assert!(snapshot.enabled);
        assert_eq!(snapshot.active.as_deref(), Some(detailed.as_str()));
        assert_eq!(applied(&demo), (15, -10.4));

        demo.set_enabled(false).unwrap();
        assert!(!demo.snapshot().enabled);
        assert_eq!(applied(&demo), (0, 0.0));
        // the preset stays the chosen one for the next time
        assert_eq!(demo.snapshot().active.as_deref(), Some(detailed.as_str()));
        assert_eq!(demo.activate("no-such-preset"), Err(EqError::Other("no such preset".to_string())));
    }

    #[test]
    fn deleting_the_active_preset_applies_a_flat_response() {
        let demo = DemoEqualizer::new(DemoStart::On);
        let presets = demo.snapshot().presets;
        // another preset goes without the output noticing
        demo.delete(&presets[0].id).unwrap();
        assert_eq!(applied(&demo), (5, -4.5));
        demo.delete(&presets[1].id).unwrap();
        let snapshot = demo.snapshot();
        assert_eq!((snapshot.presets.len(), snapshot.active, snapshot.enabled), (2, None, true));
        assert_eq!(applied(&demo), (0, 0.0));
    }

    #[test]
    fn presets_are_added_renamed_and_moved() {
        let demo = DemoEqualizer::new(DemoStart::On);
        let added = demo.add("  Sample: Pasted  ", "Preamp: -6 dB\nFilter 1: ON PK Fc 1000 Hz Gain 6 dB Q 1").unwrap();
        assert_eq!((added.name.as_str(), added.source.as_str(), added.filters.len()), ("Sample: Pasted", "pasted", 1));
        assert_eq!(names(&demo).last().map(String::as_str), Some("Sample: Pasted"));
        let error = demo.add("Sample: Broken", "Preamp: -6 dB\nbogus").unwrap_err();
        assert_eq!((error.line(), error.to_string().as_str()), (Some(2), "line 2: not a Preamp or Filter line: bogus"));

        demo.rename(&added.id, "Sample: Renamed").unwrap();
        demo.move_to(&added.id, 0).unwrap();
        assert_eq!(names(&demo)[0], "Sample: Renamed");
        demo.move_to(&added.id, 99).unwrap();
        assert_eq!(names(&demo)[4], "Sample: Renamed");
        assert_eq!(demo.rename(&added.id, "   "), Err(EqError::Other("the name is empty".to_string())));
        assert_eq!(demo.rename("no-such-preset", "Sample: Nobody"), Err(EqError::Other("no such preset".to_string())));
        // none of that was applied
        assert_eq!(applied(&demo), (5, -4.5));
    }

    const PEQ_FILE: &str = "Preamp: -6.1 dB\nFilter 1: ON PK Fc 105 Hz Gain -2.6 dB Q 0.70\n";

    /// A file in `folder` with these bytes.
    fn file(folder: &tempfile::TempDir, name: &str, bytes: &[u8]) -> gio::File {
        let path = folder.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        gio::File::for_path(path)
    }

    #[test]
    fn a_preset_is_named_after_its_file() {
        assert_eq!(preset_name("PEQdB-Headphone A.txt"), "PEQdB-Headphone A");
        assert_eq!(preset_name("Headphone B ParametricEQ.txt"), "Headphone B ParametricEQ");
        assert_eq!(preset_name("two.dots.txt"), "two.dots");
        assert_eq!(preset_name("no-extension"), "no-extension");
        assert_eq!(preset_name(".hidden"), ".hidden");
    }

    #[test]
    fn the_text_of_a_file_is_read_in_the_usual_encodings() {
        let wanted = "Preamp: -1 dB # é";
        assert_eq!(preset_text(wanted.as_bytes()).as_deref(), Some(wanted));
        let mark = [&[0xef, 0xbb, 0xbf], wanted.as_bytes()].concat();
        assert_eq!(preset_text(&mark).as_deref(), Some(wanted));
        let little: Vec<u8> =
            [0xfeff_u16].into_iter().chain(wanted.encode_utf16()).flat_map(u16::to_le_bytes).collect();
        assert_eq!(preset_text(&little).as_deref(), Some(wanted));
        let big: Vec<u8> = [0xfeff_u16].into_iter().chain(wanted.encode_utf16()).flat_map(u16::to_be_bytes).collect();
        assert_eq!(preset_text(&big).as_deref(), Some(wanted));
        assert_eq!(preset_text(b"Preamp: -1 dB # caf\xe9").as_deref(), Some("Preamp: -1 dB # café"), "Latin-1");
        assert_eq!(preset_text(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), None, "a picture");
    }

    #[test]
    fn files_become_presets_and_a_bad_one_keeps_no_other_out() {
        let folder = tempfile::tempdir().unwrap();
        let demo = DemoEqualizer::new(DemoStart::Empty);
        let files = [
            file(&folder, "Good One.txt", PEQ_FILE.as_bytes()),
            file(&folder, "Broken.txt", b"Preamp: -1 dB\nthis is no filter\n"),
            file(&folder, "Picture.png", b"\x89PNG\0\0"),
            file(&folder, "Empty.txt", b"# nothing here\n\n"),
            file(&folder, "Too Long.txt", &vec![b'#'; PRESET_FILE_MAX_BYTES + 1]),
            gio::File::for_path(folder.path().join("Missing.txt")),
            gio::File::for_path(folder.path()),
            file(&folder, "Second.cfg", PEQ_FILE.as_bytes()),
        ];
        let imported = import_files(&demo, &files);
        assert_eq!(imported.added, ["Good One", "Second"]);
        assert_eq!(names(&demo), ["Good One", "Second"]);
        assert!(demo.snapshot().presets.iter().all(|preset| preset.source == "file" && preset.filters.len() == 1));
        let failed: Vec<&str> = imported.failed.iter().map(|(name, _)| name.as_str()).collect();
        let folder_name = folder.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(failed, ["Broken.txt", "Picture.png", "Empty.txt", "Too Long.txt", "Missing.txt", folder_name]);
        assert_eq!(imported.failed[0].1, "line 2: not a Preamp or Filter line");
        assert_eq!(imported.failed[1].1, "it is not a text file");
        assert_eq!(imported.failed[2].1, "it has no Preamp or Filter lines");
        assert_eq!(imported.failed[3].1, "it is larger than 1 MiB");
        assert!(imported.failed[4].1.to_lowercase().contains("no such file"), "{}", imported.failed[4].1);
        assert_eq!(imported.failed[5].1, "it is a folder");
        // the same file again is a preset of its own
        let again = import_files(&demo, &files[..1]);
        assert_eq!(again, FileImport { added: vec!["Good One 2".to_string()], failed: Vec::new() });
    }

    #[test]
    fn the_reason_for_a_message_leaves_out_the_quoted_line() {
        let reason = |text: &str| parse_problem(&amui::eq::parse(text).unwrap_err());
        assert_eq!(reason("Preamp: 1 dB\nnonsense: with a colon"), "line 2: not a Preamp or Filter line");
        assert_eq!(reason("Filter 1: ON PK Fc 5 Hz Gain 1 dB Q 1"), "line 1: values out of range");
        assert_eq!(reason("Preamp: -50 dB"), "preamp out of range");
    }

    #[test]
    fn what_an_import_tells_names_the_files() {
        let told = |added: &[&str], failed: &[&str]| {
            FileImport {
                added: added.iter().map(|name| name.to_string()).collect(),
                failed: failed.iter().map(|name| (name.to_string(), "no good".to_string())).collect(),
            }
            .messages()
        };
        assert_eq!(told(&["A"], &[]), ["“A” added"]);
        assert_eq!(told(&["A", "B"], &[]), ["2 presets added"]);
        assert_eq!(told(&[], &["a.txt"]), ["“a.txt” was not imported: no good"]);
        assert_eq!(told(&["A"], &["b.txt"]), ["“A” added", "“b.txt” was not imported: no good"]);
        let many = told(&[], &["1", "2", "3", "4"]);
        assert_eq!(many.len(), 4);
        assert_eq!(many[3], "1 more file was not imported");
        assert_eq!(told(&[], &["1", "2", "3", "4", "5"])[3], "2 more files were not imported");
        assert!(told(&[], &[]).is_empty());
    }

    #[test]
    fn the_note_says_what_the_sink_runs() {
        let live =
            |bands: usize, gain_db: f64| Ok(LiveState { enabled_in_sink: bands > 0, bands, gain_db: Some(gain_db) });
        // as the equalizer set it
        assert_eq!(note(&live(15, -10.4), true, 15), "Sink: EQ on, 15 bands, -10.4 dB");
        assert_eq!(note(&live(1, -6.0), true, 1), "Sink: EQ on, 1 band, -6.0 dB");
        assert_eq!(note(&live(0, -0.00001), false, 0), "Sink: EQ off, 0 bands, +0.0 dB");
        // not as the equalizer set it
        assert_eq!(note(&live(15, -10.4), false, 0), "Sink: EQ on, 15 bands, -10.4 dB · set by another app");
        assert_eq!(note(&live(15, -10.4), true, 5), "Sink: EQ on, 15 bands, -10.4 dB · set by another app");
        assert_eq!(
            note(&live(0, -8.3), true, 5),
            "Sink: EQ off, 0 bands, -8.3 dB · flat now: another app changed it, toggle to apply again"
        );
        // the sink cannot be read
        let gone = Err(EqError::Sink("downmix sink is not running".to_string()));
        assert_eq!(note(&gone, true, 5), "Sink: downmix sink is not running");
        // through the service
        let demo = DemoEqualizer::new(DemoStart::On);
        assert_eq!(demo.live_note().as_deref(), Some("Sink: EQ on, 5 bands, -4.5 dB"));
        demo.set_enabled(false).unwrap();
        assert_eq!(demo.live_note().as_deref(), Some("Sink: EQ off, 0 bands, +0.0 dB"));
    }
}
