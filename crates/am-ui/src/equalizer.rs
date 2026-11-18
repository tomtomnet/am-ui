//! The equalizer as the window sees it: the presets, which of them is active and whether it is
//! on. The engine's `eq` serves it in the real application (`engine::equalizer`), an equalizer
//! in memory in the demonstration.
//!
//! Every method blocks (the state file, the output's programs) and is called on worker threads
//! (`signal::background`).

use std::sync::{Arc, Mutex};

use amui::downmix::{self, Controls};
pub use amui::eq::{EqError, Preset, Snapshot};
use amui::eq::{Equalizer, LiveState};

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
