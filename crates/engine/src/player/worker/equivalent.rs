//! A library song whose catalog version was withdrawn: Apple refuses its catalog id to the
//! account (`WebError::NotEntitled`, a 403 that is no sign-in problem). The worker asks the
//! radio for the catalog's current version of the song and acquires that in place of the
//! entry's own id; the entry keeps its own track and key, and the snapshot names the catalog id
//! that plays. What is found is remembered (`Equivalents`: in memory and in the state folder),
//! so that the next play does not fail first. Without a current version, or without a radio,
//! the song is reported as no longer available and passed over like any song that cannot be
//! played.
//!
//! A load that was refused waits (the state stays "loading") for the lookup; a prefetch that
//! was refused starts the lookup too, so the song is ready by its turn. When the current version
//! is refused as well, it is forgotten and the song counts as unavailable for the rest of the
//! session.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::Worker;
use crate::api::ApiError;
use crate::player::jobs::Job;
use crate::player::radio::{Answer, Ask, RadioJob};
use crate::render::os_error_name;
use crate::types::{PlayerState, Track};

/// The lookups of current versions, and the songs that have none.
#[derive(Default)]
pub(super) struct Withdrawn {
    /// Lookups in flight, by the song's own catalog id.
    lookups: HashMap<String, Lookup>,
    /// Songs refused to the account that have no current version (or whose current version is
    /// refused as well), this session.
    unavailable: HashSet<String>,
}

struct Lookup {
    job: Arc<RadioJob>,
    /// The queue entry whose load waits for the answer.
    load: Option<u64>,
}

impl Worker {
    /// The song a load asked for is refused to the account: look for its current version (the
    /// load waits for it), or report the song and move on.
    pub(super) fn load_refused(&mut self, job: &Arc<Job>) {
        let track = &job.track;
        if let Some(own) = track.catalog_id.clone() {
            if job.playing_id.as_deref() != Some(own.as_str()) {
                self.current_version_refused(&own);
            } else if self.shared.equivalents.get(&own).is_some() {
                // found meanwhile, for the song's prefetch: the load starts again with it
                let start_frame = self.st().pending_start_frame;
                self.start_current(start_frame, None, 0);
                return;
            } else if self.shared.radio.is_some() && !self.withdrawn.unavailable.contains(&own) {
                let entry = self.st().entry_at_index().map(|entry| entry.id);
                if self.look_up(track, entry) {
                    return;
                }
            }
        }
        self.no_longer_available(track);
    }

    /// The song a prefetch asked for is refused: its current version is looked for now, so
    /// that it is there by the song's turn. (Anything else is said when the song plays.)
    pub(super) fn prefetch_refused(&mut self, job: &Arc<Job>) {
        let Some(own) = job.track.catalog_id.clone() else { return };
        if job.playing_id.as_deref() != Some(own.as_str()) {
            self.current_version_refused(&own);
        } else if self.shared.equivalents.get(&own).is_some() {
            self.arm_prefetch(); // found meanwhile: the prefetch takes it
        } else if self.shared.radio.is_some() && !self.withdrawn.unavailable.contains(&own) {
            self.look_up(&job.track, None);
        }
    }

    /// A song known to have no version this account may have.
    pub(super) fn known_unavailable(&mut self, track: &Track) -> bool {
        track.catalog_id.as_ref().is_some_and(|own| self.withdrawn.unavailable.contains(own))
    }

    /// Ask for the current version of `track` (once at a time); `load`: the entry whose load
    /// waits for it. False when nothing could be asked.
    fn look_up(&mut self, track: &Track, load: Option<u64>) -> bool {
        let Some(own) = track.catalog_id.clone() else { return false };
        if let Some(lookup) = self.withdrawn.lookups.get_mut(&own) {
            // asked already (by the song's prefetch): the load waits for that answer
            if load.is_some() {
                lookup.load = load;
            }
            return true;
        }
        let Some(job) = self.ask(Ask::Equivalent(Box::new(track.clone()))) else { return false };
        self.log(&format!("equivalent {own}: refused to this account; looking for the catalog's current version"));
        self.withdrawn.lookups.insert(own, Lookup { job, load });
        true
    }

    /// The current version that played in place of the song `own` is refused too.
    fn current_version_refused(&mut self, own: &str) {
        self.log(&format!("equivalent {own}: the current version is refused as well"));
        self.withdrawn.unavailable.insert(own.to_string());
        if let Err(error) = self.shared.equivalents.forget(own) {
            self.log(&format!("equivalent {own}: could not be saved ({})", os_error_name(&error)));
        }
    }

    /// Report the song and move on, as for any song that cannot be played.
    fn no_longer_available(&mut self, track: &Track) {
        let message = format!("{} is no longer available on Apple Music", track.name);
        self.emit_error(message.clone(), track.catalog_id.clone(), false);
        {
            let mut st = self.st();
            st.state = PlayerState::Error;
            st.error = Some(message);
        }
        self.emit_state();
        self.skip_after_error();
    }

    pub(super) fn equivalent_answered(
        &mut self,
        job: &Arc<RadioJob>,
        track: &Track,
        outcome: Result<Answer, ApiError>,
    ) {
        let Some(own) = track.catalog_id.clone() else { return };
        let Some(lookup) = self.withdrawn.lookups.remove(&own) else { return };
        if !Arc::ptr_eq(&lookup.job, job) {
            self.withdrawn.lookups.insert(own, lookup);
            return;
        }
        let found = match outcome {
            Ok(Answer::Equivalent(song)) => {
                let found = song.and_then(|song| song.catalog_id).filter(|id| *id != own);
                if found.is_none() {
                    self.log(&format!("equivalent {own}: the catalog has no current version"));
                    self.withdrawn.unavailable.insert(own.clone());
                }
                found
            }
            Ok(_) => None,
            Err(error) => {
                // (not remembered: the next play asks again)
                self.log(&format!("equivalent {own} failed: {error}"));
                None
            }
        };
        if let Some(id) = &found {
            self.log(&format!("equivalent {own}: {id}"));
            if let Err(error) = self.shared.equivalents.remember(&own, id) {
                self.log(&format!("equivalent {own}: could not be saved ({})", os_error_name(&error)));
            }
        }
        // the load that waited, unless the player has moved on meanwhile
        let waiting = lookup.load.is_some_and(|entry| {
            let st = self.st();
            let here = st.entry_at_index().is_some_and(|current| current.id == entry);
            here && st.state == PlayerState::Loading && st.load_job.is_none() && st.current.is_none()
        });
        match (waiting, found) {
            (true, Some(_)) => {
                let start_frame = self.st().pending_start_frame;
                self.start_current(start_frame, None, 0);
            }
            (true, None) => self.no_longer_available(track),
            // a prefetch of the song takes the current version now
            (false, Some(_)) => self.arm_prefetch(),
            (false, None) => {}
        }
    }
}
