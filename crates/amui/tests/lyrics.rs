//! Tests for `amui::lyrics` (synthetic TTML only; no network, no real lyric text).
//!
//! The first three modules are tests/test_lyrics.py, test for test and under the same names
//! (`ParseTimeTest` -> `parse_time_test`, `WordTimingTest` -> `word_timing_test`,
//! `LineTimingTest` -> `line_timing_test`). `reference` compares the port with what the Python
//! parser returned for a wider set of documents.

use amui::lyrics::{LyricsError, parse_time, parse_ttml};
use amui::types::{Line, Lyrics, Timing, Word};
use serde_json::{Value, json};

const NS: &str = r##"xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:itunes="http://music.apple.com/lyric-ttml-internal""##;

/// Word timing, three divs, two agents, a background span inside a line, a whole-background line,
/// syllable spans without separators, and untimed text around a word.
fn word_ttml() -> String {
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<tt {NS} xml:lang="en" itunes:timing="Word">
 <head><metadata>
  <ttm:agent type="person" xml:id="v1"/><ttm:agent type="person" xml:id="v2"/>
  <iTunesMetadata xmlns="http://music.apple.com/lyric-ttml-internal" leadingSilence="0.5"><translations/></iTunesMetadata>
 </metadata></head>
 <body dur="1:00.000">
  <div begin="0:01.000" end="0:10.000" itunes:songPart="Verse">
   <p begin="0:01.000" end="0:04.000" itunes:key="L1" ttm:agent="v1"><span begin="0:01.000" end="0:01.500">Hello</span> <span begin="0:01.500" end="0:02.000">there,</span> <span begin="0:02.000" end="0:02.400">won</span><span begin="0:02.400" end="0:03.000">der</span><span begin="0:03.000" end="0:04.000">ful</span></p>
   <p begin="0:04.500" end="0:08.000" itunes:key="L2" ttm:agent="v2"><span begin="0:04.500" end="0:05.000">Second</span> <span begin="0:05.000" end="0:06.000">line</span> <span ttm:role="x-bg"><span begin="0:06.000" end="0:07.000">(ooh</span> <span begin="0:07.000" end="0:08.000">ooh)</span></span></p>
  </div>
  <div begin="0:10.000" end="0:20.000" itunes:songPart="Chorus">
   <p begin="0:10.000" end="0:12.000" itunes:key="L3" ttm:agent="v1"><span begin="0:10.000" end="0:11.000">Chorus</span>
      <span begin="0:11.000" end="0:12.000">words</span></p>
   <p begin="0:12.000" end="0:14.000" itunes:key="L4" ttm:agent="v1" ttm:role="x-bg"><span begin="0:12.000" end="0:13.000">(all</span> <span begin="0:13.000" end="0:14.000">background)</span></p>
  </div>
  <div begin="0:20.000" end="0:22.000" itunes:songPart="Outro">
   <p begin="0:20.000" end="0:22.000" itunes:key="L5">(<span begin="0:20.000" end="0:21.000">yeah</span>)</p>
  </div>
 </body>
</tt>
"##
    )
}

/// Line timing: text directly inside `<p>`, an untimed span, a dur attribute, a
/// whitespace-only paragraph.
fn line_ttml() -> String {
    format!(
        r##"<tt {NS} xml:lang="de" itunes:timing="Line">
<body><div begin="0.5" end="9" itunes:songPart="Verse">
<p begin="0.500" end="3.000" itunes:key="L1">Première ligne</p>
<p begin="3.000" end="6.000" itunes:key="L2" ttm:agent="v1">Deuxième   ligne
   avec <span>un span</span> sans timing</p>
<p begin="6.000" dur="2.500" itunes:key="L3">Troisième</p>
<p begin="8.5" end="9">   </p>
</div></body></tt>
"##
    )
}

fn untimed_ttml() -> String {
    format!(
        r##"<tt {NS} xml:lang="en" itunes:timing="None">
<body><div><p>First untimed</p><p>Second untimed</p></div></body></tt>
"##
    )
}

fn no_attribute_line_ttml() -> String {
    format!(r##"<tt {NS}><body><div><p begin="1" end="2">One</p><p begin="2" end="3">Two</p></div></body></tt>"##)
}

fn no_attribute_word_ttml() -> String {
    format!(
        r##"<tt {NS}><body><div><p begin="1" end="3"><span begin="1" end="2">One</span> <span begin="2" end="3">two</span></p></div></body></tt>"##
    )
}

fn overclaim_word_ttml() -> String {
    format!(r##"<tt {NS} itunes:timing="Word"><body><div><p begin="1" end="2">No spans here</p></div></body></tt>"##)
}

fn overclaim_line_ttml() -> String {
    format!(r##"<tt {NS} itunes:timing="Line"><body><div><p>No times here</p></div></body></tt>"##)
}

const MINIMAL_TTML: &str = "<tt><body><div><p begin='1' end='2'>x</p></div></body></tt>";

fn texts(lyrics: &Lyrics) -> Vec<&str> {
    lyrics.lines.iter().map(|line| line.text.as_str()).collect()
}

fn word_texts(line: &Line) -> Vec<&str> {
    line.words.iter().map(|word| word.text.as_str()).collect()
}

mod parse_time_test {
    use super::*;

    #[test]
    fn formats() {
        let cases = [
            ("12.345", 12.345),
            ("1:02.5", 62.5),
            ("0:01:02.500", 62.5),
            ("1.5s", 1.5),
            ("500ms", 0.5),
            ("2m", 120.0),
            ("12", 12.0),
            ("1h", 3600.0),
            (" 3.25 ", 3.25),
            ("00:00.000", 0.0),
            ("9.999", 9.999),
            ("12:34.567", 754.567),
            ("1,5", 1.5),
        ];
        for (text, expected) in cases {
            let seconds = parse_time(text).unwrap();
            assert!((seconds - expected).abs() < 5e-8, "{text}: {seconds}");
        }
    }

    /// The Python test also passes `None`; a `&str` cannot be anything but text.
    #[test]
    fn malformed() {
        for text in ["", "   ", "abc", "1:2:3:4", "-1", "1e3", "1:x", "1.5.2", "5 s", "::"] {
            assert!(parse_time(text).is_err(), "{text:?}");
        }
    }
}

mod word_timing_test {
    use super::*;

    fn lyrics() -> Lyrics {
        parse_ttml(&word_ttml(), "syllable-lyrics").unwrap()
    }

    #[test]
    fn document() {
        let lyrics: Lyrics = lyrics();
        assert_eq!(lyrics.timing, Timing::Word);
        assert_eq!(lyrics.timing.as_str(), "word");
        assert_eq!(lyrics.language.as_deref(), Some("en"));
        assert_eq!(lyrics.source, "syllable-lyrics");
        // L1, L2, L2's background, L3, L4 (background paragraph), L5
        assert_eq!(
            texts(&lyrics),
            ["Hello there, wonderful", "Second line", "(ooh ooh)", "Chorus words", "(all background)", "(yeah)"]
        );
        let backgrounds: Vec<bool> = lyrics.lines.iter().map(|line| line.background).collect();
        assert_eq!(backgrounds, [false, false, true, false, true, false]);
    }

    #[test]
    fn words_and_spacing() {
        let lyrics = lyrics();
        let first = &lyrics.lines[0];
        assert_eq!(first.begin, Some(1.0));
        assert_eq!(first.end, Some(4.0));
        assert_eq!(first.agent.as_deref(), Some("v1"));
        assert_eq!(word_texts(first), ["Hello ", "there, ", "won", "der", "ful"]);
        let times: Vec<(f64, Option<f64>)> = first.words.iter().map(|word: &Word| (word.begin, word.end)).collect();
        assert_eq!(times, [(1.0, Some(1.5)), (1.5, Some(2.0)), (2.0, Some(2.4)), (2.4, Some(3.0)), (3.0, Some(4.0))]);
    }

    #[test]
    fn join_invariant() {
        for line in lyrics().lines.iter().filter(|line| !line.words.is_empty()) {
            assert_eq!(word_texts(line).concat(), line.text);
        }
    }

    #[test]
    fn background_span_becomes_own_line() {
        let lyrics = lyrics();
        let (main, background) = (&lyrics.lines[1], &lyrics.lines[2]);
        assert_eq!(main.text, "Second line");
        assert_eq!(word_texts(main), ["Second ", "line"]);
        assert_eq!(main.agent.as_deref(), Some("v2"));
        assert!(background.background);
        assert_eq!(background.agent.as_deref(), Some("v2")); // inherited
        assert_eq!((background.begin, background.end), (Some(6.0), Some(8.0))); // from its words
        assert_eq!(word_texts(background), ["(ooh ", "ooh)"]);
    }

    #[test]
    fn whole_background_paragraph() {
        let lyrics = lyrics();
        let line = &lyrics.lines[4];
        assert!(line.background);
        assert_eq!(line.agent.as_deref(), Some("v1"));
        assert_eq!((line.begin, line.end), (Some(12.0), Some(14.0)));
        assert_eq!(line.words.len(), 2);
    }

    #[test]
    fn newline_between_words_is_one_space() {
        let lyrics = lyrics();
        let line = &lyrics.lines[3];
        assert_eq!(line.text, "Chorus words");
        assert_eq!(word_texts(line), ["Chorus ", "words"]);
    }

    #[test]
    fn untimed_text_around_word_sticks_to_it() {
        let lyrics = lyrics();
        let line = &lyrics.lines[5];
        assert_eq!(line.text, "(yeah)");
        assert_eq!(word_texts(line), ["(yeah)"]);
        assert_eq!(line.words[0].begin, 20.0);
    }

    #[test]
    fn index_at_and_to_dict() {
        let lyrics = lyrics();
        assert_eq!(lyrics.index_at(0.5), -1);
        assert_eq!(lyrics.index_at(1.0), 0);
        assert_eq!(lyrics.index_at(5.0), 1);
        assert_eq!(lyrics.index_at(11.0), 3);
        let data = lyrics.to_json();
        assert_eq!(data["timing"], "word");
        assert_eq!(data["lines"].as_array().unwrap().len(), 6);
        assert_eq!(data["lines"][0]["words"][0]["text"], "Hello ");
    }
}

mod line_timing_test {
    use super::*;

    #[test]
    fn lines() {
        let lyrics = parse_ttml(&line_ttml(), "lyrics").unwrap();
        assert_eq!(lyrics.timing, Timing::Line);
        assert_eq!(lyrics.language.as_deref(), Some("de"));
        assert_eq!(lyrics.source, "lyrics");
        assert_eq!(lyrics.lines.len(), 3); // the whitespace-only paragraph is dropped
        assert_eq!(texts(&lyrics), ["Première ligne", "Deuxième ligne avec un span sans timing", "Troisième"]);
        assert!(lyrics.lines.iter().all(|line| line.words.is_empty()));
        assert_eq!(lyrics.lines[1].agent.as_deref(), Some("v1"));
        assert_eq!(lyrics.lines[0].agent, None);
        assert_eq!((lyrics.lines[2].begin, lyrics.lines[2].end), (Some(6.0), Some(8.5))); // dur
        assert!(!lyrics.lines.iter().any(|line| line.background));
    }

    #[test]
    fn untimed() {
        let lyrics = parse_ttml(&untimed_ttml(), "lyrics").unwrap();
        assert_eq!(lyrics.timing, Timing::None);
        assert_eq!(texts(&lyrics), ["First untimed", "Second untimed"]);
        assert!(lyrics.lines.iter().all(|line| line.begin.is_none() && line.end.is_none()));
        assert_eq!(lyrics.index_at(100.0), -1);
    }

    #[test]
    fn timing_inferred_without_attribute() {
        assert_eq!(parse_ttml(&no_attribute_line_ttml(), "lyrics").unwrap().timing, Timing::Line);
        assert_eq!(parse_ttml(&no_attribute_word_ttml(), "lyrics").unwrap().timing, Timing::Word);
    }

    #[test]
    fn timing_attribute_downgraded_when_data_lacks_it() {
        assert_eq!(parse_ttml(&overclaim_word_ttml(), "lyrics").unwrap().timing, Timing::Line);
        assert_eq!(parse_ttml(&overclaim_line_ttml(), "lyrics").unwrap().timing, Timing::None);
    }

    #[test]
    fn minimal_unnamespaced() {
        let lyrics = parse_ttml(MINIMAL_TTML, "lyrics").unwrap();
        assert_eq!(lyrics.lines.len(), 1);
        let line = &lyrics.lines[0];
        assert_eq!((line.begin, line.end, line.text.as_str()), (Some(1.0), Some(2.0), "x"));
        assert_eq!(lyrics.language, None);
    }

    #[test]
    fn leading_bom_and_whitespace() {
        let lyrics = parse_ttml(&format!("\u{feff}\n  {MINIMAL_TTML}"), "lyrics").unwrap();
        assert_eq!(lyrics.lines.len(), 1);
    }

    /// The Python test also passes `None`; a `&str` cannot be anything but text.
    #[test]
    fn malformed_documents() {
        assert!(parse_ttml("<tt>", "lyrics").is_err());
        assert!(parse_ttml(r#"<!DOCTYPE tt [<!ENTITY a "b">]><tt><body><p>&a;</p></body></tt>"#, "lyrics").is_err());
    }

    #[test]
    fn empty_document() {
        let lyrics = parse_ttml("<tt><body/></tt>", "lyrics").unwrap();
        assert_eq!(lyrics.lines, []);
        assert_eq!(lyrics.timing, Timing::None);
    }

    #[test]
    fn line_dataclass_shape() {
        let line: Line = parse_ttml(MINIMAL_TTML, "lyrics").unwrap().lines.remove(0);
        assert_eq!(
            line.to_json(),
            json!({"begin": 1.0, "end": 2.0, "text": "x", "words": [], "background": false, "agent": null})
        );
    }
}

/// Beyond the Python tests: the port against the Python parser itself.
mod reference {
    use super::*;

    /// fixtures/lyrics_reference.json: synthetic documents and time values with what
    /// amui/lyrics.py returned for each (`Lyrics.to_dict()`, seconds, or the error text).
    fn reference() -> Value {
        serde_json::from_str(include_str!("fixtures/lyrics_reference.json")).unwrap()
    }

    #[test]
    fn documents_parse_as_in_python() {
        let reference = reference();
        let documents = reference["documents"].as_array().unwrap();
        assert!(documents.len() > 40);
        for document in documents {
            let name = document["name"].as_str().unwrap();
            let parsed = parse_ttml(document["ttml"].as_str().unwrap(), "lyrics");
            match document.get("error") {
                Some(message) => assert_eq!(parsed.unwrap_err().to_string(), message.as_str().unwrap(), "{name}"),
                None => assert_eq!(parsed.unwrap().to_json(), document["lyrics"], "{name}"),
            }
        }
    }

    #[test]
    fn times_parse_as_in_python() {
        let reference = reference();
        let times = reference["times"].as_array().unwrap();
        assert!(times.len() > 50);
        for time in times {
            let text = time["text"].as_str().unwrap();
            match time.get("error") {
                Some(message) => {
                    assert_eq!(parse_time(text).unwrap_err().to_string(), message.as_str().unwrap(), "{text:?}")
                }
                None => assert_eq!(parse_time(text).unwrap(), time["seconds"].as_f64().unwrap(), "{text:?}"),
            }
        }
    }

    /// The differences the module's top comment lists.
    #[test]
    fn stricter_than_python_on_purpose() {
        // digits of other scripts: Python read these as 12 and 62 seconds
        assert_eq!(parse_time("١٢"), Err(LyricsError::MalformedTime("١٢".into())));
        assert_eq!(parse_time("١:02"), Err(LyricsError::MalformedClock("١:02".into())));
        // a clock part that ends in a line feed passed Python's `$`
        assert_eq!(parse_time("1\n:02"), Err(LyricsError::MalformedClock("1\n:02".into())));
    }

    #[test]
    fn errors_name_no_lyrics_text() {
        let broken = "<tt><body><p begin='1'>placeholder words</p><p>more</body></tt>";
        let error = parse_ttml(broken, "lyrics").unwrap_err();
        assert_eq!(error, LyricsError::NotWellFormed);
        assert_eq!(error.to_string(), "TTML document is not well-formed XML");
        assert_eq!(format!("{error:?}"), "NotWellFormed");
        let declared = parse_ttml("<!DOCTYPE tt><tt><body><p>placeholder words</p></body></tt>", "lyrics").unwrap_err();
        assert_eq!(declared.to_string(), "TTML with a DTD or entity declarations is not accepted");
        // a malformed time inside a document is not an error at all: the line is untimed
        let lyrics = parse_ttml("<tt><body><p begin='soon'>placeholder words</p></body></tt>", "lyrics").unwrap();
        assert_eq!((lyrics.lines[0].begin, lyrics.timing), (None, Timing::None));
    }
}
