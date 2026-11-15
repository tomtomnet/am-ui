//! The lyrics of the current song: large lines of text that follow the song.
//!
//! One widget draws every line as a text layout. With timing, the line that is being sung is
//! white and the others are dimmed, and the view keeps that line about a third from the top; with
//! word timing the words of the line light up as they are sung, and each word grows a little and
//! rises above the line while it is sung, then settles back (a held word stays up a moment
//! longer). Without timing the text is evenly bright and simply scrolls. A scrolled window
//! scrolls the widget like any other content, so `scroll::SmoothScroll` moves it in step with the
//! display; when the user scrolls, the view stops following the song for a few seconds. A click
//! on a line seeks to its beginning.
//!
//! A word grows and rises by a transform when it is drawn, about a point on its baseline: the
//! layout stays as it is, so the line wraps the same way while the word grows. Its room is half
//! of the space to each neighbour (the rest of the row at its ends), and it grows into a part of
//! that room only, from its middle when the room allows: it never covers the words beside it,
//! which do not move, and two words grown side by side keep a space between them. The line is
//! then drawn in parts: the room of each growing word transformed, what lies between them as it
//! is. Syllables timed one by one grow together as their word. The scale moves in small steps
//! (`STEP`): the renderer draws the glyphs of every size it is given once and keeps them, so a
//! size of its own for every frame would mean new glyphs for every frame. When the desktop asks
//! for no animations or for less motion, the words only light up.
//!
//! Nothing runs while nothing changes. A tick callback runs only while the view glides to a line,
//! a line lights up or dims, a word is being filled or a word in view grows or settles; between
//! the lines one timer waits for the moment the next one begins; with playback paused or the view
//! hidden there is neither.

use std::cell::{Cell, OnceCell, RefCell};
use std::time::{Duration, Instant};

use amui::types::{self, Line, Timing};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, pango};

use crate::app::ctx;
use crate::scroll::{SmoothScroll, Tuning};
use crate::widgets::{activity, text_layout, with_alpha};

/// The smallest size of the text in pixels; it grows with the view.
const SMALLEST: f64 = 34.0;
/// Where the line that is being sung sits: this far down the view.
const ANCHOR: f32 = 0.35;
/// Lines of background vocals are this much smaller.
const BACKGROUND: f64 = 0.62;
/// How bright a line is that is not being sung, and a word that is yet to be sung.
const DIM: f32 = 0.36;
/// How bright the line under the pointer is at least: a click on it seeks.
const POINTED: f32 = 0.62;
/// Microseconds the view takes to glide to another line, and a line to light up or to dim.
const GLIDE: f64 = 500_000.0;
/// The view follows the song again this long after the user last scrolled.
const RESUME: Duration = Duration::from_secs(4);
/// A timer may fire a moment before its time: what is due this soon counts as due.
const SOON: Duration = Duration::from_millis(5);
/// The light that fills a word ends softly: over this much of the text's size, in this many
/// slices that are dimmer one after the other.
const SOFT: f32 = 0.45;
const SLICES: usize = 5;
/// A word that is sung grows to this much of its size at most (less where its room is narrow),
/// and rises by this much of the height of its row.
const ZOOM: f32 = 1.1;
const LIFT: f32 = 0.05;
/// How much of the room beside it a word may grow into: half, so that two words that have grown
/// side by side keep at least half of the space between them.
const SHARE: f32 = 0.5;
/// Seconds a word takes to grow once it begins, and to settle back once it has been sung.
const RISE: f64 = 0.18;
const SETTLE: f64 = 0.4;
/// A word sung for longer than `HELD` seconds stays grown after its end for a quarter of the
/// time beyond that, at most `HOLD` seconds more.
const HELD: f64 = 0.6;
const HOLD: f64 = 0.3;
/// The scale of a growing word changes in steps of this much (see the top of the module).
const STEP: f32 = 1.0 / 256.0;
/// Text closer than this many pixels to the text beside it is side by side with it.
const TOUCH: f32 = 0.75;

/// The size of the text in a view of this height: 4 % of it, and not less than 34 pixels.
pub fn text_size(view_height: f32) -> f64 {
    (view_height as f64 * 0.04).max(SMALLEST)
}

/// The lines that are being sung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sung {
    /// The current line (`Lyrics::index_at`), none before the first one begins.
    pub line: Option<usize>,
    /// While the current line is one of background vocals: the main line before it, for as long
    /// as that one lasts.
    pub under: Option<usize>,
}

impl Sung {
    pub fn at(lyrics: &types::Lyrics, position: f64) -> Sung {
        let line = usize::try_from(lyrics.index_at(position)).ok();
        let under = line.filter(|line| lyrics.lines[*line].background).and_then(|line| {
            let main = (0..line).rev().find(|before| !lyrics.lines[*before].background)?;
            lyrics.lines[main].end.is_some_and(|end| position < end).then_some(main)
        });
        Sung { line, under }
    }

    /// The line the view keeps in its place.
    pub fn anchor(&self) -> Option<usize> {
        self.under.or(self.line)
    }

    pub fn has(&self, line: usize) -> bool {
        self.line == Some(line) || self.under == Some(line)
    }
}

/// When each word of a line begins and ends. A word without an end lasts until the next one
/// begins, the last one until the line ends.
pub fn word_times(line: &Line) -> Vec<(f64, f64)> {
    let words = &line.words;
    (0..words.len())
        .map(|i| {
            let begin = words[i].begin;
            let end = words[i].end.or_else(|| words.get(i + 1).map(|next| next.begin)).or(line.end);
            (begin, end.unwrap_or(begin + 0.4).max(begin))
        })
        .collect()
}

/// How much of a word that is sung from `begin` to `end` has been sung at `position`: nothing
/// before it begins, all of it after it ends.
pub fn fill(begin: f64, end: f64, position: f64) -> f32 {
    if position <= begin {
        0.0
    } else if position >= end {
        1.0
    } else {
        ((position - begin) / (end - begin)) as f32
    }
}

/// Whether a word of the lines sung at `position` is being filled at that moment.
pub fn filling(lyrics: &types::Lyrics, position: f64) -> bool {
    if lyrics.timing != Timing::Word {
        return false;
    }
    let sung = Sung::at(lyrics, position);
    [sung.line, sung.under]
        .into_iter()
        .flatten()
        .any(|line| word_times(&lyrics.lines[line]).iter().any(|(begin, end)| *begin < position && position < *end))
}

/// The next moment after `position` at which the lyrics look different while the song plays: a
/// line begins, a word of the lines being sung begins, or the main line under background vocals
/// ends.
pub fn next_change(lyrics: &types::Lyrics, position: f64) -> Option<f64> {
    if lyrics.timing == Timing::None {
        return None;
    }
    let sung = Sung::at(lyrics, position);
    let lines = lyrics.lines.iter().filter_map(|line| line.begin);
    let words = [sung.line, sung.under]
        .into_iter()
        .flatten()
        .flat_map(|line| lyrics.lines[line].words.iter().map(|word| word.begin));
    let end = sung.under.and_then(|line| lyrics.lines[line].end);
    lines.chain(words).chain(end).filter(|moment| *moment > position).min_by(f64::total_cmp)
}

/// How bright a line is while the song moves from one line to the next: `part` (0 to 1) of the
/// way from what it was to what it is.
pub fn level(sung: bool, was_sung: bool, part: f32) -> f32 {
    let of = |lit: bool| if lit { 1.0 } else { DIM };
    of(was_sung) + (of(sung) - of(was_sung)) * part.clamp(0.0, 1.0)
}

/// How visible a line is that reaches from `top` to `bottom` of a view of `height`. Lines fade
/// out over `band` toward the upper edge (which lies `inset` below the top of the view: the
/// header bar is there) and toward the lower edge, instead of being cut off there.
pub fn edge_fade(top: f32, bottom: f32, height: f32, inset: f32, band: f32) -> f32 {
    let part = ((top - inset) / band).min((height - bottom) / band).clamp(0.0, 1.0);
    part * part * (3.0 - 2.0 * part)
}

/// Fast at first, slow at the end.
fn ease(part: f64) -> f64 {
    1.0 - (1.0 - part.clamp(0.0, 1.0)).powi(3)
}

/// Slow at both ends.
fn smooth(part: f64) -> f64 {
    let part = part.clamp(0.0, 1.0);
    part * part * (3.0 - 2.0 * part)
}

/// When a word sung from `begin` to `end` begins to settle back: when it ends (a held word a
/// little later), and not before it has grown.
fn settles(begin: f64, end: f64) -> f64 {
    let held = ((end - begin - HELD) * 0.25).clamp(0.0, HOLD);
    (end + held).max(begin + RISE)
}

/// When a word sung from `begin` to `end` is back at its own size.
pub fn settled(begin: f64, end: f64) -> f64 {
    settles(begin, end) + SETTLE
}

/// How far a word sung from `begin` to `end` has grown at `position`, from 0 (its own size) to 1
/// (as large as it gets): it grows as it begins, stays so while it is sung and settles back after.
pub fn zoom(begin: f64, end: f64, position: f64) -> f32 {
    if position <= begin {
        return 0.0;
    }
    let settles = settles(begin, end);
    let part = if position <= settles {
        smooth((position - begin) / RISE)
    } else {
        1.0 - smooth((position - settles) / SETTLE)
    };
    part as f32
}

/// `scale` in the steps a word grows by.
pub fn step(scale: f32) -> f32 {
    1.0 + ((scale - 1.0) / STEP).round() * STEP
}

/// How much a word `width` wide may grow with `before` and `after` of room beside it, and where
/// the point it grows from lies on it, from its left end: in its middle when the room allows that
/// on both sides, otherwise as near to it as the room allows.
pub fn growth(width: f32, before: f32, after: f32) -> (f32, f32) {
    let (before, after) = (before.max(0.0), after.max(0.0));
    if width <= 0.0 {
        return (1.0, 0.0);
    }
    let most = ZOOM.min(1.0 + (before + after) / width);
    let extra = (most - 1.0) * width;
    let origin = if extra <= 0.0 || (before >= extra / 2.0 && after >= extra / 2.0) {
        width / 2.0
    } else if before < extra / 2.0 {
        before / (most - 1.0)
    } else {
        width - after / (most - 1.0)
    };
    (most, origin)
}

/// A word on one row of a line and the room it may grow into. Timed words without a space between
/// them (the syllables of one word) are one word; a word broken across rows is one on each row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Room {
    /// Where its text is, from left to right.
    pub x: f32,
    pub width: f32,
    /// Its row: the top, the height and the baseline.
    pub y: f32,
    pub height: f32,
    pub baseline: f32,
    /// The room, from left to right: up to halfway to the words beside it, to the ends of the
    /// row at its ends.
    pub left: f32,
    pub right: f32,
    /// The point it grows from, and the scale it grows to at most.
    pub origin: f32,
    pub most: f32,
    /// When it is sung: from the first beginning of its timed words to their last end.
    pub begin: f64,
    pub end: f64,
}

impl Room {
    /// How the word stands at `position`: its scale, and how far it has risen in pixels. Both are
    /// nothing (1 and 0) while it does not grow.
    pub fn grown(&self, position: f64) -> (f32, f32) {
        let part = zoom(self.begin, self.end, position);
        (step(1.0 + (self.most - 1.0) * part), LIFT * self.height * part)
    }

    /// Whether the word grows or settles at `position`.
    pub fn moves(&self, position: f64) -> bool {
        self.begin < position && position < settled(self.begin, self.end)
    }
}

/// The words of a line with their rooms, by rows and from left to right: `pieces` are the
/// stretches of its timed words with the times those are sung, `width` is the width of its rows.
pub fn rooms(pieces: &[(Stretch, f64, f64)], width: f32) -> Vec<Room> {
    let mut pieces: Vec<_> = pieces.iter().filter(|(stretch, ..)| stretch.width > 0.0).collect();
    pieces.sort_by(|(a, ..), (b, ..)| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    let mut rooms: Vec<Room> = Vec::with_capacity(pieces.len());
    for (stretch, begin, end) in pieces {
        let right = stretch.x + stretch.width;
        let touching = |room: &&mut Room| room.y == stretch.y && stretch.x - (room.x + room.width) < TOUCH;
        match rooms.last_mut().filter(touching) {
            Some(room) => {
                room.width = right.max(room.x + room.width) - room.x;
                room.begin = room.begin.min(*begin);
                room.end = room.end.max(*end);
            }
            None => rooms.push(Room {
                x: stretch.x,
                width: stretch.width,
                y: stretch.y,
                height: stretch.height,
                baseline: stretch.baseline,
                left: 0.0,
                right: width,
                origin: 0.0,
                most: 1.0,
                begin: *begin,
                end: *end,
            }),
        }
    }
    for n in 0..rooms.len() {
        let room = rooms[n];
        let before = n.checked_sub(1).map(|n| rooms[n]).filter(|before| before.y == room.y);
        let after = rooms.get(n + 1).filter(|after| after.y == room.y);
        let left = before.map_or(room.x.min(0.0), |before| (before.x + before.width + room.x) / 2.0);
        let right = after.map_or(width.max(room.x + room.width), |after| (room.x + room.width + after.x) / 2.0);
        let (most, origin) = growth(room.width, (room.x - left) * SHARE, (right - room.x - room.width) * SHARE);
        rooms[n] = Room { left, right, most, origin: room.x + origin, ..room };
    }
    rooms
}

/// A part of a line that is drawn in one piece: an area of it (x, y, width, height), and for a
/// word that grows in it how it stands (see `Grown`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Part {
    pub area: (f32, f32, f32, f32),
    pub grown: Option<Grown>,
}

/// A word that has grown: its scale about the point (x, y) on its baseline that it grows from,
/// and how far it has risen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grown {
    pub scale: f32,
    pub x: f32,
    pub y: f32,
    pub lift: f32,
}

/// Cut a line `width` x `height` into the parts it is drawn in while words of it grow, from the
/// top: on a row with a growing word each such word's room is a part, and what lies between them
/// a part as it is; rows without one are drawn together. The parts reach `margin` beyond the
/// line, so that nothing that stands out of it is cut off. `grown` tells how a word stands.
pub fn parts(
    rooms: &[Room],
    grown: impl Fn(&Room) -> (f32, f32),
    width: f32,
    height: f32,
    margin: f32,
    mut part: impl FnMut(Part),
) {
    let (start, end) = (-margin, width + margin);
    let grows = |room: &Room| grown(room) != (1.0, 0.0);
    // from x to x, from y to y
    let mut emit = |(left, right): (f32, f32), (top, bottom): (f32, f32), grown: Option<Grown>| {
        if right > left && bottom > top {
            part(Part { area: (left, top, right - left, bottom - top), grown });
        }
    };
    // where the rows without a growing word begin that are not drawn yet
    let mut waiting: Option<f32> = None;
    let (mut top, mut first) = (-margin, 0);
    while first < rooms.len() {
        let y = rooms[first].y;
        let last = rooms[first..].iter().position(|room| room.y != y).map_or(rooms.len(), |count| first + count);
        let row = &rooms[first..last];
        let bottom = if last == rooms.len() { height + margin } else { y + row[0].height };
        if row.iter().any(grows) {
            if let Some(from) = waiting.take() {
                emit((start, end), (from, top), None);
            }
            let mut x = start;
            for (n, room) in row.iter().enumerate().filter(|(_, room)| grows(room)) {
                let (scale, lift) = grown(room);
                let left = if n == 0 { start } else { room.left };
                let right = if n + 1 == row.len() { end } else { room.right };
                emit((x, left), (top, bottom), None);
                emit((left, right), (top, bottom), Some(Grown { scale, x: room.origin, y: room.baseline, lift }));
                x = right;
            }
            emit((x, end), (top, bottom), None);
        } else if waiting.is_none() {
            waiting = Some(top);
        }
        (top, first) = (bottom, last);
    }
    if let Some(from) = waiting {
        emit((start, end), (from, top), None);
    }
}

/// Where the lines are, one under the other, in what the view scrolls through.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flow {
    pub tops: Vec<f32>,
    pub heights: Vec<f32>,
    /// The height of everything: the lines and the room above and below them.
    pub extent: f32,
    /// How far below the top of the view the line that is sung is kept (timed lyrics).
    anchor: Option<f32>,
}

impl Flow {
    /// Stack lines of these `heights`, each `gaps[i]` below the one before it, for a view of
    /// height `view`. With an `anchor` there is enough room above the first and below the last
    /// line to scroll each of them to it; without one the text begins `lead` below the top and
    /// ends `lead` above the bottom.
    pub fn new(heights: &[f32], gaps: &[f32], view: f32, anchor: Option<f32>, lead: f32) -> Flow {
        let mut tops = Vec::with_capacity(heights.len());
        let mut y = anchor.unwrap_or(lead);
        for (line, height) in heights.iter().enumerate() {
            if line > 0 {
                y += gaps.get(line).copied().unwrap_or(0.0);
            }
            tops.push(y);
            y += height;
        }
        let extent = match (anchor, tops.last()) {
            (Some(anchor), Some(last)) => (last - anchor + view).max(y + lead),
            _ => y + lead,
        };
        Flow { tops, heights: heights.to_vec(), extent: extent.max(view), anchor }
    }

    /// The scroll position that shows `line` at the anchor (the beginning before the first line
    /// is sung), for a view of height `view`.
    pub fn target(&self, line: Option<usize>, view: f32) -> f32 {
        let wanted = match (line.and_then(|line| self.tops.get(line)), self.anchor) {
            (Some(top), Some(anchor)) => top - anchor,
            _ => 0.0,
        };
        wanted.clamp(0.0, (self.extent - view).max(0.0))
    }

    /// The line at `y`.
    pub fn line_at(&self, y: f32) -> Option<usize> {
        let line = self.tops.partition_point(|top| *top <= y).checked_sub(1)?;
        (y < self.tops[line] + self.heights[line]).then_some(line)
    }

    /// The lines that reach into the part from `from` to `to`.
    pub fn visible(&self, from: f32, to: f32) -> std::ops::Range<usize> {
        let first = self.tops.partition_point(|top| *top <= from).saturating_sub(1);
        first..self.tops.partition_point(|top| *top < to).max(first)
    }
}

/// A part of one row of a line that belongs to one word, in the line's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The baseline of the row.
    pub baseline: f32,
    /// The text runs from right to left here.
    pub backwards: bool,
}

/// The parts (x, y, width, height) of a word that are lit when `part` (0 to 1) of it is sung.
/// The word lies on `stretches`, in reading order: more than one when it is broken across rows.
pub fn lit(stretches: &[Stretch], part: f32) -> impl Iterator<Item = (f32, f32, f32, f32)> + '_ {
    let total: f32 = stretches.iter().map(|stretch| stretch.width).sum();
    let mut left = total * part.clamp(0.0, 1.0);
    stretches.iter().map_while(move |stretch| {
        if left <= 0.0 {
            return None;
        }
        let width = left.min(stretch.width);
        let x = if stretch.backwards { stretch.x + stretch.width - width } else { stretch.x };
        left -= width;
        Some((x, stretch.y, width, stretch.height))
    })
}

/// Where the light ends in a word of which `part` is sung, while that is inside the word: the
/// front's x and y, the height of its row, how much of the word's stretch on that row lies
/// beyond it, and whether the text runs from right to left there.
pub fn front(stretches: &[Stretch], part: f32) -> Option<(f32, f32, f32, f32, bool)> {
    if part <= 0.0 || part >= 1.0 {
        return None;
    }
    let total: f32 = stretches.iter().map(|stretch| stretch.width).sum();
    let mut left = total * part;
    for stretch in stretches {
        if left < stretch.width {
            let x = if stretch.backwards { stretch.x + stretch.width - left } else { stretch.x + left };
            return Some((x, stretch.y, stretch.height, stretch.width - left, stretch.backwards));
        }
        left -= stretch.width;
    }
    None
}

/// Add `part` to the lit parts of a line: what is lit side by side on a row becomes one part.
fn join(parts: &mut Vec<(f32, f32, f32, f32)>, part: (f32, f32, f32, f32)) {
    if let Some(last) = parts.last_mut().filter(|last| last.1 == part.1) {
        if (last.0 + last.2 - part.0).abs() < TOUCH {
            last.2 = part.0 + part.2 - last.0;
            return;
        }
        if (part.0 + part.2 - last.0).abs() < TOUCH {
            *last = (part.0, last.1, last.0 + last.2 - part.0, last.3);
            return;
        }
    }
    parts.push(part);
}

/// Where the piece of a layout's text from byte `from` to byte `to` is drawn.
fn stretches_of(layout: &pango::Layout, from: i32, to: i32) -> Vec<Stretch> {
    let unit = pango::SCALE as f32;
    let mut stretches = Vec::new();
    let mut rows = layout.iter();
    loop {
        if let Some(row) = rows.line_readonly() {
            let (start, end) = (row.start_index(), row.start_index() + row.length());
            if from < end && to > start {
                let (top, bottom) = rows.line_yrange();
                let baseline = rows.baseline() as f32 / unit;
                let backwards = row.resolved_direction() == pango::Direction::Rtl;
                for [left, right] in row.x_ranges(from.max(start), to.min(end)).as_chunks::<2>().0 {
                    stretches.push(Stretch {
                        x: *left as f32 / unit,
                        y: top as f32 / unit,
                        width: (right - left) as f32 / unit,
                        height: (bottom - top) as f32 / unit,
                        baseline,
                        backwards,
                    });
                }
            }
        }
        if !rows.next_line() {
            return stretches;
        }
    }
}

/// A word of a line with word timing: when it is sung and where it is drawn.
pub struct Word {
    begin: f64,
    end: f64,
    stretches: Vec<Stretch>,
}

/// The timed words of a line, as they light up and as they grow.
pub struct Words {
    words: Vec<Word>,
    rooms: Vec<Room>,
}

/// A line as it is drawn.
pub struct Row {
    layout: pango::Layout,
    /// Where its text begins and ends from left to right (text that runs from right to left
    /// stands at the right).
    across: (f32, f32),
    /// Its timed words, looked up when the line is first sung.
    words: OnceCell<Words>,
}

impl Row {
    /// The words of `line`, which this row shows one after the other.
    fn words(&self, line: &Line) -> &Words {
        self.words.get_or_init(|| {
            let limit = self.layout.text().len();
            let mut from = 0;
            let words: Vec<Word> = line
                .words
                .iter()
                .zip(word_times(line))
                .map(|(word, (begin, end))| {
                    // the space after a word is not part of what lights up
                    let to = (from + word.text.trim_end().len()).min(limit);
                    let stretches =
                        if from < to { stretches_of(&self.layout, from as i32, to as i32) } else { Vec::new() };
                    from += word.text.len();
                    Word { begin, end, stretches }
                })
                .collect();
            let pieces: Vec<(Stretch, f64, f64)> = words
                .iter()
                .flat_map(|word| word.stretches.iter().map(|stretch| (*stretch, word.begin, word.end)))
                .collect();
            let width = self.layout.width() as f32 / pango::SCALE as f32;
            Words { rooms: rooms(&pieces, width), words }
        })
    }
}

/// What is lit of a line that is being sung. It is gathered anew for every frame into the same
/// vectors: drawing allocates nothing for it.
#[derive(Default)]
pub struct Light {
    /// The lit parts (x, y, width, height), those side by side joined.
    parts: Vec<(f32, f32, f32, f32)>,
    /// Where the light ends in the words that are being filled (see `front`).
    fronts: Vec<(f32, f32, f32, f32, bool)>,
}

impl Light {
    fn gather(&mut self, words: &[Word], position: f64) {
        self.parts.clear();
        self.fronts.clear();
        for word in words {
            let filled = fill(word.begin, word.end, position);
            for part in lit(&word.stretches, filled) {
                join(&mut self.parts, part);
            }
            self.fronts.extend(front(&word.stretches, filled));
        }
    }
}

/// Whether the desktop asks for less motion. GTK says so from 4.22 on; the setting is looked up
/// by its name because the bindings this program is built with are those of GTK 4.20.
fn reduced_motion(settings: &gtk::Settings) -> bool {
    const NAME: &str = "gtk-interface-reduced-motion";
    settings.find_property(NAME).is_some()
        && glib::EnumValue::from_value(&settings.property_value(NAME))
            .is_some_and(|(_, value)| value.nick() == "reduce")
}

/// The view on its way from one scroll position to another.
#[derive(Clone, Copy)]
pub struct Glide {
    from: f64,
    to: f64,
    /// The time of the frame it began with; none before its first frame.
    started: Option<i64>,
}

mod imp {
    use super::*;

    #[derive(glib::Properties)]
    #[properties(wrapper_type = super::Lyrics)]
    pub struct Lyrics {
        // what a scrolled window asks of its content
        #[property(override_interface = gtk::Scrollable, get, set, nullable)]
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vadjustment, nullable)]
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub on_scroll: RefCell<Option<glib::SignalHandlerId>>,

        pub text: RefCell<Option<types::Lyrics>>,
        pub rows: RefCell<Vec<Row>>,
        pub flow: RefCell<Flow>,
        /// What the rows were made for: the width, the height, the font.
        pub made_for: Cell<(i32, i32, u32)>,
        /// The rows have to be made again whatever they were made for.
        pub stale: Cell<bool>,
        /// The size of the text in pixels.
        pub size: Cell<f64>,
        /// How much of the view lies above this widget: the text grows with the view's height.
        pub above: Cell<f32>,
        /// How far below the top of this widget lines begin to fade out.
        pub inset: Cell<f32>,

        pub sung: Cell<Sung>,
        /// The lines that were sung before, while they dim.
        pub before: Cell<Sung>,
        /// How far the change from `before` to `sung` has come, 0 to 1, and the frame it began at.
        pub turned: Cell<f32>,
        pub turn_started: Cell<Option<i64>>,
        pub glide: Cell<Option<Glide>>,
        /// This widget is moving the text itself (everything else that moves it is the user).
        pub moving: Cell<bool>,
        /// When the user last scrolled, while the view waits before it follows the song again.
        pub scrolled_at: Cell<Option<Instant>>,
        pub resume: RefCell<Option<glib::SourceId>>,
        /// The timer for the moment the lyrics change next, and that moment.
        pub wait: RefCell<Option<glib::SourceId>>,
        pub wait_until: Cell<Option<Instant>>,
        pub tick: RefCell<Option<gtk::TickCallbackId>>,

        pub pointer: Cell<Option<(f64, f64)>>,
        pub pressed: Cell<Option<(f64, f64)>>,
        pub hover: Cell<Option<usize>>,
        pub pointer_hidden: Cell<bool>,
        /// What is lit of the line being drawn.
        pub light: RefCell<Light>,
    }

    impl Default for Lyrics {
        fn default() -> Self {
            Lyrics {
                hadjustment: RefCell::default(),
                vadjustment: RefCell::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                on_scroll: RefCell::default(),
                text: RefCell::default(),
                rows: RefCell::default(),
                flow: RefCell::default(),
                made_for: Cell::new((0, 0, 0)),
                stale: Cell::new(false),
                size: Cell::new(SMALLEST),
                above: Cell::new(0.0),
                inset: Cell::new(0.0),
                sung: Cell::default(),
                before: Cell::default(),
                turned: Cell::new(1.0),
                turn_started: Cell::new(None),
                glide: Cell::new(None),
                moving: Cell::new(false),
                scrolled_at: Cell::new(None),
                resume: RefCell::default(),
                wait: RefCell::default(),
                wait_until: Cell::new(None),
                tick: RefCell::default(),
                pointer: Cell::new(None),
                pressed: Cell::new(None),
                hover: Cell::new(None),
                pointer_hidden: Cell::new(false),
                light: RefCell::default(),
            }
        }
    }

    impl Lyrics {
        fn set_vadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            if let (Some(old), Some(handler)) = (self.vadjustment.take(), self.on_scroll.take()) {
                old.disconnect(handler);
            }
            if let Some(adjustment) = &adjustment {
                let weak = self.obj().downgrade();
                let handler = adjustment.connect_value_changed(move |_| {
                    if let Some(lyrics) = weak.upgrade() {
                        lyrics.moved();
                    }
                });
                self.on_scroll.replace(Some(handler));
            }
            self.vadjustment.replace(adjustment);
            self.obj().fit();
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Lyrics {
        const NAME: &'static str = "AmLyrics";
        type Type = super::Lyrics;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("lyrics");
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for Lyrics {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let motion = gtk::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_motion(move |_, x, y| {
                if let Some(lyrics) = weak.upgrade() {
                    lyrics.point(Some((x, y)));
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(lyrics) = weak.upgrade() {
                    lyrics.point(None);
                }
            });
            obj.add_controller(motion);

            let click = gtk::GestureClick::new();
            let weak = obj.downgrade();
            click.connect_pressed(move |_, _, x, y| {
                if let Some(lyrics) = weak.upgrade() {
                    lyrics.imp().pressed.set(Some((x, y)));
                }
            });
            let weak = obj.downgrade();
            click.connect_released(move |_, _, x, y| {
                if let Some(lyrics) = weak.upgrade() {
                    lyrics.clicked(x, y);
                }
            });
            obj.add_controller(click);
        }
    }

    impl WidgetImpl for Lyrics {
        fn map(&self) {
            self.parent_map();
            self.obj().refresh(false);
        }

        fn unmap(&self) {
            // hidden: nothing runs, and the view is at its line again when it comes back
            let obj = self.obj();
            if let Some(tick) = self.tick.take() {
                tick.remove();
            }
            obj.stop_waiting();
            if let Some(timer) = self.resume.take() {
                timer.remove();
            }
            self.scrolled_at.set(None);
            self.glide.set(None);
            self.turned.set(1.0);
            self.pointer.set(None);
            self.hover.set(None);
            self.parent_unmap();
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, _width: i32, _height: i32, _baseline: i32) {
            self.obj().arrange();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (text, rows, flow) = (self.text.borrow(), self.rows.borrow(), self.flow.borrow());
            let Some(text) = text.as_ref().filter(|text| text.lines.len() == rows.len()) else { return };
            let (width, height, scroll) = (obj.width() as f32, obj.height() as f32, obj.scroll() as f32);
            let colour = obj.color();
            let size = self.size.get() as f32;
            let band = size * 1.5;
            let timed = text.timing != Timing::None;
            let (sung, before, turned) = (self.sung.get(), self.before.get(), self.turned.get());
            let position = ctx().player.position();
            let zooms = obj.zooms();
            let mut light = self.light.borrow_mut();
            for line in flow.visible(scroll, scroll + height) {
                let (top, tall) = (flow.tops[line] - scroll, flow.heights[line]);
                let fade = edge_fade(top, top + tall, height, self.inset.get(), band);
                if tall <= 0.0 || fade <= 0.0 {
                    continue;
                }
                let (row, source) = (&rows[line], &text.lines[line]);
                // the words of the lines being sung light up as they are sung; those of a line
                // sung before may still be settling back
                let by_word = timed && sung.has(line) && !source.words.is_empty();
                let words = if by_word { Some(row.words(source)) } else { row.words.get() };
                if by_word && let Some(words) = words {
                    light.gather(&words.words, position);
                }
                let bright = if timed { level(sung.has(line), before.has(line), turned) } else { 1.0 };
                let bright = if self.hover.get() == Some(line) { bright.max(POINTED) } else { bright };
                // the line as it is lit, as far as it lies in `area` (all of it without one)
                let paint = |area: Option<(f32, f32, f32, f32)>| {
                    if !by_word {
                        snapshot.append_layout(&row.layout, &with_alpha(&colour, bright * fade));
                        return;
                    }
                    snapshot.append_layout(&row.layout, &with_alpha(&colour, DIM * fade));
                    let light_up = |(x, y, wide, high): (f32, f32, f32, f32), bright: f32| {
                        let inside = area.is_none_or(|(left, top, across, down)| {
                            x < left + across && left < x + wide && y < top + down && top < y + high
                        });
                        if inside {
                            snapshot.push_clip(&graphene::Rect::new(x, y, wide, high));
                            snapshot.append_layout(&row.layout, &with_alpha(&colour, bright * fade));
                            snapshot.pop();
                        }
                    };
                    for part in &light.parts {
                        light_up(*part, 1.0);
                    }
                    // the light ends softly: a few slices beyond its front, each dimmer than the
                    // one before, and none beyond the word
                    for &(x, y, high, room, backwards) in &light.fronts {
                        let reach = (size * SOFT).min(room);
                        for slice in 0..SLICES {
                            let near = reach * slice as f32 / SLICES as f32;
                            let far = reach * (slice + 1) as f32 / SLICES as f32;
                            let bright = 1.0 - (slice as f32 + 0.5) / SLICES as f32;
                            light_up((if backwards { x - far } else { x + near }, y, far - near, high), bright);
                        }
                    }
                };
                snapshot.save();
                snapshot.translate(&graphene::Point::new(0.0, top));
                let grown = |room: &Room| room.grown(position);
                let growing = words.filter(|words| zooms && words.rooms.iter().any(|room| grown(room) != (1.0, 0.0)));
                match growing {
                    // each growing word is drawn scaled and risen in its room, the rest of the line
                    // around them
                    Some(words) => parts(&words.rooms, grown, width, tall, size, |part| {
                        snapshot.save();
                        if let Some(Grown { scale, x, y, lift }) = part.grown {
                            snapshot.translate(&graphene::Point::new(x, y - lift));
                            snapshot.scale(scale, scale);
                            snapshot.translate(&graphene::Point::new(-x, -y));
                        }
                        let (x, y, wide, high) = part.area;
                        snapshot.push_clip(&graphene::Rect::new(x, y, wide, high));
                        paint(Some(part.area));
                        snapshot.pop();
                        snapshot.restore();
                    }),
                    None => paint(None),
                }
                snapshot.restore();
            }
        }
    }

    impl ScrollableImpl for Lyrics {}
}

glib::wrapper! {
    pub struct Lyrics(ObjectSubclass<imp::Lyrics>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl Default for Lyrics {
    fn default() -> Self {
        Lyrics::new()
    }
}

impl Lyrics {
    pub fn new() -> Lyrics {
        let lyrics: Lyrics = glib::Object::new();
        let player = ctx().player.clone();
        player.position_changed.connect(&lyrics, |lyrics, _| lyrics.refresh(true));
        player.changed.connect(&lyrics, |lyrics, _| lyrics.refresh(true));
        lyrics
    }

    /// The scrolled window that scrolls these lyrics: without a scrollbar, in step with the
    /// display.
    pub fn scrolled(&self) -> gtk::ScrolledWindow {
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .child(self)
            .build();
        SmoothScroll::attach(&scroller, Tuning::default());
        scroller
    }

    /// Show these lyrics (or none), beginning at the line that is being sung.
    pub fn set_text(&self, text: Option<types::Lyrics>) {
        let imp = self.imp();
        if *imp.text.borrow() == text {
            return;
        }
        imp.text.replace(text);
        imp.sung.set(Sung::default());
        imp.before.set(Sung::default());
        imp.turned.set(1.0);
        imp.glide.set(None);
        imp.hover.set(None);
        self.follow_again();
        imp.stale.set(true);
        if self.width() > 0 {
            self.arrange();
        }
        if !self.timed() {
            // text without timing is read from its beginning
            self.scroll_to(0.0);
        }
        self.queue_draw();
        self.refresh(false);
    }

    /// Where this widget is in its view: `above` is how much of the view lies above it (the text
    /// grows with the height of the whole view), `inset` how far below its own top the lines
    /// begin to fade out (the view's header bar lies over that part).
    pub fn set_frame(&self, above: f32, inset: f32) {
        let imp = self.imp();
        if imp.above.replace(above) != above || imp.inset.replace(inset) != inset {
            imp.stale.set(true);
            if self.width() > 0 {
                self.arrange();
                self.queue_draw();
            }
        }
    }

    /// Hide the pointer over the text (the view does that when the pointer rests). A pointer
    /// that is not shown points at no line.
    pub fn set_pointer_hidden(&self, hidden: bool) {
        let imp = self.imp();
        if imp.pointer_hidden.replace(hidden) != hidden {
            self.point(imp.pointer.get());
            self.show_cursor();
        }
    }

    fn scroll(&self) -> f64 {
        self.imp().vadjustment.borrow().as_ref().map_or(0.0, |adjustment| adjustment.value())
    }

    fn timed(&self) -> bool {
        self.imp().text.borrow().as_ref().is_some_and(|text| text.timing != Timing::None)
    }

    /// Make the rows for the width, the text size and the font of the moment when one of those
    /// changed, and keep the view at its line.
    fn arrange(&self) {
        let imp = self.imp();
        let (width, height) = (self.width(), self.height());
        let made_for = (width, height, self.pango_context().serial());
        let fresh = imp.stale.replace(false);
        if imp.made_for.replace(made_for) == made_for && !fresh {
            return;
        }
        let size = text_size(height as f32 + imp.above.get());
        imp.size.set(size);
        let (mut rows, mut heights, mut gaps) = (Vec::new(), Vec::new(), Vec::new());
        let timed = {
            let text = imp.text.borrow();
            for line in text.as_ref().map_or(&[][..], |text| &text.lines[..]) {
                let layout =
                    text_layout(self, if line.background { size * BACKGROUND } else { size }, pango::Weight::Bold, 0.0);
                layout.set_width(width.max(1) * pango::SCALE);
                layout.set_wrap(pango::WrapMode::WordChar);
                // a line with word timing is its words one after the other
                let words: String = line.words.iter().map(|word| word.text.as_str()).collect();
                let shown = if words.trim().is_empty() { line.text.trim_end() } else { words.trim_end() };
                layout.set_text(shown);
                let (_, text) = layout.pixel_extents();
                let blank = shown.trim().is_empty();
                heights.push(if blank { 0.0 } else { text.height() as f32 });
                // background vocals stay close to the line they belong to
                gaps.push(if blank { 0.0 } else { size as f32 * if line.background { 0.25 } else { 0.7 } });
                let across = (text.x() as f32, (text.x() + text.width()) as f32);
                rows.push(Row { layout, across, words: OnceCell::new() });
            }
            text.as_ref().is_some_and(|text| text.timing != Timing::None)
        };
        let inset = imp.inset.get();
        // the top of the line that is sung, with its first row about a third down the view
        let anchor = timed.then(|| (height as f32 * ANCHOR - size as f32 * 0.6).max(inset));
        // text without timing begins below the part in which lines fade out
        let lead = inset + size as f32 * 1.7;
        imp.flow.replace(Flow::new(&heights, &gaps, height as f32, anchor, lead));
        imp.rows.replace(rows);
        self.fit();
        self.follow(false);
    }

    /// Tell the scrolled window how much there is to scroll.
    fn fit(&self) {
        let imp = self.imp();
        let (width, page) = (self.width() as f64, self.height() as f64);
        let extent = (imp.flow.borrow().extent as f64).max(page);
        imp.moving.set(true);
        if let Some(adjustment) = imp.hadjustment.borrow().as_ref() {
            adjustment.configure(0.0, 0.0, width, width * 0.1, width * 0.9, width);
        }
        if let Some(adjustment) = imp.vadjustment.borrow().clone() {
            let value = adjustment.value().clamp(0.0, extent - page);
            adjustment.configure(value, 0.0, extent, imp.size.get() * 2.0, page * 0.8, page);
        }
        imp.moving.set(false);
    }

    fn scroll_to(&self, value: f64) {
        let imp = self.imp();
        let Some(adjustment) = imp.vadjustment.borrow().clone() else { return };
        imp.moving.set(true);
        adjustment.set_value(value);
        imp.moving.set(false);
    }

    /// The text is somewhere else than it was.
    fn moved(&self) {
        let imp = self.imp();
        self.queue_draw();
        // another line may lie under the pointer now
        self.point(imp.pointer.get());
        if imp.moving.get() || !self.timed() {
            return;
        }
        // not moved from here: the user scrolls, and reads where they like for a while
        imp.glide.set(None);
        imp.scrolled_at.set(Some(Instant::now()));
        if imp.resume.borrow().is_none() {
            self.resume_after(RESUME);
        }
    }

    /// Follow the song again once the user has not scrolled for a while.
    fn resume_after(&self, wait: Duration) {
        let weak = self.downgrade();
        let timer = glib::timeout_add_local_once(wait, move || {
            activity::note(activity::Source::Lyrics);
            let Some(lyrics) = weak.upgrade() else { return };
            let imp = lyrics.imp();
            imp.resume.replace(None);
            match imp.scrolled_at.get().map(|at| at.elapsed() + SOON) {
                // scrolled again in the meantime: wait for the rest of the pause
                Some(since) if since < RESUME => lyrics.resume_after(RESUME - since + SOON),
                _ => {
                    imp.scrolled_at.set(None);
                    lyrics.refresh(true);
                }
            }
        });
        self.imp().resume.replace(Some(timer));
    }

    /// Forget that the user scrolled.
    fn follow_again(&self) {
        let imp = self.imp();
        imp.scrolled_at.set(None);
        if let Some(timer) = imp.resume.take() {
            timer.remove();
        }
    }

    /// Look at the player: light the lines that are being sung and bring the view to them, and
    /// see to it that what has to run for that runs. `smoothly`: over a moment, otherwise at once.
    fn refresh(&self, smoothly: bool) {
        if !self.is_mapped() {
            return;
        }
        self.follow(smoothly);
        self.run();
    }

    fn follow(&self, smoothly: bool) {
        let imp = self.imp();
        let text = imp.text.borrow();
        let Some(text) = text.as_ref().filter(|text| text.timing != Timing::None) else { return };
        let smoothly = smoothly && adw::is_animations_enabled(self);
        let sung = Sung::at(text, ctx().player.position());
        let before = imp.sung.replace(sung);
        if before != sung {
            imp.before.set(before);
            imp.turned.set(if smoothly { 0.0 } else { 1.0 });
            imp.turn_started.set(None);
            self.queue_draw();
        }
        if imp.scrolled_at.get().is_some() {
            // the user is reading elsewhere
            return;
        }
        let target = imp.flow.borrow().target(sung.anchor(), self.height() as f32) as f64;
        let heading = imp.glide.get().map_or(self.scroll(), |glide| glide.to);
        if (heading - target).abs() < 0.5 {
            return;
        }
        if smoothly {
            imp.glide.set(Some(Glide { from: self.scroll(), to: target, started: None }));
        } else {
            imp.glide.set(None);
            self.scroll_to(target);
        }
    }

    /// Whether a word is being filled at this moment.
    fn filling(&self) -> bool {
        let player = ctx().player.clone();
        player.is_playing() && self.imp().text.borrow().as_ref().is_some_and(|text| filling(text, player.position()))
    }

    /// Whether words grow as they are sung: not when the desktop asks for no animations (when
    /// libadwaita skips its own) or for less motion.
    fn zooms(&self) -> bool {
        adw::is_animations_enabled(self) && !reduced_motion(&self.settings())
    }

    /// Whether a word in view grows or settles at this moment.
    fn zooming(&self) -> bool {
        let imp = self.imp();
        let player = ctx().player.clone();
        if !player.is_playing() || !self.zooms() {
            return false;
        }
        let position = player.position();
        let (rows, flow) = (imp.rows.borrow(), imp.flow.borrow());
        let scroll = self.scroll() as f32;
        flow.visible(scroll, scroll + self.height() as f32).any(|line| {
            // the words of a line are looked up when it is first sung: no other one has grown
            let words = rows.get(line).and_then(|row| row.words.get());
            words.is_some_and(|words| words.rooms.iter().any(|room| room.moves(position)))
        })
    }

    /// Whether something changes with every frame.
    fn busy(&self) -> bool {
        let imp = self.imp();
        imp.glide.get().is_some() || imp.turned.get() < 1.0 || self.filling() || self.zooming()
    }

    /// See to it that what has to run runs: the tick callback while something changes with every
    /// frame, otherwise the timer for the moment the lyrics change next.
    fn run(&self) {
        let imp = self.imp();
        if imp.tick.borrow().is_some() {
            return;
        }
        if self.busy() {
            self.stop_waiting();
            let tick = self.add_tick_callback(|lyrics, clock| lyrics.frame(clock));
            imp.tick.replace(Some(tick));
        } else {
            self.wait_for_change();
        }
    }

    fn frame(&self, clock: &gdk::FrameClock) -> glib::ControlFlow {
        activity::note(activity::Source::Lyrics);
        let imp = self.imp();
        let now = clock.frame_time();
        if let Some(glide) = imp.glide.get() {
            // a glide and a change of brightness begin with the first frame that shows them
            let started = glide.started.unwrap_or(now);
            let part = ((now - started) as f64 / GLIDE).min(1.0);
            imp.glide.set((part < 1.0).then_some(Glide { started: Some(started), ..glide }));
            self.scroll_to(glide.from + (glide.to - glide.from) * ease(part));
        }
        if imp.turned.get() < 1.0 {
            let started = imp.turn_started.get().unwrap_or(now);
            imp.turn_started.set(Some(started));
            imp.turned.set(((now - started) as f64 / GLIDE).min(1.0) as f32);
            self.queue_draw();
        }
        if ctx().player.is_playing() {
            // the song goes on: the next line may be due, a word may be filling or growing
            self.follow(true);
            if self.filling() || self.zooming() {
                self.queue_draw();
            }
        }
        if self.busy() {
            return glib::ControlFlow::Continue;
        }
        // nothing changes until the next line or word: the callback ends here
        imp.tick.replace(None);
        self.wait_for_change();
        glib::ControlFlow::Break
    }

    /// Between the lines nothing is drawn: one timer waits for the moment the lyrics change next.
    fn wait_for_change(&self) {
        let imp = self.imp();
        let player = ctx().player.clone();
        let next = if self.is_mapped() && player.is_playing() {
            let position = player.position();
            imp.text.borrow().as_ref().and_then(|text| next_change(text, position)).map(|moment| moment - position)
        } else {
            None
        };
        let Some(seconds) = next else { return self.stop_waiting() };
        // a little late rather than early: the line has to have begun when the timer looks
        let due = Instant::now() + Duration::from_secs_f64(seconds.max(0.0) + 0.002);
        let near = |a: Instant, b: Instant| a.max(b).duration_since(a.min(b)) < Duration::from_millis(20);
        if imp.wait.borrow().is_some() && imp.wait_until.get().is_some_and(|until| near(until, due)) {
            // the timer that waits is the right one: the song was not moved
            return;
        }
        self.stop_waiting();
        let weak = self.downgrade();
        let timer = glib::timeout_add_local_once(due.saturating_duration_since(Instant::now()), move || {
            activity::note(activity::Source::Lyrics);
            if let Some(lyrics) = weak.upgrade() {
                lyrics.imp().wait.replace(None);
                lyrics.refresh(true);
            }
        });
        imp.wait.replace(Some(timer));
        imp.wait_until.set(Some(due));
    }

    fn stop_waiting(&self) {
        let imp = self.imp();
        if let Some(timer) = imp.wait.take() {
            timer.remove();
        }
        imp.wait_until.set(None);
    }

    /// The line with a beginning to seek to at this point of the widget.
    fn line_under(&self, x: f64, y: f64) -> Option<usize> {
        let imp = self.imp();
        let line = imp.flow.borrow().line_at((y + self.scroll()) as f32)?;
        let on_text = imp
            .rows
            .borrow()
            .get(line)
            .is_some_and(|row| (row.across.0 - 16.0..=row.across.1 + 16.0).contains(&(x as f32)));
        let seekable =
            imp.text.borrow().as_ref().and_then(|text| text.lines.get(line)).is_some_and(|l| l.begin.is_some());
        (on_text && seekable).then_some(line)
    }

    /// The pointer is at this point of the widget (or nowhere over it).
    fn point(&self, at: Option<(f64, f64)>) {
        let imp = self.imp();
        imp.pointer.set(at);
        let hover = at.filter(|_| !imp.pointer_hidden.get()).and_then(|(x, y)| self.line_under(x, y));
        if imp.hover.replace(hover) != hover {
            self.queue_draw();
            self.show_cursor();
        }
    }

    fn show_cursor(&self) {
        let imp = self.imp();
        self.set_cursor_from_name(if imp.pointer_hidden.get() {
            Some("none")
        } else if imp.hover.get().is_some() {
            Some("pointer")
        } else {
            None
        });
    }

    /// A click on a line seeks to its beginning.
    fn clicked(&self, x: f64, y: f64) {
        let imp = self.imp();
        let Some((from_x, from_y)) = imp.pressed.take() else { return };
        if (from_x - x).abs() > 8.0 || (from_y - y).abs() > 8.0 {
            // dragged, not clicked
            return;
        }
        let begin = self.line_under(x, y).and_then(|line| imp.text.borrow().as_ref()?.lines.get(line)?.begin);
        if let Some(begin) = begin {
            // the view follows the song again, from the line that was chosen
            self.follow_again();
            ctx().player.playback.seek(begin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amui::types::Word as TimedWord;

    fn line(begin: f64, end: f64, text: &str) -> Line {
        Line { begin: Some(begin), end: Some(end), text: text.to_string(), ..Line::default() }
    }

    fn word(begin: f64, end: Option<f64>, text: &str) -> TimedWord {
        TimedWord { begin, end, text: text.to_string() }
    }

    /// Three lines; the third is background vocals that begin while the second is still sung.
    fn lyrics() -> types::Lyrics {
        let mut second = line(10.0, 16.0, "paper lantern");
        second.words = vec![word(10.0, Some(11.0), "paper "), word(12.0, None, "lantern")];
        let mut echo = line(14.0, 15.5, "lantern");
        echo.background = true;
        echo.words = vec![word(14.0, Some(15.0), "lantern")];
        types::Lyrics {
            lines: vec![line(4.0, 8.0, "quiet harbor"), second, echo, line(20.0, 24.0, "amber tide")],
            timing: Timing::Word,
            ..types::Lyrics::default()
        }
    }

    #[test]
    fn the_current_line_is_the_last_one_that_began() {
        let lyrics = lyrics();
        assert_eq!(Sung::at(&lyrics, 0.0), Sung { line: None, under: None });
        assert_eq!(Sung::at(&lyrics, 4.0).line, Some(0));
        // a line stays current after its end, until the next one begins
        assert_eq!(Sung::at(&lyrics, 9.5), Sung { line: Some(0), under: None });
        assert_eq!(Sung::at(&lyrics, 10.0).line, Some(1));
        assert_eq!(Sung::at(&lyrics, 500.0).line, Some(3));
        assert_eq!(Sung::at(&lyrics, 0.0).anchor(), None);
    }

    #[test]
    fn background_vocals_keep_their_main_line_lit() {
        let lyrics = lyrics();
        // the echo began, the line it belongs to is still being sung: both are lit, the view
        // stays at the main line
        let both = Sung::at(&lyrics, 14.5);
        assert_eq!(both, Sung { line: Some(2), under: Some(1) });
        assert_eq!(both.anchor(), Some(1));
        assert!(both.has(1) && both.has(2) && !both.has(0));
        // the main line ended: only the echo is left
        let echo = Sung::at(&lyrics, 16.5);
        assert_eq!(echo, Sung { line: Some(2), under: None });
        assert_eq!(echo.anchor(), Some(2));
    }

    #[test]
    fn lyrics_without_timing_have_no_current_line() {
        let lyrics = types::Lyrics {
            lines: vec![Line { text: "quiet harbor".into(), ..Line::default() }],
            timing: Timing::None,
            ..types::Lyrics::default()
        };
        assert_eq!(Sung::at(&lyrics, 30.0), Sung::default());
        assert_eq!(next_change(&lyrics, 30.0), None);
        assert!(!filling(&lyrics, 30.0));
    }

    #[test]
    fn a_word_fills_between_its_beginning_and_its_end() {
        assert_eq!(fill(10.0, 12.0, 9.0), 0.0);
        assert_eq!(fill(10.0, 12.0, 10.0), 0.0);
        assert_eq!(fill(10.0, 12.0, 10.5), 0.25);
        assert_eq!(fill(10.0, 12.0, 11.0), 0.5);
        assert_eq!(fill(10.0, 12.0, 12.0), 1.0);
        assert_eq!(fill(10.0, 12.0, 40.0), 1.0);
        // a word without a length is sung the moment it begins
        assert_eq!(fill(10.0, 10.0, 10.0), 0.0);
        assert_eq!(fill(10.0, 10.0, 10.01), 1.0);
    }

    #[test]
    fn words_without_an_end_last_until_the_next_or_the_line_s_end() {
        let lyrics = lyrics();
        assert_eq!(word_times(&lyrics.lines[1]), vec![(10.0, 11.0), (12.0, 16.0)]);
        let mut open = line(1.0, 3.0, "a b");
        open.words = vec![word(1.0, None, "a "), word(2.0, None, "b")];
        assert_eq!(word_times(&open), vec![(1.0, 2.0), (2.0, 3.0)]);
        open.end = None;
        assert_eq!(word_times(&open), vec![(1.0, 2.0), (2.0, 2.4)]);
    }

    #[test]
    fn frames_are_needed_only_while_a_word_fills() {
        let lyrics = lyrics();
        assert!(!filling(&lyrics, 9.0));
        assert!(filling(&lyrics, 10.5));
        // between two words of the line nothing changes
        assert!(!filling(&lyrics, 11.5));
        assert!(filling(&lyrics, 13.0));
        // the echo is over, the main line under it still fills
        assert!(filling(&lyrics, 15.2));
        assert!(!filling(&lyrics, 17.0));
        let mut by_line = lyrics.clone();
        by_line.timing = Timing::Line;
        assert!(!filling(&by_line, 10.5));
    }

    #[test]
    fn the_timer_waits_for_what_changes_next() {
        let lyrics = lyrics();
        assert_eq!(next_change(&lyrics, 0.0), Some(4.0));
        assert_eq!(next_change(&lyrics, 5.0), Some(10.0));
        // the next word of the line that is sung comes before the next line
        assert_eq!(next_change(&lyrics, 11.5), Some(12.0));
        assert_eq!(next_change(&lyrics, 12.5), Some(14.0));
        // under background vocals the main line's end is a change too
        assert_eq!(next_change(&lyrics, 15.6), Some(16.0));
        assert_eq!(next_change(&lyrics, 16.5), Some(20.0));
        assert_eq!(next_change(&lyrics, 21.0), None);
    }

    #[test]
    fn lines_light_up_and_dim_over_a_moment() {
        assert_eq!(level(true, true, 0.3), 1.0);
        assert_eq!(level(false, false, 0.3), DIM);
        assert_eq!(level(true, false, 0.0), DIM);
        assert_eq!(level(true, false, 1.0), 1.0);
        assert_eq!(level(false, true, 0.0), 1.0);
        assert!((level(false, true, 0.5) - (1.0 + DIM) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn the_text_grows_with_the_view() {
        assert_eq!(text_size(600.0), 34.0);
        assert_eq!(text_size(850.0), 34.0);
        assert!((text_size(1000.0) - 40.0).abs() < 1e-4);
        assert!((text_size(2000.0) - 80.0).abs() < 1e-4);
    }

    #[test]
    fn the_view_scrolls_the_sung_line_to_its_place() {
        // four lines of 40 pixels, 20 apart, in a view of 600 with the anchor 200 below its top
        let flow = Flow::new(&[40.0; 4], &[20.0; 4], 600.0, Some(200.0), 60.0);
        assert_eq!(flow.tops, vec![200.0, 260.0, 320.0, 380.0]);
        // before the first line and at the first line the view is at its beginning
        assert_eq!(flow.target(None, 600.0), 0.0);
        assert_eq!(flow.target(Some(0), 600.0), 0.0);
        // each line is scrolled up to the anchor, the last one too
        assert_eq!(flow.target(Some(2), 600.0), 120.0);
        assert_eq!(flow.target(Some(3), 600.0), 180.0);
        assert_eq!(flow.extent, 780.0);
        // a line that does not exist leaves the view at its beginning
        assert_eq!(flow.target(Some(9), 600.0), 0.0);
    }

    #[test]
    fn text_without_timing_scrolls_from_its_top_to_its_bottom() {
        let flow = Flow::new(&[40.0, 90.0, 40.0], &[0.0, 20.0, 20.0], 150.0, None, 30.0);
        assert_eq!(flow.tops, vec![30.0, 90.0, 200.0]);
        assert_eq!(flow.extent, 270.0);
        assert_eq!(flow.target(Some(2), 150.0), 0.0);
        // text shorter than the view fills it and does not scroll
        assert_eq!(Flow::new(&[40.0], &[0.0], 400.0, None, 30.0).extent, 400.0);
        assert_eq!(Flow::new(&[], &[], 400.0, Some(140.0), 30.0).extent, 400.0);
    }

    #[test]
    fn lines_are_found_by_where_they_are() {
        let flow = Flow::new(&[40.0, 90.0, 40.0], &[0.0, 20.0, 20.0], 150.0, None, 30.0);
        assert_eq!(flow.line_at(10.0), None);
        assert_eq!(flow.line_at(30.0), Some(0));
        assert_eq!(flow.line_at(69.9), Some(0));
        // between two lines there is none
        assert_eq!(flow.line_at(80.0), None);
        assert_eq!(flow.line_at(179.0), Some(1));
        assert_eq!(flow.line_at(239.0), Some(2));
        assert_eq!(flow.line_at(260.0), None);
        assert_eq!(flow.visible(0.0, 150.0), 0..2);
        assert_eq!(flow.visible(100.0, 250.0), 1..3);
        assert_eq!(flow.visible(250.0, 400.0), 2..3);
        assert_eq!(Flow::default().visible(0.0, 100.0), 0..0);
    }

    #[test]
    fn lines_fade_toward_the_edges() {
        // a view of 800 whose upper 50 lie under the header bar, fading over 40 pixels
        let fade = |top: f32, bottom: f32| edge_fade(top, bottom, 800.0, 50.0, 40.0);
        assert_eq!(fade(300.0, 340.0), 1.0);
        assert_eq!(fade(90.0, 130.0), 1.0);
        assert_eq!(fade(70.0, 110.0), 0.5);
        assert_eq!(fade(50.0, 90.0), 0.0);
        assert_eq!(fade(20.0, 60.0), 0.0);
        assert_eq!(fade(720.0, 760.0), 1.0);
        assert_eq!(fade(740.0, 780.0), 0.5);
        assert_eq!(fade(770.0, 810.0), 0.0);
    }

    #[test]
    fn a_word_lights_up_from_where_it_begins() {
        let stretch =
            |x: f32, y: f32, width: f32| Stretch { x, y, width, height: 30.0, baseline: y + 24.0, backwards: false };
        let lit = |stretches: &[Stretch], part: f32| lit(stretches, part).collect::<Vec<_>>();
        let word = [stretch(100.0, 0.0, 80.0)];
        assert_eq!(lit(&word, 0.0), vec![]);
        assert_eq!(lit(&word, 0.25), vec![(100.0, 0.0, 20.0, 30.0)]);
        assert_eq!(lit(&word, 1.0), vec![(100.0, 0.0, 80.0, 30.0)]);
        // a word broken across two rows fills the first row, then the second
        let broken = [stretch(300.0, 0.0, 60.0), stretch(0.0, 30.0, 40.0)];
        assert_eq!(lit(&broken, 0.25), vec![(300.0, 0.0, 25.0, 30.0)]);
        assert_eq!(lit(&broken, 0.75), vec![(300.0, 0.0, 60.0, 30.0), (0.0, 30.0, 15.0, 30.0)]);
        // text that runs from right to left fills from the right
        let backwards = [Stretch { backwards: true, ..stretch(100.0, 0.0, 80.0) }];
        assert_eq!(lit(&backwards, 0.25), vec![(160.0, 0.0, 20.0, 30.0)]);
    }

    #[test]
    fn the_light_has_a_front_while_a_word_is_sung() {
        let stretch =
            |x: f32, y: f32, width: f32| Stretch { x, y, width, height: 30.0, baseline: y + 24.0, backwards: false };
        let word = [stretch(100.0, 0.0, 80.0)];
        // before the word begins and after it ends there is no front
        assert_eq!(front(&word, 0.0), None);
        assert_eq!(front(&word, 1.0), None);
        // the front is where the lit part ends, with the rest of the word beyond it
        assert_eq!(front(&word, 0.25), Some((120.0, 0.0, 30.0, 60.0, false)));
        assert_eq!(front(&word, 0.75), Some((160.0, 0.0, 30.0, 20.0, false)));
        // in a word broken across rows the front is on the row the light has reached
        let broken = [stretch(300.0, 0.0, 60.0), stretch(0.0, 30.0, 40.0)];
        assert_eq!(front(&broken, 0.25), Some((325.0, 0.0, 30.0, 35.0, false)));
        assert_eq!(front(&broken, 0.75), Some((15.0, 30.0, 30.0, 25.0, false)));
        // from right to left the front moves to the left
        let backwards = [Stretch { backwards: true, ..stretch(100.0, 0.0, 80.0) }];
        assert_eq!(front(&backwards, 0.25), Some((160.0, 0.0, 30.0, 60.0, true)));
    }

    #[test]
    fn what_is_lit_side_by_side_is_one_part() {
        let mut parts = Vec::new();
        join(&mut parts, (0.0, 0.0, 50.0, 30.0));
        join(&mut parts, (50.0, 0.0, 30.0, 30.0));
        assert_eq!(parts, vec![(0.0, 0.0, 80.0, 30.0)]);
        // the next row is a part of its own, and so is a word after a gap
        join(&mut parts, (0.0, 30.0, 20.0, 30.0));
        join(&mut parts, (40.0, 30.0, 20.0, 30.0));
        assert_eq!(parts.len(), 3);
        // from right to left the parts grow to the left
        let mut parts = vec![(200.0, 0.0, 50.0, 30.0)];
        join(&mut parts, (170.0, 0.0, 30.0, 30.0));
        assert_eq!(parts, vec![(170.0, 0.0, 80.0, 30.0)]);
    }

    #[test]
    fn the_glide_starts_fast_and_ends_slowly() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!(ease(0.25) > 0.5 && ease(0.5) > 0.85);
        assert_eq!(ease(7.0), 1.0);
    }

    #[test]
    fn a_word_grows_while_it_is_sung_and_settles_after() {
        // a word sung for a second, from 10 to 11
        assert_eq!(zoom(10.0, 11.0, 9.0), 0.0);
        assert_eq!(zoom(10.0, 11.0, 10.0), 0.0);
        // it grows as it begins, slowly at first and at the end
        assert!(zoom(10.0, 11.0, 10.02) < 0.1);
        assert!((zoom(10.0, 11.0, 10.0 + RISE / 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(zoom(10.0, 11.0, 10.0 + RISE), 1.0);
        // it stays so while it is sung, and a moment after because it was held
        assert_eq!(zoom(10.0, 11.0, 10.9), 1.0);
        assert_eq!(zoom(10.0, 11.0, 11.05), 1.0);
        // then it settles back
        let settles = 11.0 + (1.0 - HELD) * 0.25;
        assert!((zoom(10.0, 11.0, settles + SETTLE / 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(zoom(10.0, 11.0, settled(10.0, 11.0)), 0.0);
        assert_eq!(zoom(10.0, 11.0, 30.0), 0.0);
    }

    #[test]
    fn a_word_grows_and_settles_without_a_jump() {
        // a very short word, a quick one, a held one, a long note
        for (begin, end) in [(10.0, 10.05), (10.0, 10.3), (10.0, 11.0), (10.0, 14.0)] {
            let mut before = 0.0;
            for n in 0..5000 {
                let now = zoom(begin, end, 9.9 + n as f64 * 0.001);
                assert!((now - before).abs() < 0.01, "{begin} to {end}, at {n} ms");
                before = now;
            }
            // whatever its length, a word grows all the way
            assert_eq!(zoom(begin, end, settled(begin, end) - SETTLE), 1.0);
        }
    }

    #[test]
    fn a_held_word_stays_grown_a_little_longer() {
        // a short word settles once it has grown, a quick one as it ends
        assert_eq!(settled(10.0, 10.05), 10.0 + RISE + SETTLE);
        assert_eq!(settled(10.0, 10.4), 10.4 + SETTLE);
        // a held one a moment after its end, the longer the longer it was held, up to a limit
        assert!((settled(10.0, 11.0) - (11.1 + SETTLE)).abs() < 1e-9);
        assert!((settled(10.0, 11.5) - (11.725 + SETTLE)).abs() < 1e-9);
        assert!((settled(10.0, 20.0) - (20.0 + HOLD + SETTLE)).abs() < 1e-9);
    }

    #[test]
    fn a_growing_word_passes_through_few_sizes() {
        assert_eq!(step(1.0), 1.0);
        assert_eq!(step(1.0 + STEP * 0.4), 1.0);
        assert_eq!(step(1.0 + STEP * 0.6), 1.0 + STEP);
        // whatever the moment, the scale is one of the steps: their glyphs are drawn once
        let sizes: std::collections::BTreeSet<u32> =
            (0..=1000).map(|n| step(1.0 + (ZOOM - 1.0) * n as f32 / 1000.0).to_bits()).collect();
        assert!(sizes.len() <= ((ZOOM - 1.0) / STEP).round() as usize + 1, "{}", sizes.len());
    }

    #[test]
    fn a_word_grows_into_its_own_room_only() {
        // room enough on both sides: as large as words get, from its middle
        assert_eq!(growth(100.0, 20.0, 20.0), (ZOOM, 50.0));
        // less room: the room on both sides together sets the scale
        let (most, origin) = growth(100.0, 2.0, 2.0);
        assert!((most - 1.04).abs() < 1e-6 && origin == 50.0);
        // no room on its left (the first word of a row): it grows to the right only
        assert_eq!(growth(100.0, 0.0, 30.0), (ZOOM, 0.0));
        // otherwise it grows from as near its middle as its room allows, and never beyond it
        for (before, after) in [(1.0, 30.0), (30.0, 1.0), (3.0, 5.0), (0.0, 0.0), (0.5, 0.5)] {
            let (most, origin) = growth(100.0, before, after);
            assert!((1.0..=ZOOM).contains(&most) && (0.0..=100.0).contains(&origin));
            assert!(origin * (most - 1.0) <= before + 1e-4, "{before} {after}");
            assert!((100.0 - origin) * (most - 1.0) <= after + 1e-4, "{before} {after}");
        }
        assert_eq!(growth(100.0, 0.0, 0.0).0, 1.0);
    }

    #[test]
    fn syllables_grow_as_one_word_into_their_part_of_the_space() {
        let stretch =
            |x: f32, y: f32, width: f32| Stretch { x, y, width, height: 40.0, baseline: y + 30.0, backwards: false };
        // one word sung in three syllables without a space between them, a word after a space,
        // and a word on the next row
        let pieces = [
            (stretch(0.0, 0.0, 60.0), 1.0, 1.2),
            (stretch(60.0, 0.0, 20.0), 1.2, 1.4),
            (stretch(80.0, 0.0, 40.0), 1.4, 1.9),
            (stretch(132.0, 0.0, 50.0), 2.0, 2.5),
            (stretch(0.0, 40.0, 70.0), 2.6, 3.0),
        ];
        let rooms = rooms(&pieces, 300.0);
        assert_eq!(rooms.len(), 3);
        let (word, after, below) = (rooms[0], rooms[1], rooms[2]);
        assert_eq!((word.x, word.width, word.begin, word.end), (0.0, 120.0, 1.0, 1.9));
        // the space between two words is shared at its middle
        assert_eq!((word.left, word.right, after.left, after.right), (0.0, 126.0, 126.0, 300.0));
        // the first word of the row grows to the right only, into part of its half of the space
        assert_eq!(word.origin, 0.0);
        assert!((word.most - (1.0 + 6.0 * SHARE / 120.0)).abs() < 1e-6);
        // the last word has the rest of the row: it grows as far as words do, from its middle
        assert_eq!((after.most, after.origin), (ZOOM, 157.0));
        assert_eq!((below.y, below.baseline, below.left, below.right), (40.0, 70.0, 0.0, 300.0));
        // the syllables grow together, from the first one's beginning until the last one's end
        assert_eq!(word.grown(0.5), (1.0, 0.0));
        let (scale, lift) = word.grown(1.0 + RISE);
        assert!((scale - word.most).abs() <= STEP / 2.0 && (lift - LIFT * 40.0).abs() < 1e-5);
        assert!(word.moves(1.5) && word.moves(settled(1.0, 1.9) - 0.01));
        assert!(!word.moves(1.0) && !word.moves(settled(1.0, 1.9)));
        assert_eq!(word.grown(settled(1.0, 1.9)), (1.0, 0.0));
    }

    #[test]
    fn a_line_is_drawn_in_parts_around_its_growing_words() {
        let room = |x: f32, y: f32, left: f32, right: f32| Room {
            x,
            width: right - left - 10.0,
            y,
            height: 40.0,
            baseline: y + 30.0,
            left,
            right,
            origin: x + 20.0,
            most: 1.05,
            begin: 0.0,
            end: 1.0,
        };
        // a line of two rows 300 wide: three words on the first, one on the second
        let rooms = [
            room(0.0, 0.0, 0.0, 100.0),
            room(105.0, 0.0, 100.0, 200.0),
            room(205.0, 0.0, 200.0, 300.0),
            room(0.0, 40.0, 0.0, 300.0),
        ];
        let drawn = |grown: &dyn Fn(&Room) -> (f32, f32)| {
            let mut drawn = Vec::new();
            parts(&rooms, grown, 300.0, 80.0, 10.0, |part| drawn.push(part));
            drawn
        };
        // nothing grows: the line is one part
        assert_eq!(drawn(&|_| (1.0, 0.0)), vec![Part { area: (-10.0, -10.0, 320.0, 100.0), grown: None }]);
        // the middle word grows: its room is a part of its own, and the rest is drawn around it
        let grown = Grown { scale: 1.05, x: 125.0, y: 30.0, lift: 2.0 };
        assert_eq!(
            drawn(&|room: &Room| if room.x == 105.0 { (1.05, 2.0) } else { (1.0, 0.0) }),
            vec![
                Part { area: (-10.0, -10.0, 110.0, 50.0), grown: None },
                Part { area: (100.0, -10.0, 100.0, 50.0), grown: Some(grown) },
                Part { area: (200.0, -10.0, 110.0, 50.0), grown: None },
                Part { area: (-10.0, 40.0, 320.0, 50.0), grown: None },
            ]
        );
        // the first word of the first row and the word below grow: their rooms reach the margins
        let parts = drawn(&|room: &Room| if room.x == 0.0 { (1.02, 1.0) } else { (1.0, 0.0) });
        let areas: Vec<_> = parts.iter().map(|part| (part.area, part.grown.is_some())).collect();
        assert_eq!(
            areas,
            vec![
                ((-10.0, -10.0, 110.0, 50.0), true),
                ((100.0, -10.0, 210.0, 50.0), false),
                ((-10.0, 40.0, 320.0, 50.0), true),
            ]
        );
    }
}
