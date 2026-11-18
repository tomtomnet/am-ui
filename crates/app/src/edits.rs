//! What was changed in the library here and the service may not show yet.
//!
//! Apple accepts a change of the library at once and applies it some time later, seconds or a few
//! minutes: until then its lists do not have the playlist that was just created, still have the
//! one that was just deleted (for a moment without a name), still show the old name and the old
//! songs. Asking again right after a change is therefore no way to see it. What was changed is
//! kept here for a while and laid over every answer: an answer that is behind changes nothing, and
//! one that has caught up agrees anyway.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use amui::types::{Playlist, Track};

use crate::data::{Collection, Item};
use crate::library::same_song;

/// How long a change counts over what the service says.
pub const HOLD: Duration = Duration::from_secs(10 * 60);

/// Something that counts until `until`.
struct Held<T> {
    value: T,
    until: Instant,
}

/// A song added to a playlist.
struct Added {
    track: Track,
    /// How many copies of it the playlist had before: a list with no more than that does not have
    /// this one yet.
    before: usize,
}

/// What was changed in the songs of one playlist.
#[derive(Default)]
struct Songs {
    /// The songs added, in that order.
    added: Vec<Held<Added>>,
    /// The entries removed (`entry`).
    removed: Vec<Held<String>>,
}

#[derive(Default)]
pub struct Edits {
    /// The playlists created here, the newest last.
    created: Vec<Held<Playlist>>,
    /// The new names and descriptions, by library id.
    renamed: HashMap<String, Held<(String, String)>>,
    /// The library ids of the playlists deleted here.
    deleted: HashMap<String, Instant>,
    songs: HashMap<String, Songs>,
}

/// Which entry of a playlist a song is.
fn entry(track: &Track) -> String {
    track.library_id.clone().unwrap_or_else(|| track.key())
}

impl Edits {
    /// A playlist was created (with `songs` in it).
    pub fn created(&mut self, playlist: &Playlist, songs: &[Track], now: Instant) {
        let until = now + HOLD;
        let Some(id) = playlist.library_id.clone() else { return };
        self.created.retain(|held| held.value.library_id.as_ref() != Some(&id));
        self.created.push(Held { value: playlist.clone(), until });
        self.deleted.remove(&id);
        self.added(&id, songs, &[], now);
    }

    /// A playlist got a new name and description.
    pub fn renamed(&mut self, id: &str, name: &str, description: &str, now: Instant) {
        let until = now + HOLD;
        self.renamed.insert(id.to_string(), Held { value: (name.to_string(), description.to_string()), until });
        for held in &mut self.created {
            if held.value.library_id.as_deref() == Some(id) {
                held.value.name = name.to_string();
                held.value.description = description.to_string();
            }
        }
    }

    /// A playlist was deleted.
    pub fn deleted(&mut self, id: &str, now: Instant) {
        self.created.retain(|held| held.value.library_id.as_deref() != Some(id));
        self.renamed.remove(id);
        self.songs.remove(id);
        self.deleted.insert(id.to_string(), now + HOLD);
    }

    /// Songs were added to a playlist that had the songs `known`, as far as that is known (a song
    /// can be in a playlist several times).
    pub fn added(&mut self, id: &str, tracks: &[Track], known: &[Track], now: Instant) {
        if tracks.is_empty() {
            return;
        }
        let until = now + HOLD;
        let songs = self.songs.entry(id.to_string()).or_default();
        for track in tracks {
            let is_it = |other: &Track| same_song(other, track).is_some();
            let was_removed = |other: &Track| songs.removed.iter().any(|held| held.value == entry(other));
            let before = known.iter().filter(|other| is_it(other) && !was_removed(other)).count()
                + songs.added.iter().filter(|held| is_it(&held.value.track)).count();
            songs.removed.retain(|held| held.value != entry(track));
            songs.added.push(Held { value: Added { track: track.clone(), before }, until });
        }
    }

    /// A song was removed from a playlist (every copy of it: that is what the service does).
    pub fn removed(&mut self, id: &str, track: &Track, now: Instant) {
        let songs = self.songs.entry(id.to_string()).or_default();
        let gone = entry(track);
        songs.added.retain(|held| entry(&held.value.track) != gone && same_song(&held.value.track, track).is_none());
        songs.removed.retain(|held| held.value != gone);
        songs.removed.push(Held { value: gone, until: now + HOLD });
    }

    /// Forget what no longer counts.
    fn expire(&mut self, now: Instant) {
        self.created.retain(|held| held.until > now);
        self.renamed.retain(|_, held| held.until > now);
        self.deleted.retain(|_, until| *until > now);
        for songs in self.songs.values_mut() {
            songs.added.retain(|held| held.until > now);
            songs.removed.retain(|held| held.until > now);
        }
        self.songs.retain(|_, songs| !songs.added.is_empty() || !songs.removed.is_empty());
    }

    /// `item` as it is after what was changed; `None` when it is gone.
    fn overlaid(&self, item: Item) -> Option<Item> {
        let Item::Playlist(mut playlist) = item else { return Some(item) };
        let Some(id) = playlist.library_id.clone() else { return Some(Item::Playlist(playlist)) };
        if self.deleted.contains_key(&id) {
            return None;
        }
        if let Some(held) = self.renamed.get(&id) {
            (playlist.name, playlist.description) = held.value.clone();
        }
        if playlist.name.trim().is_empty() {
            // the service lists a playlist that is on its way out without its attributes: nothing
            // to show, unless it is one that was created here
            let created = self.created.iter().find(|held| held.value.library_id.as_ref() == Some(&id))?;
            return Some(Item::Playlist(created.value.clone()));
        }
        Some(Item::Playlist(playlist))
    }

    /// A list of the library that holds playlists, as the service gave it, after what was changed:
    /// what was deleted is left out, new names are laid over the old ones and the playlists
    /// created here that it does not have yet stand first, the newest first (after Favorite Songs
    /// where that leads the list), where the service will put them.
    pub fn items(&mut self, listed: Vec<Item>, now: Instant) -> Vec<Item> {
        self.expire(now);
        let mut items: Vec<Item> = listed.into_iter().filter_map(|item| self.overlaid(item)).collect();
        let listed = |id: &Option<String>| {
            items.iter().any(|item| matches!(item, Item::Playlist(playlist) if playlist.library_id == *id))
        };
        let missing: Vec<Item> = self
            .created
            .iter()
            .rev()
            .filter(|held| !listed(&held.value.library_id))
            .map(|held| Item::Playlist(held.value.clone()))
            .collect();
        let first = usize::from(matches!(items.first(), Some(Item::Playlist(playlist)) if playlist.is_favorites));
        items.splice(first..first, missing);
        items
    }

    /// The songs of a playlist as the service gave them, after what was changed: what was removed
    /// is left out, what was added and it does not have yet comes at the end.
    fn songs(&self, id: &str, listed: Vec<Track>) -> Vec<Track> {
        let Some(songs) = self.songs.get(id) else { return listed };
        let mut tracks: Vec<Track> = listed
            .into_iter()
            .filter(|track| {
                let gone = entry(track);
                !songs.removed.iter().any(|held| held.value == gone)
            })
            .collect();
        for held in &songs.added {
            let added = &held.value;
            let copies = tracks.iter().filter(|track| same_song(track, &added.track).is_some()).count();
            if copies <= added.before {
                tracks.push(added.track.clone());
            }
        }
        tracks
    }

    /// An album or a playlist as the service gave it, after what was changed.
    pub fn collection(&mut self, answer: &Collection, now: Instant) -> Collection {
        self.expire(now);
        let mut collection = answer.clone();
        if let Item::Playlist(playlist) = &mut collection.item
            && let Some(id) = playlist.library_id.clone()
        {
            if let Some(held) = self.renamed.get(&id) {
                (playlist.name, playlist.description) = held.value.clone();
            }
            if playlist.name.trim().is_empty()
                && let Some(held) = self.created.iter().find(|held| held.value.library_id.as_ref() == Some(&id))
            {
                *playlist = held.value.clone();
            }
            collection.tracks = self.songs(&id, std::mem::take(&mut collection.tracks));
        }
        collection
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amui::types::Album;

    fn playlist(id: &str, name: &str) -> Playlist {
        Playlist { can_edit: true, ..Playlist::new(Some(id), name) }
    }

    fn listed(playlists: &[Playlist]) -> Vec<Item> {
        playlists.iter().cloned().map(Item::Playlist).collect()
    }

    fn names(items: &[Item]) -> Vec<&str> {
        items.iter().map(Item::title).collect()
    }

    fn song(catalog_id: &str, library_id: Option<&str>, name: &str) -> Track {
        Track { library_id: library_id.map(str::to_string), ..Track::new(Some(catalog_id), name, "Artist A") }
    }

    fn with_songs(id: &str, tracks: Vec<Track>) -> Collection {
        Collection { item: Item::Playlist(playlist(id, "Mix")), tracks, next_offset: None, total: None }
    }

    fn titles(collection: &Collection) -> Vec<&str> {
        collection.tracks.iter().map(|track| track.name.as_str()).collect()
    }

    #[test]
    fn a_created_playlist_stays_until_the_service_lists_it() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let favorites = Playlist { is_favorites: true, ..playlist("p.f", "Favorite Songs") };
        let list = [favorites, playlist("p.1", "Old")];
        edits.created(&playlist("p.2", "New"), &[], now);
        edits.created(&playlist("p.3", "Newer"), &[], now);
        // an answer from before: the new ones first, the newest first, after Favorite Songs
        let answer = edits.items(listed(&list), now);
        assert_eq!(names(&answer), ["Favorite Songs", "Newer", "New", "Old"]);
        // without Favorite Songs at the top they lead the list
        assert_eq!(names(&edits.items(listed(&list[1..]), now)), ["Newer", "New", "Old"]);
        assert_eq!(names(&edits.items(Vec::new(), now)), ["Newer", "New"]);
        // what is listed comes from the service, once
        let mut known = playlist("p.2", "New");
        known.artwork = Some(amui::types::Artwork::new("file:///cover.jpg"));
        let answer = edits.items(listed(&[known.clone(), list[1].clone()]), now);
        assert_eq!(names(&answer), ["Newer", "New", "Old"]);
        assert_eq!(answer[1], Item::Playlist(known));
        // after a while what the service says counts
        assert_eq!(names(&edits.items(listed(&list), now + HOLD)), ["Favorite Songs", "Old"]);
    }

    #[test]
    fn a_stale_answer_does_not_bring_a_deleted_playlist_back() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let list = [playlist("p.1", "Keep"), playlist("p.2", "Gone")];
        edits.deleted("p.2", now);
        assert_eq!(names(&edits.items(listed(&list), now)), ["Keep"]);
        // listed without its attributes, as the service does for a moment
        let ghost = [list[0].clone(), playlist("p.2", "")];
        assert_eq!(names(&edits.items(listed(&ghost), now + Duration::from_secs(1))), ["Keep"]);
        // once the time is over the list decides again
        assert_eq!(names(&edits.items(listed(&list), now + HOLD)), ["Keep", "Gone"]);
        // a playlist without a name is nothing to show, deleted here or not
        let (mut edits, now) = (Edits::default(), Instant::now());
        assert_eq!(names(&edits.items(listed(&[playlist("p.9", " "), list[0].clone()]), now)), ["Keep"]);
        // creating and deleting a playlist leaves nothing
        edits.created(&playlist("p.5", "Brief"), &[], now);
        edits.deleted("p.5", now);
        assert_eq!(names(&edits.items(listed(&[playlist("p.5", "Brief")]), now)), Vec::<&str>::new());
    }

    #[test]
    fn a_new_name_counts_over_the_old_one() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let list = [Playlist { description: "Old words".into(), ..playlist("p.1", "Old") }, playlist("p.2", "Other")];
        edits.renamed("p.1", "New", "New words", now);
        let answer = edits.items(listed(&list), now);
        assert_eq!(names(&answer), ["New", "Other"]);
        assert!(matches!(&answer[0], Item::Playlist(p) if p.description == "New words"));
        // the page of the playlist says so too
        let page = edits.collection(&with_songs("p.1", vec![song("1", Some("i.1"), "Song A")]), now);
        assert_eq!(page.item.title(), "New");
        // a playlist created here takes its new name along
        edits.created(&playlist("p.3", "First"), &[], now);
        edits.renamed("p.3", "Second", "", now);
        assert_eq!(names(&edits.items(Vec::new(), now)), ["Second"]);
        // after a while the service's name counts
        assert_eq!(names(&edits.items(listed(&list), now + HOLD)), ["Old", "Other"]);
    }

    #[test]
    fn other_rows_of_a_list_pass_through() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        edits.deleted("p.1", now);
        let album = Item::Album(Album::new(Some("1"), "Album A", "Artist A"));
        let catalog = Item::Playlist(Playlist { catalog_id: Some("pl.1".into()), ..Playlist::new(None, "Catalog") });
        let list = vec![album.clone(), Item::Playlist(playlist("p.1", "Gone")), catalog.clone()];
        assert_eq!(edits.items(list, now), [album, catalog]);
    }

    #[test]
    fn the_songs_of_a_playlist_follow_what_was_done_to_them() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let answer = with_songs("p.1", vec![song("1", Some("i.1"), "Song A"), song("2", Some("i.2"), "Song B")]);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song A", "Song B"]);

        // a removed song stays away while the service still lists it
        edits.removed("p.1", &song("1", Some("i.1"), "Song A"), now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song B"]);
        // a song added from elsewhere (a catalog song) waits at the end until it is listed
        edits.added("p.1", &[song("3", None, "Song C")], &answer.tracks, now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song B", "Song C"]);
        let caught_up = with_songs("p.1", vec![song("2", Some("i.2"), "Song B"), song("3", Some("i.3"), "Song C")]);
        let shown = edits.collection(&caught_up, now);
        assert_eq!(titles(&shown), ["Song B", "Song C"]);
        assert_eq!(shown.tracks[1].library_id.as_deref(), Some("i.3"));
        // removing it again takes the waiting copy too
        edits.removed("p.1", &song("3", Some("i.3"), "Song C"), now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song B"]);
        // adding the removed one again brings it back
        edits.added("p.1", &[song("1", Some("i.1"), "Song A")], &answer.tracks, now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song A", "Song B"]);
        // other playlists are not concerned
        assert_eq!(titles(&edits.collection(&with_songs("p.2", answer.tracks.clone()), now)), ["Song A", "Song B"]);
        // after a while the service's list counts
        let later = now + HOLD;
        assert_eq!(titles(&edits.collection(&answer, later)), ["Song A", "Song B"]);
        assert!(edits.songs.is_empty());
    }

    #[test]
    fn a_song_can_be_added_to_a_playlist_that_has_it() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let answer = with_songs("p.1", vec![song("1", Some("i.1"), "Song A"), song("2", Some("i.2"), "Song B")]);
        // a second copy of what the playlist has, twice: both wait at the end
        edits.added("p.1", &[song("1", Some("i.1"), "Song A")], &answer.tracks, now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song A", "Song B", "Song A"]);
        edits.added("p.1", &[song("1", Some("i.1"), "Song A")], &answer.tracks, now);
        assert_eq!(titles(&edits.collection(&answer, now)), ["Song A", "Song B", "Song A", "Song A"]);
        // the service has one of them by now, then both: nothing is shown twice
        let mut tracks = answer.tracks.clone();
        tracks.push(song("1", Some("i.1"), "Song A"));
        let one_more = with_songs("p.1", tracks.clone());
        assert_eq!(titles(&edits.collection(&one_more, now)), ["Song A", "Song B", "Song A", "Song A"]);
        tracks.push(song("1", Some("i.1"), "Song A"));
        let caught_up = with_songs("p.1", tracks);
        assert_eq!(titles(&edits.collection(&caught_up, now)), ["Song A", "Song B", "Song A", "Song A"]);
        // removing the song takes every copy
        edits.removed("p.1", &song("1", Some("i.1"), "Song A"), now);
        assert_eq!(titles(&edits.collection(&caught_up, now)), ["Song B"]);
    }

    #[test]
    fn a_created_playlist_has_its_songs_at_once() {
        let (mut edits, now) = (Edits::default(), Instant::now());
        let created = playlist("p.7", "Made");
        edits.created(&created, &[song("1", None, "Song A")], now);
        // the service does not know it yet: an empty answer, without even a name
        let unknown = Collection { item: Item::Playlist(playlist("p.7", "")), ..with_songs("p.7", Vec::new()) };
        let shown = edits.collection(&unknown, now);
        assert_eq!(shown.item.title(), "Made");
        assert_eq!(titles(&shown), ["Song A"]);
    }
}
