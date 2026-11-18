//! Turning the rows of a list into new ones by changing only what differs.
//!
//! A list view keeps its place by the rows it knows: rows that stay are left alone, so that what
//! is on screen stays where it is, and a row that is taken out only lets the ones below it move
//! up. Replacing all the rows of a store would throw the view back to its beginning.

use std::collections::HashMap;
use std::ops::Range;

use amui::types::Track;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::data::Item;

/// More separate changes than this are made as one.
const MOST_CHANGES: usize = 200;

/// One step from the old list to the new one: at `at` of the old list, take out `remove` rows and
/// put in the new rows of `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub at: usize,
    pub remove: usize,
    pub insert: Range<usize>,
}

/// The changes that turn `old` into `new`, in the order of their places: applied from the last
/// to the first, each finds the old list as it was. `same` tells whether a row is unchanged, `key`
/// which row it is (the n-th row of a key in one list is the n-th of that key in the other).
pub fn plan<T>(old: &[T], new: &[T], same: impl Fn(&T, &T) -> bool, key: impl Fn(&T) -> String) -> Vec<Change> {
    let mut start = 0;
    while start < old.len() && start < new.len() && same(&old[start], &new[start]) {
        start += 1;
    }
    let (mut old_end, mut new_end) = (old.len(), new.len());
    while old_end > start && new_end > start && same(&old[old_end - 1], &new[new_end - 1]) {
        old_end -= 1;
        new_end -= 1;
    }
    if start == old_end && start == new_end {
        return Vec::new();
    }
    // between what is the same at both ends: the rows found in both lists...
    let mut places: HashMap<(String, u32), usize> = HashMap::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    for (index, row) in old.iter().enumerate().take(old_end).skip(start) {
        let key = key(row);
        let nth = seen.entry(key.clone()).or_default();
        places.insert((key, *nth), index);
        *nth += 1;
    }
    seen.clear();
    let mut found: Vec<(usize, usize)> = Vec::new();
    for (index, row) in new.iter().enumerate().take(new_end).skip(start) {
        let key = key(row);
        let nth = seen.entry(key.clone()).or_default();
        if let Some(place) = places.get(&(key, *nth)) {
            found.push((*place, index));
        }
        *nth += 1;
    }
    // ...and of those, as many as can stay where they are in relation to each other
    let mut changes: Vec<Change> = Vec::new();
    let mut add = |at: usize, remove: usize, insert: Range<usize>| {
        if remove == 0 && insert.is_empty() {
            return;
        }
        match changes.last_mut() {
            Some(last) if last.at + last.remove == at && last.insert.end == insert.start => {
                last.remove += remove;
                last.insert.end = insert.end;
            }
            _ => changes.push(Change { at, remove, insert }),
        }
    };
    let (mut old_at, mut new_at) = (start, start);
    for (old_index, new_index) in longest_rising(&found) {
        add(old_at, old_index - old_at, new_at..new_index);
        if !same(&old[old_index], &new[new_index]) {
            add(old_index, 1, new_index..new_index + 1);
        }
        (old_at, new_at) = (old_index + 1, new_index + 1);
    }
    add(old_at, old_end - old_at, new_at..new_end);
    if changes.len() > MOST_CHANGES {
        return vec![Change { at: start, remove: old_end - start, insert: start..new_end }];
    }
    changes
}

/// The longest part of `pairs` (old place, new place), in the order of the new places, whose old
/// places rise as well.
fn longest_rising(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    // tails[n]: the index in `pairs` of the pair that ends the best run of n + 1 pairs
    let mut tails: Vec<usize> = Vec::new();
    let mut before: Vec<Option<usize>> = vec![None; pairs.len()];
    for (index, (old, _)) in pairs.iter().enumerate() {
        let length = tails.partition_point(|tail| pairs[*tail].0 < *old);
        before[index] = length.checked_sub(1).map(|shorter| tails[shorter]);
        if length == tails.len() {
            tails.push(index);
        } else {
            tails[length] = index;
        }
    }
    let mut run = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(index) = at {
        run.push(pairs[index]);
        at = before[index];
    }
    run.reverse();
    run
}

/// Which row of a list of the library or a playlist this is.
fn row_key(row: &glib::Object) -> String {
    let Some(boxed) = row.downcast_ref::<glib::BoxedAnyObject>() else { return String::new() };
    if let Ok(track) = boxed.try_borrow::<Track>() {
        return track.library_id.clone().unwrap_or_else(|| track.key());
    }
    boxed.try_borrow::<Item>().map(|item| format!("{}:{}", item.kind(), item.key())).unwrap_or_default()
}

/// Whether two rows show the same.
fn same_row(old: &glib::Object, new: &glib::Object) -> bool {
    let (Some(old), Some(new)) =
        (old.downcast_ref::<glib::BoxedAnyObject>(), new.downcast_ref::<glib::BoxedAnyObject>())
    else {
        return false;
    };
    if let (Ok(old), Ok(new)) = (old.try_borrow::<Track>(), new.try_borrow::<Track>()) {
        return *old == *new;
    }
    match (old.try_borrow::<Item>(), new.try_borrow::<Item>()) {
        (Ok(old), Ok(new)) => *old == *new,
        _ => false,
    }
}

/// Make `store` hold `rows`, keeping the rows it has that stay. True when anything changed.
pub fn update_store(store: &gio::ListStore, rows: &[glib::BoxedAnyObject]) -> bool {
    let old: Vec<glib::Object> = (0..store.n_items()).filter_map(|position| store.item(position)).collect();
    let new: Vec<glib::Object> = rows.iter().map(|row| row.clone().upcast()).collect();
    let changes = plan(&old, &new, same_row, row_key);
    for change in changes.iter().rev() {
        store.splice(change.at as u32, change.remove as u32, &rows[change.insert.clone()]);
    }
    !changes.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::boxed;

    /// Rows are (key, content): rows with the same key are the same row, equal if their content is.
    type Row = (&'static str, u32);

    fn changes(old: &[Row], new: &[Row]) -> Vec<Change> {
        plan(old, new, |a, b| a == b, |row| row.0.to_string())
    }

    /// Apply the changes as `update_store` does and check that they lead to `new`.
    fn check(old: &[Row], new: &[Row]) -> Vec<Change> {
        let plan = changes(old, new);
        let mut list = old.to_vec();
        for change in plan.iter().rev() {
            list.splice(change.at..change.at + change.remove, new[change.insert.clone()].iter().copied());
        }
        assert_eq!(list, new, "from {old:?} by {plan:?}");
        plan
    }

    fn rows(keys: &[&'static str]) -> Vec<Row> {
        keys.iter().map(|key| (*key, 0)).collect()
    }

    #[test]
    fn what_stays_is_not_touched() {
        let list = rows(&["a", "b", "c", "d", "e"]);
        assert!(check(&list, &list).is_empty());
        // a row taken out
        assert_eq!(check(&list, &rows(&["a", "b", "d", "e"])), [Change { at: 2, remove: 1, insert: 2..2 }]);
        // a row added at the beginning, at the end, in the middle
        assert_eq!(check(&list, &rows(&["n", "a", "b", "c", "d", "e"])), [Change { at: 0, remove: 0, insert: 0..1 }]);
        assert_eq!(check(&list, &rows(&["a", "b", "c", "d", "e", "n"])), [Change { at: 5, remove: 0, insert: 5..6 }]);
        assert_eq!(check(&list, &rows(&["a", "b", "n", "c", "d", "e"])), [Change { at: 2, remove: 0, insert: 2..3 }]);
        // a row that changed is replaced where it is
        let mut changed = list.clone();
        changed[3].1 = 1;
        assert_eq!(check(&list, &changed), [Change { at: 3, remove: 1, insert: 3..4 }]);
    }

    #[test]
    fn separate_changes_stay_separate() {
        let list = rows(&["a", "b", "c", "d", "e", "f", "g"]);
        // one row taken out at either end of the list: not everything between them replaced
        let plan = check(&list, &rows(&["b", "c", "d", "e", "f"]));
        assert_eq!(plan, [Change { at: 0, remove: 1, insert: 0..0 }, Change { at: 6, remove: 1, insert: 5..5 }]);
        // a row moved: the others stay
        let plan = check(&list, &rows(&["a", "c", "d", "e", "b", "f", "g"]));
        assert_eq!(plan.iter().map(|change| change.remove).sum::<usize>(), 1);
        assert_eq!(plan.iter().map(|change| change.insert.len()).sum::<usize>(), 1);
    }

    #[test]
    fn lists_with_rows_of_the_same_key_are_handled() {
        // a song that is in a playlist twice
        check(&rows(&["a", "b", "a", "c"]), &rows(&["a", "c"]));
        check(&rows(&["a", "b", "a", "c"]), &rows(&["b", "a", "c", "a"]));
        check(&rows(&["x", "x", "x"]), &rows(&["x"]));
        check(&rows(&["x"]), &rows(&["x", "x", "x"]));
    }

    #[test]
    fn any_list_can_become_any_other() {
        // lists of a few rows taken from eight, in every kind of order
        let mut state = 7u32;
        let mut next = |limit: u32| {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            (state >> 16) % limit
        };
        const KEYS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        for _ in 0..500 {
            let mut list =
                |length: u32| -> Vec<Row> { (0..length).map(|_| (KEYS[next(8) as usize], next(2))).collect() };
            let (old, new) = (list(9), list(7));
            check(&old, &new);
        }
        // so many separate changes are made as one
        let old: Vec<Row> = (0..1000).map(|n| ("x", n)).collect();
        let new: Vec<Row> = (0..1000).map(|n| ("x", if n % 2 == 0 { n } else { n + 5000 })).collect();
        assert_eq!(check(&old, &new).len(), 1);
    }

    #[test]
    fn a_store_keeps_the_rows_that_stay() {
        let song =
            |id: &str, name: &str| Track { library_id: Some(id.to_string()), ..Track::new(None, name, "Artist A") };
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for track in [song("i.1", "Song A"), song("i.2", "Song B"), song("i.3", "Song C")] {
            store.append(&boxed(track));
        }
        let first = store.item(0).unwrap();
        let third = store.item(2).unwrap();
        let notified = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = notified.clone();
        store.connect_items_changed(move |_, at, removed, added| seen.borrow_mut().push((at, removed, added)));
        // the second song goes and another one comes at the end; the first one has a new name
        let new = [song("i.1", "Song A2"), song("i.3", "Song C"), song("i.4", "Song D")].map(boxed);
        assert!(update_store(&store, &new));
        let keys: Vec<String> = (0..3).map(|at| row_key(&store.item(at).unwrap())).collect();
        assert_eq!(keys, ["i.1", "i.3", "i.4"]);
        assert_eq!(*notified.borrow(), [(3, 0, 1), (0, 2, 1)]);
        assert_ne!(store.item(0).unwrap(), first, "a row that changed is replaced");
        assert_eq!(store.item(1).unwrap(), third, "a row that stays is the same object");
        assert!(!update_store(
            &store,
            &[song("i.1", "Song A2"), song("i.3", "Song C"), song("i.4", "Song D")].map(boxed)
        ));
        assert!(update_store(&store, &[]));
        assert_eq!(store.n_items(), 0);
    }
}
