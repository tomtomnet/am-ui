//! Apple TTML lyrics -> timed lines/words.
//!
//! Apple's TTML: `<tt xmlns:itunes="http://music.apple.com/lyric-ttml-internal"
//! itunes:timing="Word|Line|None"> <body><div><p begin="12.345" end="15.1" ttm:agent="v1">
//! <span begin end>word</span> ... <span ttm:role="x-bg">...</span> </p>`. Times: "mm:ss.mmm",
//! "ss.mmm", "h:mm:ss.mmm", "1.5s", "500ms".
//!
//! What the live documents look like (checked 2026-11-07 against the web API):
//!
//! * Line timing (`itunes:timing="Line"`): every `<p>` carries begin/end and holds its text
//!   directly (no spans). Optional `ttm:agent` on `<p>`.
//! * Word timing ("Word"): every `<p>` carries begin/end; words are `<span begin end>` children.
//!   White space between words is literal text (" ") after the span; syllables of one word are
//!   adjacent spans with no white space between them. Background vocals are an *untimed*
//!   `<span ttm:role="x-bg">` that wraps timed child spans, placed at the end of the `<p>`.
//! * `<div>` elements carry begin/end/itunes:songPart; the `<p>` times are absolute (Apple's
//!   player treats them that way, so no parent-offset arithmetic here).
//! * The `<head>` lists `ttm:agent` elements (xml:id, type) and an iTunesMetadata element
//!   (leadingSilence); neither is needed for display.
//!
//! Contract notes (see research/engine-api.md):
//!
//! * `Line::text` is white-space-normalised and excludes background spans.
//! * `Word::text` keeps the untimed text that follows the word up to the next word (normally a
//!   single space, sometimes nothing for syllables or a punctuation tail), so the texts of a
//!   line's words concatenate to `Line::text` for every line with words. The UI can therefore lay
//!   words out by concatenation.
//! * Background spans become their own `Line` (`background: true`) placed right after the line
//!   they belong to, inheriting that line's agent.
//! * `Lyrics::timing` follows `itunes:timing` when present but is downgraded when the document
//!   does not actually contain that granularity (a "Word" document with no timed spans reports
//!   `Timing::Line`); without the attribute it is inferred.
//!
//! Lyrics text never goes into an error or a log: the errors below carry a time value at most.
//!
//! Deliberate differences from amui/lyrics.py:
//!
//! * Errors are [`LyricsError`] values with the texts of Python's `ValueError`s. The two checks
//!   for arguments that are not strings have no counterpart (the types rule them out), and
//!   [`parse_ttml`] has no default for `source` (Python: "lyrics").
//! * The XML parser is `roxmltree` instead of expat. Both are namespace-aware and refuse what is
//!   not well-formed; on the fine print of XML that Apple's documents never touch they may judge
//!   a document differently.
//! * Numbers are ASCII digits. Python's `\d` and `float()` also took the decimal digits of other
//!   scripts ("١٢" was 12 seconds), and its `$` let a clock part end in a line feed; both are
//!   malformed time values here.
//! * White space is what Rust calls white space (Unicode `White_Space`); Python adds U+001C to
//!   U+001F, which XML documents cannot contain.

use roxmltree::{Document, Node};

use crate::types::{Line, Lyrics, Timing, Word};
use crate::util::python_repr;

pub const TT_NS: &str = "http://www.w3.org/ns/ttml";
pub const TTM_NS: &str = "http://www.w3.org/ns/ttml#metadata";
pub const TTP_NS: &str = "http://www.w3.org/ns/ttml#parameter";
pub const ITUNES_NS: &str = "http://music.apple.com/lyric-ttml-internal";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Seconds per unit of the offset-time forms ("ms" is tried before "s").
const UNITS: [(&str, f64); 4] = [("ms", 0.001), ("h", 3600.0), ("m", 60.0), ("s", 1.0)];

/// A time value or a document that cannot be read (Python: `ValueError`, same texts). A message
/// names the offending time value and never any other document content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LyricsError {
    #[error("empty time value")]
    EmptyTime,
    #[error("unsupported clock value: {}", python_repr(.0))]
    UnsupportedClock(String),
    #[error("malformed clock value: {}", python_repr(.0))]
    MalformedClock(String),
    #[error("malformed time value: {}", python_repr(.0))]
    MalformedTime(String),
    #[error("TTML with a DTD or entity declarations is not accepted")]
    Dtd,
    #[error("TTML document is not well-formed XML")]
    NotWellFormed,
}

/// `\d+`
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// `\d+(?:[.,]\d+)?`
fn is_number(text: &str) -> bool {
    match text.split_once(['.', ',']) {
        Some((whole, fraction)) => is_digits(whole) && is_digits(fraction),
        None => is_digits(text),
    }
}

/// The value of text [`is_number`] accepted (a decimal comma counts as a point).
fn number(text: &str) -> Option<f64> {
    text.replace(',', ".").parse().ok()
}

/// TTML clock value -> seconds.
///
/// Accepts "ss.mmm", "m:ss.mmm", "h:mm:ss.mmm", bare integers, and the offset-time forms "1.5s",
/// "500ms", "2m", "1h". Frame-based clock values ("hh:mm:ss:ff") are rejected because the frame
/// rate is not known here.
pub fn parse_time(value: &str) -> Result<f64, LyricsError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(LyricsError::EmptyTime);
    }
    if text.contains(':') {
        let parts: Vec<&str> = text.split(':').collect();
        if !(2..=3).contains(&parts.len()) {
            return Err(LyricsError::UnsupportedClock(text.to_string()));
        }
        let malformed = || LyricsError::MalformedClock(text.to_string());
        let well_formed =
            parts.split_last().is_some_and(|(last, first)| first.iter().all(|p| is_digits(p)) && is_number(last));
        if !well_formed {
            return Err(malformed());
        }
        let mut total = 0.0;
        for part in parts {
            total = total * 60.0 + number(part).ok_or_else(malformed)?;
        }
        return Ok(total);
    }
    let (digits, scale) = UNITS
        .iter()
        .find_map(|(unit, scale)| text.strip_suffix(unit).map(|digits| (digits, *scale)))
        .unwrap_or((text, 1.0));
    let malformed = || LyricsError::MalformedTime(text.to_string());
    if !is_number(digits) {
        return Err(malformed());
    }
    Ok(number(digits).ok_or_else(malformed)? * scale)
}

// --- element helpers --------------------------------------------------------

fn is_named(node: Node, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

/// Attribute by local name: the namespaced form first, then the plain form (lenient for
/// hand-made TTML).
fn attr<'a>(element: Node<'a, '_>, name: &str, namespace: Option<&str>) -> Option<&'a str> {
    if let Some(namespace) = namespace
        && let Some(value) = element.attribute((namespace, name))
    {
        return Some(value);
    }
    // not `element.attribute(name)`: roxmltree would take the attribute of any namespace for it
    element
        .attributes()
        .find(|attribute| attribute.namespace().is_none() && attribute.name() == name)
        .map(|a| a.value())
}

fn is_background(element: Node) -> bool {
    attr(element, "role", Some(TTM_NS)).unwrap_or("").split_whitespace().any(|role| role == "x-bg")
}

/// (begin, end) in seconds from begin/end/dur; malformed values count as absent.
fn times(element: Node) -> (Option<f64>, Option<f64>) {
    let (mut begin, mut end) = (None, None);
    let (raw_begin, raw_end, raw_dur) =
        (attr(element, "begin", None), attr(element, "end", None), attr(element, "dur", None));
    // as in Python, the first malformed value ends the reading: what was read before it stays
    let mut read = || -> Result<(), LyricsError> {
        if let Some(raw) = raw_begin {
            begin = Some(parse_time(raw)?);
        }
        if let Some(raw) = raw_end {
            end = Some(parse_time(raw)?);
        } else if let (Some(raw), Some(begin)) = (raw_dur, begin) {
            end = Some(begin + parse_time(raw)?);
        }
        Ok(())
    };
    let _ = read();
    (begin, end)
}

/// Every run of white space as one space.
fn collapse(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if !character.is_whitespace() {
            collapsed.push(character);
        } else if !in_space {
            collapsed.push(' ');
        }
        in_space = character.is_whitespace();
    }
    collapsed
}

fn normalise(text: &str) -> String {
    collapse(text).trim().to_string()
}

// --- line assembly ----------------------------------------------------------

/// (begin, end) of a timed span.
type Timed = (f64, Option<f64>);

/// Flattens one `<p>` (or one background span) into text tokens, words and nested background
/// lines.
#[derive(Default)]
struct Collector<'a, 'input> {
    /// Document-order text, with its times when the text is a timed span (a word).
    tokens: Vec<(String, Option<Timed>)>,
    backgrounds: Vec<Node<'a, 'input>>,
}

impl<'a, 'input> Collector<'a, 'input> {
    fn walk(&mut self, element: Node<'a, 'input>, inside_background: bool) {
        for child in element.children() {
            if child.is_text() {
                // the element's own text and the text that follows each of its children
                if let Some(text) = child.text().filter(|text| !text.is_empty()) {
                    self.tokens.push((text.to_string(), None));
                }
            } else if is_named(child, "span") {
                if is_background(child) && !inside_background {
                    self.backgrounds.push(child);
                } else {
                    self.span(child, inside_background);
                }
            } else if is_named(child, "br") {
                self.tokens.push((" ".to_string(), None));
            }
            // metadata or unknown children contribute no text, but the text after them still
            // belongs to the line; comments and processing instructions are not there at all
        }
    }

    fn span(&mut self, span: Node<'a, 'input>, inside_background: bool) {
        let (begin, end) = times(span);
        let has_nested_spans = span.children().any(|child| is_named(child, "span"));
        match begin {
            Some(begin) if !has_nested_spans => {
                let text: String =
                    span.descendants().filter(|node| node.is_text()).filter_map(|node| node.text()).collect();
                // without anything to show it is a timed white-space separator
                let word = (!text.trim().is_empty()).then_some((begin, end));
                self.tokens.push((text, word));
            }
            _ => self.walk(span, inside_background),
        }
    }

    fn build(&self, begin: Option<f64>, end: Option<f64>, background: bool, agent: Option<String>) -> Option<Line> {
        let timed: Vec<Timed> = self.tokens.iter().filter_map(|(_, word)| *word).collect();
        let (text, words) = if timed.is_empty() {
            (normalise(&self.tokens.iter().map(|(text, _)| text.as_str()).collect::<String>()), Vec::new())
        } else {
            let pieces = self.word_pieces(timed.len());
            let words = timed.iter().zip(&pieces).map(|(&(begin, end), text)| Word { begin, end, text: text.clone() });
            (pieces.concat(), words.collect())
        };
        if text.is_empty() && words.is_empty() {
            return None;
        }
        let begin = begin.or_else(|| words.first().map(|word| word.begin));
        let end = end.or_else(|| words.iter().filter_map(|word| word.end).reduce(f64::max));
        Some(Line { begin, end, text, words, background, agent })
    }

    /// Per-word text such that the pieces concatenate to the normalised line text.
    fn word_pieces(&self, count: usize) -> Vec<String> {
        let mut pieces = vec![String::new(); count];
        let mut prefix = String::new();
        let mut filled = 0; // words seen so far: the untimed text goes to the last of them
        for (text, word) in &self.tokens {
            if word.is_some() {
                pieces[filled] = std::mem::take(&mut prefix) + text;
                filled += 1;
            } else if filled == 0 {
                prefix.push_str(text);
            } else {
                pieces[filled - 1].push_str(text);
            }
        }
        let mut pieces: Vec<String> = pieces.iter().map(|piece| collapse(piece)).collect();
        for index in 1..pieces.len() {
            if pieces[index - 1].ends_with(' ') && pieces[index].starts_with(' ') {
                pieces[index] = pieces[index].trim_start_matches(' ').to_string();
            }
        }
        if let Some(first) = pieces.first_mut() {
            *first = first.trim_start().to_string();
        }
        if let Some(last) = pieces.last_mut() {
            *last = last.trim_end().to_string();
        }
        pieces
    }
}

fn background_line(
    span: Node,
    parent_begin: Option<f64>,
    parent_end: Option<f64>,
    agent: Option<&str>,
) -> Option<Line> {
    let mut collector = Collector::default();
    collector.walk(span, true);
    let (begin, end) = times(span);
    let agent = attr(span, "agent", Some(TTM_NS)).filter(|own| !own.is_empty()).or(agent);
    let mut line = collector.build(begin, end, true, agent.map(str::to_owned))?;
    if line.begin.is_none() {
        (line.begin, line.end) = (parent_begin, parent_end);
    }
    Some(line)
}

fn paragraph_lines(paragraph: Node) -> Vec<Line> {
    let (begin, end) = times(paragraph);
    let agent = attr(paragraph, "agent", Some(TTM_NS));
    let whole_background = is_background(paragraph);
    let mut collector = Collector::default();
    collector.walk(paragraph, whole_background);
    let main = collector.build(begin, end, whole_background, agent.map(str::to_owned));
    let backgrounds = collector.backgrounds.iter().filter_map(|span| background_line(*span, begin, end, agent));
    main.into_iter().chain(backgrounds).collect()
}

fn timing_label(value: &str) -> Option<Timing> {
    match value.trim().to_lowercase().as_str() {
        "word" => Some(Timing::Word),
        "line" => Some(Timing::Line),
        "none" => Some(Timing::None),
        _ => None,
    }
}

fn rank(timing: Timing) -> u8 {
    match timing {
        Timing::None => 0,
        Timing::Line => 1,
        Timing::Word => 2,
    }
}

/// Parse one TTML document; `source` ("syllable-lyrics" or "lyrics") goes into the result as it
/// is. Lines keep document order; words only when spans carry timing.
///
/// An error when the document is not well-formed XML (the message never includes document
/// content).
pub fn parse_ttml(ttml: &str, source: &str) -> Result<Lyrics, LyricsError> {
    let text = ttml.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    if text.contains("<!ENTITY") || text.contains("<!DOCTYPE") {
        return Err(LyricsError::Dtd);
    }
    let document = Document::parse(text).map_err(|_| LyricsError::NotWellFormed)?;
    let root = document.root_element();

    let body = root.children().find(|child| is_named(*child, "body")).unwrap_or(root);
    let lines: Vec<Line> = body.descendants().filter(|node| is_named(*node, "p")).flat_map(paragraph_lines).collect();

    let has_words = lines.iter().any(|line| !line.words.is_empty());
    let has_line_timing = lines.iter().any(|line| line.begin.is_some());
    let inferred = if has_words {
        Timing::Word
    } else if has_line_timing {
        Timing::Line
    } else {
        Timing::None
    };
    let timing = match attr(root, "timing", Some(ITUNES_NS)).and_then(timing_label) {
        Some(declared) if rank(declared) <= rank(inferred) => declared,
        _ => inferred,
    };

    let stated = |language: &&str| !language.is_empty();
    let language = root.attribute((XML_NS, "lang")).filter(stated).or_else(|| attr(root, "lang", None).filter(stated));
    Ok(Lyrics { lines, timing, language: language.map(str::to_owned), source: source.to_string() })
}
