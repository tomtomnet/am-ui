//! Apple Music links: what a link pasted into the search field points at. No network.
//!
//! `parse_music_link(text)` recognises the links music.apple.com hands out (also its beta., geo.
//! and embed. hosts and the old itunes.apple.com form), whatever their storefront:
//!
//! ```text
//! https://music.apple.com/gb/album/<slug>/1234567800?i=1234567890     song 1234567890 (album 1234567800)
//! https://music.apple.com/us/song/<slug>/1234567890                   song
//! https://music.apple.com/gb/album/<slug>/1234567800                  album
//! https://music.apple.com/gb/playlist/<slug>/pl.u-xxxxxxx             playlist
//! https://music.apple.com/gb/artist/<slug>/5000003                    artist
//! https://itunes.apple.com/us/album/<slug>/id1234567800?i=1234567890  song
//! music.apple.com/gb/album/x/1234567800   (pasted without its scheme) album
//! ```
//!
//! Stations, music videos, curators and search pages are not links the application opens. The
//! rules are those of the Python application's `app/links.py`, down to how Python's `urlsplit`
//! and `parse_qs` read a URL.

const HOSTS: [&str; 6] = [
    "music.apple.com",
    "beta.music.apple.com",
    "geo.music.apple.com",
    "embed.music.apple.com",
    "itunes.apple.com",
    "geo.itunes.apple.com",
];

/// Characters that read as `/ ? # @ :` once Unicode's compatibility forms are resolved ("℀" is
/// "a/c", "：" is ":"). Python refuses a URL whose host part has one, and so does this.
const LOOKALIKES: [char; 19] = [
    '\u{2047}', '\u{2048}', '\u{2049}', '\u{2100}', '\u{2101}', '\u{2105}', '\u{2106}', '\u{2a74}', '\u{fe13}',
    '\u{fe16}', '\u{fe55}', '\u{fe56}', '\u{fe5f}', '\u{fe6b}', '\u{ff03}', '\u{ff0f}', '\u{ff1a}', '\u{ff1f}',
    '\u{ff20}',
];

/// What a link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Song,
    Album,
    Playlist,
    Artist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: Kind,
    /// The catalog id of the song, album, playlist or artist.
    pub id: String,
    /// The two-letter storefront the link names, empty when it names none. Nothing uses it yet:
    /// the catalog is asked in the account's own storefront.
    #[allow(dead_code)]
    pub storefront: String,
    /// The album of an album link, and of a song link that goes through its album (`?i=`).
    /// Nothing uses it yet: a song link plays the song.
    #[allow(dead_code)]
    pub album_id: Option<String>,
}

/// The link in `text`, when the whole of it is an Apple Music link to a song, an album, a
/// playlist or an artist.
pub fn parse_music_link(text: &str) -> Option<Link> {
    let text = strip(text);
    if text.is_empty() || text.contains([' ', '\n']) {
        return None;
    }
    let url = if has_scheme(text) {
        text.to_string()
    } else if HOSTS.iter().any(|host| starts_with_ignoring_case(text, host)) {
        format!("https://{text}")
    } else {
        return None;
    };
    // tabs and line ends inside a URL do not count
    let url: String = url.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let rest = rest.strip_prefix("//")?;
    let (authority, rest) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    if authority.contains(['[', ']']) || authority.contains(LOOKALIKES) {
        return None;
    }
    // the host: after a user name, before a port
    let host = authority.rsplit('@').next()?.split(':').next()?;
    if !HOSTS.iter().any(|known| host.eq_ignore_ascii_case(known)) {
        return None;
    }
    let rest = rest.split('#').next()?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut storefront = "";
    if parts.first().is_some_and(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_lowercase())) {
        storefront = parts.remove(0);
    }
    let kind = parts.first()?.to_ascii_lowercase();
    let tail = *parts.last()?;
    let link = |kind: Kind, id: &str, album_id: Option<&str>| {
        Some(Link {
            kind,
            id: id.to_string(),
            storefront: storefront.to_string(),
            album_id: album_id.map(str::to_owned),
        })
    };
    // `?i=` picks one song of an album
    if matches!(kind.as_str(), "album" | "song" | "music-video")
        && let Some(song) = query_value(query, "i").filter(|value| numeric(value))
    {
        return link(Kind::Song, &song, if kind == "album" { item_id(tail) } else { None });
    }
    match kind.as_str() {
        "song" => link(Kind::Song, item_id(tail)?, None),
        "album" => {
            let album = item_id(tail)?;
            link(Kind::Album, album, Some(album))
        }
        "artist" => link(Kind::Artist, item_id(tail)?, None),
        "playlist" => link(Kind::Playlist, parts[1..].iter().rev().find(|part| playlist_id(part))?, None),
        _ => None,
    }
}

/// Without the white space around it, by Python's idea of white space (`str.strip`).
fn strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// `scheme://` at the start.
fn has_scheme(text: &str) -> bool {
    let Some((scheme, _)) = text.split_once("://") else { return false };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.as_bytes().get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Three to twenty digits: the id of a song, an album or an artist.
fn numeric(text: &str) -> bool {
    (3..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// "1234567800", or the old form "id1234567800".
fn item_id(segment: &str) -> Option<&str> {
    let segment = strip(segment);
    if starts_with_ignoring_case(segment, "id") && numeric(&segment[2..]) {
        return Some(&segment[2..]);
    }
    numeric(segment).then_some(segment)
}

/// "pl.u-xxxxxxx", "pl.0123abcd…": the id of a catalog playlist.
fn playlist_id(segment: &str) -> bool {
    segment.strip_prefix("pl.").is_some_and(|rest| {
        rest.len() >= 4 && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// The first value of `name` in a query string; a field without a value does not count.
fn query_value(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|field| field.split_once('='))
        .filter(|(_, value)| !value.is_empty())
        .find(|(field, _)| unquote(field) == name)
        .map(|(_, value)| unquote(value))
}

/// A name or value of a query string as it was meant: `+` is a space, `%41` is "A".
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let hex = |at: usize| bytes.get(at).and_then(|byte| (*byte as char).to_digit(16));
    let mut plain = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match (bytes[at], hex(at + 1), hex(at + 2)) {
            (b'%', Some(high), Some(low)) => {
                plain.push((high * 16 + low) as u8);
                at += 3;
            }
            (b'+', _, _) => {
                plain.push(b' ');
                at += 1;
            }
            (byte, _, _) => {
                plain.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&plain).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: Kind, id: &str, storefront: &str, album_id: Option<&str>) -> Option<Link> {
        Some(Link { kind, id: id.into(), storefront: storefront.into(), album_id: album_id.map(str::to_owned) })
    }

    #[test]
    fn songs() {
        // one song of an album
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/album/amber-harbor/1234567800?i=1234567890"),
            link(Kind::Song, "1234567890", "gb", Some("1234567800"))
        );
        // a song of its own
        assert_eq!(
            parse_music_link("https://music.apple.com/us/song/velvet-orbit/1234567890"),
            link(Kind::Song, "1234567890", "us", None)
        );
        // the old form
        assert_eq!(
            parse_music_link("https://itunes.apple.com/us/album/quiet-signal/id1234567800?i=1234567890"),
            link(Kind::Song, "1234567890", "us", Some("1234567800"))
        );
        // `i` among other fields, encoded, and only its first value
        assert_eq!(
            parse_music_link("https://music.apple.com/de/album/x/1234567800?l=en&i=1234567890&i=5550001#top"),
            link(Kind::Song, "1234567890", "de", Some("1234567800"))
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/de/album/x/1234567800?%69=%31234567890"),
            link(Kind::Song, "1234567890", "de", Some("1234567800"))
        );
        // a song picked on a page that is not an album
        assert_eq!(
            parse_music_link("https://music.apple.com/us/song/x/5550001?i=1234567890"),
            link(Kind::Song, "1234567890", "us", None)
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/us/music-video/x/5550001?i=1234567890"),
            link(Kind::Song, "1234567890", "us", None)
        );
    }

    #[test]
    fn albums_playlists_artists() {
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/album/paper-lantern/1234567800"),
            link(Kind::Album, "1234567800", "gb", Some("1234567800"))
        );
        assert_eq!(
            parse_music_link("https://itunes.apple.com/us/album/x/id1000005"),
            link(Kind::Album, "1000005", "us", Some("1000005"))
        );
        assert_eq!(
            parse_music_link("music.apple.com/jp/album/1234567800/"),
            link(Kind::Album, "1234567800", "jp", Some("1234567800"))
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/playlist/honey-mix/pl.u-aBc123XyZ"),
            link(Kind::Playlist, "pl.u-aBc123XyZ", "gb", None)
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/playlist/x/pl.0123456789abcdef0123456789abcdef"),
            link(Kind::Playlist, "pl.0123456789abcdef0123456789abcdef", "gb", None)
        );
        // the id of a playlist is looked for from the end of the path
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/playlist/pl.u-first/pl.u-second/more"),
            link(Kind::Playlist, "pl.u-second", "gb", None)
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/artist/glass-meadow/5000003"),
            link(Kind::Artist, "5000003", "gb", None)
        );
        // an `i` that is not a song id leaves an album link an album link
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/album/x/1234567800?i=abc"),
            link(Kind::Album, "1234567800", "gb", Some("1234567800"))
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/album/x/1234567800?i="),
            link(Kind::Album, "1234567800", "gb", Some("1234567800"))
        );
        // a fragment is not a query
        assert_eq!(
            parse_music_link("https://music.apple.com/gb/album/x/1234567800#?i=1234567890"),
            link(Kind::Album, "1234567800", "gb", Some("1234567800"))
        );
    }

    #[test]
    fn any_storefront_or_none() {
        for storefront in ["us", "gb", "jp", "br", "za"] {
            let found = parse_music_link(&format!("https://music.apple.com/{storefront}/artist/x/5000003"));
            assert_eq!(found, link(Kind::Artist, "5000003", storefront, None));
        }
        assert_eq!(
            parse_music_link("https://music.apple.com/artist/x/5000003"),
            link(Kind::Artist, "5000003", "", None)
        );
        // a storefront is two small letters: anything else is read as the kind of page
        assert_eq!(parse_music_link("https://music.apple.com/US/artist/x/5000003"), None);
        assert_eq!(parse_music_link("https://music.apple.com/usa/artist/x/5000003"), None);
    }

    #[test]
    fn hosts_and_schemes() {
        let hosts = [
            "music.apple.com",
            "beta.music.apple.com",
            "geo.music.apple.com",
            "embed.music.apple.com",
            "itunes.apple.com",
            "geo.itunes.apple.com",
        ];
        for host in hosts {
            for prefix in ["https://", "http://", ""] {
                let found = parse_music_link(&format!("{prefix}{host}/us/album/x/1234567800"));
                assert_eq!(found, link(Kind::Album, "1234567800", "us", Some("1234567800")), "{prefix}{host}");
            }
        }
        // capitals in the scheme, the host and the kind; a port; white space around the link
        assert_eq!(
            parse_music_link("  HTTPS://Music.Apple.COM:443/us/ALBUM/x/1234567800\n"),
            link(Kind::Album, "1234567800", "us", Some("1234567800"))
        );
        assert_eq!(
            parse_music_link("Music.Apple.com/us/song/x/ID1234567890"),
            link(Kind::Song, "1234567890", "us", None)
        );
        for other in [
            "https://example.com/us/album/x/1234567800",
            "https://music.apple.com.example.com/us/album/x/1234567800",
            "https://example.com/music.apple.com/us/album/x/1234567800",
            "music.apple.community/us/album/x/1234567800",
            "https://podcasts.apple.com/us/album/x/1234567800",
            "ftp://music.apple.com/us/album/x/1234567800",
            "music://music.apple.com/us/album/x/1234567800",
            "//music.apple.com/us/album/x/1234567800",
            "https:music.apple.com/us/album/x/1234567800",
            "https://[music.apple.com]/us/album/x/1234567800",
            "https:///us/album/x/1234567800",
        ] {
            assert_eq!(parse_music_link(other), None, "{other}");
        }
    }

    #[test]
    fn ids_are_numeric_or_playlist_ids() {
        for other in [
            "https://music.apple.com/us/album/x/abc",
            "https://music.apple.com/us/album/x/l.AbCdEfG",
            "https://music.apple.com/us/album/x/12",
            "https://music.apple.com/us/album/x/123456789012345678901",
            "https://music.apple.com/us/album/x/1234567800a",
            "https://music.apple.com/us/album/x/idx1234567800",
            "https://music.apple.com/us/album",
            "https://music.apple.com/us/song/x/pl.u-aBc123XyZ",
            "https://music.apple.com/us/artist/x/",
            "https://music.apple.com/us/playlist/x/1234567800",
            "https://music.apple.com/us/playlist/x/pl.abc",
            "https://music.apple.com/us/playlist/x/PL.u-aBc123XyZ",
            "https://music.apple.com/us/playlist/x/pl.u-aBc123XyZ!",
            "https://music.apple.com/us/playlist/x/p.AbCdEfGhIj",
        ] {
            assert_eq!(parse_music_link(other), None, "{other}");
        }
        assert_eq!(
            parse_music_link("https://music.apple.com/us/album/x/123"),
            link(Kind::Album, "123", "us", Some("123"))
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/us/album/x/12345678901234567890"),
            link(Kind::Album, "12345678901234567890", "us", Some("12345678901234567890"))
        );
        assert_eq!(
            parse_music_link("https://music.apple.com/us/playlist/x/pl.abcd"),
            link(Kind::Playlist, "pl.abcd", "us", None)
        );
    }

    #[test]
    fn what_is_not_a_link() {
        for other in [
            "",
            "   ",
            "amber",
            "amber harbor",
            "music.apple.com",
            "https://music.apple.com",
            "https://music.apple.com/us",
            "https://music.apple.com/us/browse",
            "https://music.apple.com/us/station/x/ra.1234567800",
            "https://music.apple.com/us/music-video/x/1234567800",
            "https://music.apple.com/us/curator/x/1234567800",
            "https://music.apple.com/us/search?term=amber",
            "https://music.apple.com/us/album/x/1234567800 and more",
            "see https://music.apple.com/us/album/x/1234567800",
            "https://music.apple.com/us/album/x/1234567800\nhttps://music.apple.com/us/album/x/5550001",
        ] {
            assert_eq!(parse_music_link(other), None, "{other:?}");
        }
    }
}
