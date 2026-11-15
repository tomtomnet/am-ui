//! Values as the Python command line printed them.
//!
//! Several lines of the commands carry a value straight from a state file or a summary:
//! `token file present: True`, `variant: {'audio_group': ..., 'codecs': 'ec-3'}`,
//! `duration_s: 201.386667`, `silent channels: ['LFE']`. They keep Python's spelling, so the
//! output of both command lines reads the same.

use std::time::Duration;

use serde_json::Value;

use crate::util::python_repr;

/// Python's `True` / `False`.
pub fn truth(flag: bool) -> &'static str {
    if flag { "True" } else { "False" }
}

/// Python's `repr()` of a float: the shortest text that reads back as the same number, with a
/// decimal point (`201.0`) or, below 1e-4 and from 1e16 on, an exponent of at least two digits
/// (`1e-05`, `1.5e+16`).
pub fn float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..16).contains(&exponent) {
        let plain = format!("{value}");
        if plain.contains('.') { plain } else { format!("{plain}.0") }
    } else {
        format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    }
}

/// Python's `repr()` of a value read from JSON: text in quotes, `None`, `True`, a list or a
/// dictionary of such.
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(flag) => truth(*flag).to_string(),
        Value::Number(number) => match number.as_f64() {
            Some(real) if number.is_f64() => float(real),
            _ => number.to_string(),
        },
        Value::String(text) => python_repr(text),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(members) => {
            let pairs = members.iter().map(|(key, value)| format!("{}: {}", python_repr(key), repr(value)));
            format!("{{{}}}", pairs.collect::<Vec<_>>().join(", "))
        }
    }
}

/// Python's `str()` of a value read from JSON, as an f-string prints it: text as it is, anything
/// else as [`repr`] has it; `None` for a value that is not there.
pub fn shown(value: Option<&Value>) -> String {
    match value {
        None => "None".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => repr(other),
    }
}

/// Python's truth value of a value read from JSON; false for one that is not there.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(members)) => !members.is_empty(),
    }
}

/// `2026-01-02 03:04` in this machine's time zone, for a time in seconds since the Unix epoch
/// (Python: `time.strftime("%Y-%m-%d %H:%M", time.localtime(seconds))`).
pub fn local_minute(seconds: f64) -> String {
    let whole = Duration::try_from_secs_f64(seconds.max(0.0)).map_or(0, |since| since.as_secs());
    let whole = libc::time_t::try_from(whole).unwrap_or(0);
    let mut parts = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid for the call and `localtime_r` writes only through the
    // second; `tm` is integers and a pointer, for which all zeros is a value, so it is
    // initialised either way.
    let parts = unsafe {
        if libc::localtime_r(&whole, parts.as_mut_ptr()).is_null() {
            return format!("@{whole}");
        }
        parts.assume_init()
    };
    minute(parts.tm_year + 1900, parts.tm_mon + 1, parts.tm_mday, parts.tm_hour, parts.tm_min)
}

fn minute(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> String {
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Reference values: `repr()` of the same floats in Python.
    #[test]
    fn floats_are_spelled_as_python_spells_them() {
        for (value, text) in [
            (201.386667, "201.386667"),
            (201.0, "201.0"),
            (0.5, "0.5"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (-11.0, "-11.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (0.000015, "1.5e-05"),
            (1e100, "1e+100"),
            (1e-100, "1e-100"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(float(value), text);
        }
        assert_eq!(float(f64::NAN), "nan");
    }

    /// Reference values: `str()` and `repr()` of the same values in Python, after `json.loads`.
    #[test]
    fn values_are_spelled_as_python_spells_them() {
        let variant = json!({"audio_group": "audio-atmos-2768", "codecs": "ec-3", "bandwidth": "772001"});
        assert_eq!(
            shown(Some(&variant)),
            "{'audio_group': 'audio-atmos-2768', 'codecs': 'ec-3', 'bandwidth': '772001'}"
        );
        let lossless = json!({"audio_group": "audio-alac-stereo-48000-24", "bandwidth": null, "sample_rate": 48000,
                              "bit_depth": 24});
        assert_eq!(
            repr(&lossless),
            "{'audio_group': 'audio-alac-stereo-48000-24', 'bandwidth': None, 'sample_rate': 48000, 'bit_depth': 24}"
        );
        for (value, as_str, as_repr) in [
            (json!(null), "None", "None"),
            (json!(true), "True", "True"),
            (json!(false), "False", "False"),
            (json!(0), "0", "0"),
            (json!(-7), "-7", "-7"),
            (json!(18446744073709551615u64), "18446744073709551615", "18446744073709551615"),
            (json!(2.0), "2.0", "2.0"),
            (json!(1e-7), "1e-07", "1e-07"),
            (json!("Song A"), "Song A", "'Song A'"),
            (json!("it's"), "it's", "\"it's\""),
            (json!(""), "", "''"),
            (json!([]), "[]", "[]"),
            (json!({}), "{}", "{}"),
            (json!(["LFE", "TFL"]), "['LFE', 'TFL']", "['LFE', 'TFL']"),
            (
                json!([null, 1, 1.5, [true], {"a": "b"}]),
                "[None, 1, 1.5, [True], {'a': 'b'}]",
                "[None, 1, 1.5, [True], {'a': 'b'}]",
            ),
        ] {
            assert_eq!(shown(Some(&value)), as_str, "{value}");
            assert_eq!(repr(&value), as_repr, "{value}");
        }
        assert_eq!(shown(None), "None");
        assert_eq!((truth(true), truth(false)), ("True", "False"));
    }

    #[test]
    fn truth_values() {
        for value in [json!(null), json!(false), json!(0), json!(0.0), json!(""), json!([]), json!({})] {
            assert!(!truthy(Some(&value)), "{value}");
        }
        for value in [json!(true), json!(1), json!(-0.5), json!("0"), json!([0]), json!({"a": null})] {
            assert!(truthy(Some(&value)), "{value}");
        }
        assert!(!truthy(None));
    }

    #[test]
    fn minutes() {
        assert_eq!(minute(2026, 1, 2, 3, 4), "2026-01-02 03:04");
        assert_eq!(minute(2026, 11, 15, 23, 59), "2026-11-15 23:59");
        // whatever the time zone of the machine: the form, and a minute for every moment
        let stamp = local_minute(1_767_323_045.9);
        let digits: String = stamp.chars().map(|c| if c.is_ascii_digit() { '0' } else { c }).collect();
        assert_eq!(digits, "0000-00-00 00:00");
        assert_eq!(local_minute(f64::NAN).len(), 16);
        assert_eq!(local_minute(-5.0), local_minute(0.0));
    }
}
