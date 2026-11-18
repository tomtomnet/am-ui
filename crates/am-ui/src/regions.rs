//! Apple Music's regions ("storefronts") with their names, for Preferences > Library > Catalog
//! Region, and how one is found among them by what is typed. The codes are ISO 3166-1 alpha-2 in
//! lower case.

use crate::sorting::search_text;

/// (code, name), by code.
pub const REGIONS: [(&str, &str); 135] = [
    ("ae", "United Arab Emirates"),
    ("ag", "Antigua and Barbuda"),
    ("ai", "Anguilla"),
    ("am", "Armenia"),
    ("ar", "Argentina"),
    ("at", "Austria"),
    ("au", "Australia"),
    ("az", "Azerbaijan"),
    ("bb", "Barbados"),
    ("be", "Belgium"),
    ("bg", "Bulgaria"),
    ("bh", "Bahrain"),
    ("bm", "Bermuda"),
    ("bo", "Bolivia"),
    ("br", "Brazil"),
    ("bs", "Bahamas"),
    ("bw", "Botswana"),
    ("by", "Belarus"),
    ("bz", "Belize"),
    ("ca", "Canada"),
    ("ch", "Switzerland"),
    ("cl", "Chile"),
    ("cn", "China mainland"),
    ("co", "Colombia"),
    ("cr", "Costa Rica"),
    ("cv", "Cabo Verde"),
    ("cy", "Cyprus"),
    ("cz", "Czechia"),
    ("de", "Germany"),
    ("dk", "Denmark"),
    ("dm", "Dominica"),
    ("do", "Dominican Republic"),
    ("dz", "Algeria"),
    ("ec", "Ecuador"),
    ("ee", "Estonia"),
    ("eg", "Egypt"),
    ("es", "Spain"),
    ("fi", "Finland"),
    ("fj", "Fiji"),
    ("fr", "France"),
    ("gb", "United Kingdom"),
    ("gd", "Grenada"),
    ("gh", "Ghana"),
    ("gm", "Gambia"),
    ("gr", "Greece"),
    ("gt", "Guatemala"),
    ("gy", "Guyana"),
    ("hk", "Hong Kong"),
    ("hn", "Honduras"),
    ("hr", "Croatia"),
    ("hu", "Hungary"),
    ("id", "Indonesia"),
    ("ie", "Ireland"),
    ("il", "Israel"),
    ("in", "India"),
    ("is", "Iceland"),
    ("it", "Italy"),
    ("jm", "Jamaica"),
    ("jo", "Jordan"),
    ("jp", "Japan"),
    ("ke", "Kenya"),
    ("kg", "Kyrgyzstan"),
    ("kh", "Cambodia"),
    ("kn", "Saint Kitts and Nevis"),
    ("kr", "South Korea"),
    ("kw", "Kuwait"),
    ("ky", "Cayman Islands"),
    ("kz", "Kazakhstan"),
    ("la", "Laos"),
    ("lb", "Lebanon"),
    ("lc", "Saint Lucia"),
    ("lk", "Sri Lanka"),
    ("lt", "Lithuania"),
    ("lu", "Luxembourg"),
    ("lv", "Latvia"),
    ("md", "Moldova"),
    ("mk", "North Macedonia"),
    ("ml", "Mali"),
    ("mn", "Mongolia"),
    ("mo", "Macao"),
    ("ms", "Montserrat"),
    ("mt", "Malta"),
    ("mu", "Mauritius"),
    ("mw", "Malawi"),
    ("mx", "Mexico"),
    ("my", "Malaysia"),
    ("mz", "Mozambique"),
    ("na", "Namibia"),
    ("ne", "Niger"),
    ("ng", "Nigeria"),
    ("ni", "Nicaragua"),
    ("nl", "Netherlands"),
    ("no", "Norway"),
    ("np", "Nepal"),
    ("nz", "New Zealand"),
    ("om", "Oman"),
    ("pa", "Panama"),
    ("pe", "Peru"),
    ("pg", "Papua New Guinea"),
    ("ph", "Philippines"),
    ("pk", "Pakistan"),
    ("pl", "Poland"),
    ("pt", "Portugal"),
    ("py", "Paraguay"),
    ("qa", "Qatar"),
    ("ro", "Romania"),
    ("rs", "Serbia"),
    ("ru", "Russia"),
    ("sa", "Saudi Arabia"),
    ("se", "Sweden"),
    ("sg", "Singapore"),
    ("si", "Slovenia"),
    ("sk", "Slovakia"),
    ("sn", "Senegal"),
    ("sv", "El Salvador"),
    ("th", "Thailand"),
    ("tj", "Tajikistan"),
    ("tm", "Turkmenistan"),
    ("tn", "Tunisia"),
    ("tr", "Türkiye"),
    ("tt", "Trinidad and Tobago"),
    ("tw", "Taiwan"),
    ("tz", "Tanzania"),
    ("ua", "Ukraine"),
    ("ug", "Uganda"),
    ("us", "United States"),
    ("uy", "Uruguay"),
    ("uz", "Uzbekistan"),
    ("vc", "Saint Vincent and the Grenadines"),
    ("ve", "Venezuela"),
    ("vg", "British Virgin Islands"),
    ("vn", "Vietnam"),
    ("ye", "Yemen"),
    ("za", "South Africa"),
    ("zw", "Zimbabwe"),
];

/// What people write for a region instead of its code.
const OTHER_CODES: [(&str, &str); 3] = [("uk", "gb"), ("usa", "us"), ("uae", "ae")];

/// The name of a region, or its code in capitals when the list does not have it.
pub fn name(code: &str) -> String {
    REGIONS.iter().find(|(known, _)| *known == code).map_or_else(|| code.to_uppercase(), |(_, name)| name.to_string())
}

/// How well a region fits what was typed (the lower the better); `None`: not at all. `wanted` and
/// `name` are in the form a search compares (`sorting::search_text`).
fn rank(wanted: &str, code: &str, name: &str) -> Option<u8> {
    let other_code = OTHER_CODES.iter().any(|(other, region)| *other == wanted && *region == code);
    if wanted == code || other_code {
        Some(0)
    } else if name.starts_with(wanted) {
        Some(1)
    } else if name.split(' ').any(|word| word.starts_with(wanted)) {
        Some(2)
    } else if name.contains(wanted) {
        Some(3)
    } else {
        None
    }
}

/// The regions that fit `term`, the best first: the region with that code, those whose name
/// begins with it, those with a word that begins with it, those whose name contains it, each in
/// the order of the names. Capitals and accents make no difference. An empty term: all of them.
pub fn search(term: &str) -> Vec<(&'static str, &'static str)> {
    let wanted = search_text(term);
    let mut found: Vec<(u8, String, &'static str, &'static str)> = REGIONS
        .iter()
        .filter_map(|(code, name)| {
            let key = search_text(name);
            let rank = if wanted.is_empty() { Some(0) } else { rank(&wanted, code, &key) };
            rank.map(|rank| (rank, key, *code, *name))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, _, code, name)| (code, name)).collect()
}

/// Whether `term` asks for the account's own region (the entry that is called "Account").
pub fn means_account(term: &str) -> bool {
    "account".starts_with(&search_text(term))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(term: &str) -> Vec<&'static str> {
        search(term).into_iter().map(|(code, _)| code).collect()
    }

    #[test]
    fn the_code_of_a_region_finds_it_first() {
        assert_eq!(codes("us")[0], "us");
        assert_eq!(codes("US")[0], "us");
        assert_eq!(codes(" gb ")[0], "gb");
        assert_eq!(codes("de")[0], "de");
        // after it what else fits: names that begin with the letters, then those that hold them
        let us = codes("us");
        assert!(us.contains(&"au") && us.contains(&"ru"), "{us:?}");
        let position = |code: &str| us.iter().position(|found| *found == code).unwrap();
        // "Belarus", "Cyprus", "Russia"... all hold the letters: in the order of their names
        assert!(position("by") < position("cy") && position("cy") < position("ru"));
        // what people write instead of the code
        assert_eq!(codes("uk")[0], "gb");
        assert_eq!(codes("uk")[1], "ua");
        assert_eq!(codes("usa")[0], "us");
        assert_eq!(codes("uae")[0], "ae");
    }

    #[test]
    fn names_are_found_from_their_beginning_first() {
        // the name begins with it, a later word begins with it, the name contains it
        let found = codes("ma");
        let position = |code: &str| found.iter().position(|found| *found == code).unwrap();
        assert!(position("my") < position("ml"), "Malaysia before Mali, in the order of the names");
        assert!(position("ml") < position("mt") && position("mt") < position("mu"));
        // no code is "ma": what begins with the letters comes first, what holds them last
        assert_eq!(rank("ma", "my", "malaysia"), Some(1));
        assert_eq!(rank("ma", "dm", "dominica"), None);
        assert_eq!(rank("ma", "dk", "denmark"), Some(3));
        assert_eq!(rank("arab", "ae", "united arab emirates"), Some(2));
        assert_eq!(codes("arab"), ["sa", "ae"]);
        assert_eq!(codes("united"), ["ae", "gb", "us"]);
        assert!(codes("zzz").is_empty());
    }

    #[test]
    fn capitals_and_accents_make_no_difference() {
        assert_eq!(codes("TÜRKIYE"), ["tr"]);
        assert_eq!(codes("turkiye"), ["tr"]);
        assert_eq!(codes("türk"), ["tr", "tm"]);
        assert_eq!(codes("ÉTATS").len(), 0);
        assert_eq!(codes("Cabo")[0], "cv");
    }

    #[test]
    fn without_a_term_every_region_is_there_by_name() {
        let all = search("");
        assert_eq!(all.len(), REGIONS.len());
        assert_eq!(all[0], ("dz", "Algeria"));
        assert!(all.windows(2).all(|pair| search_text(pair[0].1) <= search_text(pair[1].1)));
        assert_eq!(all.last(), Some(&("zw", "Zimbabwe")));
    }

    #[test]
    fn the_accounts_entry_is_found_by_its_word() {
        assert!(means_account("") && means_account("acc") && means_account("Account"));
        assert!(!means_account("us") && !means_account("accounts"));
    }
}
