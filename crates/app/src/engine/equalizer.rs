//! The equalizer on the engine: am-ui's presets in the state folder and the equalizer stage of
//! the output (`amui::eq`).
//!
//! Of the application, only this can change the output's equalizer, and it does so only when the
//! user acts: making it reads and sets nothing, the state file is read when the dialog first
//! asks, and the output is set by `set_enabled`, by `activate` and by the `delete` of the active
//! preset of an equalizer that is on. The engine appends every one of these to its audit log.

use std::sync::Arc;

use amui::eq::Equalizer;

use crate::equalizer::{self, EqError, EqualizerService, Preset, Snapshot};

pub struct EngineEqualizer {
    equalizer: Arc<Equalizer>,
}

impl EngineEqualizer {
    /// On the engine's equalizer of the real output (`Equalizer::new`). It is shared with
    /// whatever else looks at the equalizer's state: there is one such state in the application.
    pub fn new(equalizer: Arc<Equalizer>) -> EngineEqualizer {
        EngineEqualizer { equalizer }
    }
}

impl EqualizerService for EngineEqualizer {
    fn snapshot(&self) -> Snapshot {
        // from the file: the dialog shows what is saved, also after a change that could not be
        self.equalizer.reload()
    }

    fn set_enabled(&self, on: bool) -> Result<(), EqError> {
        equalizer::switch(&self.equalizer, on)
    }

    fn activate(&self, id: &str) -> Result<(), EqError> {
        equalizer::activate(&self.equalizer, id)
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
        equalizer::delete(&self.equalizer, id)
    }

    fn live_note(&self) -> Option<String> {
        // the engine's own chain runs exactly what the equalizer says: nothing to report
        if self.equalizer.is_in_app() {
            return None;
        }
        equalizer::live_note(&self.equalizer)
    }
}
