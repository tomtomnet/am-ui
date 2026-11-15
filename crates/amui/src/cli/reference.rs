//! The commands against what the Python command line printed.
//!
//! `tests/fixtures/cli_reference.json` holds, for synthetic answers of the network and synthetic
//! states of a scratch folder, the exit status and the output of the Python commands, and what
//! the Python helpers return (`tests/reference/cli_reference.py` made it by running the Python
//! engine over a scratch folder and a stand-in network). The tests here serve the same answers,
//! build the same states and compare text for text.
//!
//! What is taken out of the output on both sides: the scratch folders (`<var>`, `<scratch>`),
//! the time the cache listing prints for an entry (`<time>`, which depends on the time zone),
//! and all but the last of the progress lines of a render (how many there are depends on how
//! fast the stand-in renderer is). One thing differs on purpose: a hint names this program
//! (`run: amui acquire ...`) where Python's named `python3 -m amui`.
//!
//! The reference's sink is a table that takes what the downmix tool sets, with the values a
//! `pw-cli` listing shows; here that is the stand-in sink of the `downmix` tests. Its wrapper
//! service is a table too; here that is the stand-in service of the scratch engine. Its renderer
//! is a script of the reference, run by `bash` as the render script of the scratch folder.

use std::fs;
use std::path::Path;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use super::library::{clip, hms, mmss, table};
use super::pipeline::{fmt_bytes, song_id};
use super::testing::{Scratch, reference, reference_network};
use super::text::{repr, shown};
use crate::cache::{ArtCache, MediaCache, PcmCache};
use crate::doctor::{Found, Report};
use crate::lossless::ACCOUNT_FILES;
use crate::web::Source;
use crate::web::testing::{ANDROID_TOKEN, BROWSER_TOKEN, sign_in};

fn text(value: &Value) -> &str {
    value.as_str().expect("a text in the reference file")
}

fn list(value: &Value) -> &Vec<Value> {
    value.as_array().expect("a list in the reference file")
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// Put a scratch engine into the state a group of the reference describes.
fn prepare(scratch: &Scratch, state: &Value) {
    let config = &scratch.config;
    if let Some(script) = state.get("render_script") {
        fs::write(&config.render_script, text(script)).unwrap();
    }
    for signin in state.get("signins").map(list).into_iter().flatten() {
        match text(signin) {
            "android" => sign_in(config, Source::Android, ANDROID_TOKEN),
            _ => sign_in(config, Source::Browser, BROWSER_TOKEN),
        }
    }
    if let Some(status) = state.get("android_status") {
        write(&config.android_session_status_file, text(status).as_bytes());
    }
    if let Some(status) = state.get("browser_status") {
        write(&config.session_status_file, text(status).as_bytes());
    }
    if let Some(route) = state.get("lossless") {
        // a built route, its service as the group has it
        for path in [&config.lossless_rip_helper, &config.lossless_wrapperctl] {
            write(path, b"stand-in\n");
        }
        if route["signed_in"] == true {
            for name in ACCOUNT_FILES {
                write(&config.lossless_data_dir.join(name), b"stand-in\n");
            }
        }
        let regions =
            route["regions"].as_array().map(|regions| regions.iter().map(|id| text(id).to_string()).collect());
        *scratch.route.regions.lock().unwrap() = regions;
        *scratch.route.dispatch.lock().unwrap() = route.get("dispatch").cloned().unwrap_or(json!({}));
    }
    let pause = || std::thread::sleep(Duration::from_millis(10)); // one after the other, as in the reference
    let zeros = |count: &Value| vec![0u8; count.as_u64().expect("a size") as usize];
    let media = MediaCache::new(config.clone());
    for entry in state.get("media").map(list).into_iter().flatten() {
        let source = config.tmp_dir.join("reference-media");
        fs::write(&source, zeros(&entry["bytes"])).unwrap();
        media.put(text(&entry["key"]), &source, entry["meta"].as_object().unwrap()).unwrap();
        pause();
    }
    let pcm = PcmCache::new(config);
    for entry in state.get("pcm").map(list).into_iter().flatten() {
        let directory = pcm.claim(text(&entry["key"])).unwrap();
        for (name, content) in entry["files"].as_object().unwrap() {
            let data = match content.get("zeros") {
                Some(count) => zeros(count),
                None => content["json"].to_string().into_bytes(),
            };
            fs::write(directory.join(name), data).unwrap();
        }
        pause();
    }
    let art = ArtCache::new(config);
    for entry in state.get("art").map(list).into_iter().flatten() {
        art.put(text(&entry["url"]), &zeros(&entry["bytes"])).unwrap();
    }
}

/// Every group of the reference: its state, then its command lines one after the other.
#[test]
fn commands_print_what_the_python_commands_printed() {
    let reference = reference();
    let stamp = Regex::new(r"used \d{4}-\d\d-\d\d \d\d:\d\d").unwrap();
    let progress = Regex::new(r"(?:  t\+ *[0-9.]+s +[0-9]+ frames +[0-9.]+s[^\n]*\n)+").unwrap();
    let mut compared = 0;
    for group in list(&reference["groups"]) {
        // the reference's engine has one render script for all its groups, in its scratch folder
        let scratch = Scratch::with(&[("AMUI_RENDER_SCRIPT", "{scratch}/render.sh")], reference_network());
        prepare(&scratch, &group["state"]);
        let (var, folder) = (scratch.config.var.display().to_string(), scratch.dir.path().display().to_string());
        let cleaned = |printed: &str| {
            let printed = printed.replace(&var, "<var>").replace(&folder, "<scratch>");
            let printed = stamp.replace_all(&printed, "used <time>");
            let last = |lines: &regex::Captures| format!("{}\n", lines[0].lines().last().unwrap_or_default());
            progress.replace_all(&printed, last).into_owned()
        };
        for case in list(&group["cases"]) {
            let argv: Vec<&str> = list(&case["argv"]).iter().map(text).collect();
            let ran = scratch.run(&argv);
            let named = format!("{}: {argv:?}", text(&group["name"]));
            assert_eq!(cleaned(&ran.out), text(&case["stdout"]), "{named}");
            assert_eq!(cleaned(&ran.err), text(&case["stderr"]).replace("python3 -m amui ", "amui "), "{named}");
            assert_eq!(i64::from(ran.status), case["status"].as_i64().unwrap(), "{named}");
            compared += 1;
        }
        // nothing but the stand-ins was asked, and nothing else was started
        assert!(scratch.wire.lock().unwrap().is_empty(), "{}", text(&group["name"]));
        assert!(scratch.machine.calls.lock().unwrap().is_empty(), "{}", text(&group["name"]));
    }
    assert_eq!(compared, 96);
}

#[test]
fn clipped_cells() {
    for case in list(&reference()["values"]["clip"]) {
        let width = case["width"].as_u64().unwrap() as usize;
        assert_eq!(clip(text(&case["text"]), width), text(&case["clipped"]), "{case}");
    }
}

#[test]
fn tables() {
    for case in list(&reference()["values"]["table"]) {
        let headers: Vec<&str> = list(&case["headers"]).iter().map(text).collect();
        let rows: Vec<Vec<String>> = list(&case["rows"])
            .iter()
            .map(|row| list(row).iter().map(|cell| text(cell).to_string()).collect())
            .collect();
        let widths: Vec<usize> =
            case["widths"].as_array().into_iter().flatten().map(|width| width.as_u64().unwrap() as usize).collect();
        let printed: String = table(&headers, &rows, &widths).iter().map(|line| format!("{line}\n")).collect();
        assert_eq!(printed, text(&case["printed"]), "{}", case["headers"]);
    }
}

#[test]
fn lengths_and_times() {
    let values = reference();
    for case in list(&values["values"]["mmss"]) {
        assert_eq!(mmss(case["ms"].as_i64()), text(&case["text"]), "{case}");
    }
    for case in list(&values["values"]["hms"]) {
        assert_eq!(hms(case["seconds"].as_f64()), text(&case["text"]), "{case}");
    }
}

#[test]
fn sizes() {
    for case in list(&reference()["values"]["fmt_bytes"]) {
        assert_eq!(fmt_bytes(case["size"].as_u64().unwrap()), text(&case["text"]), "{case}");
    }
}

#[test]
fn songs_named_by_id_or_link() {
    let cases = reference();
    let cases = list(&cases["values"]["song_id"]);
    assert!(cases.len() > 20);
    for case in cases {
        let expected = match case.get("id") {
            Some(id) => Ok(text(id).to_string()),
            None => Err(text(&case["error"])),
        };
        assert_eq!(song_id(text(&case["value"])), expected, "{case}");
    }
}

/// `str()` and `repr()` of values that came out of a JSON file, as an f-string and a dictionary
/// print them.
#[test]
fn values_in_python_s_spelling() {
    let cases = reference();
    let cases = list(&cases["values"]["python_text"]);
    assert!(cases.len() > 30);
    for case in cases {
        assert_eq!(shown(Some(&case["value"])), text(&case["str"]), "{case}");
        assert_eq!(repr(&case["value"]), text(&case["repr"]), "{case}");
    }
}

/// The rows `report_lines()` of the reference script adds, printed by the doctor's report.
#[test]
fn the_doctor_s_report_lines() {
    let mut report = Report::default();
    report.section("A section");
    report.add(Found::Ok, "one", "there", true);
    report.add(Found::Missing, "two", "needed", true);
    report.add(Found::Missing, "three", "nice to have", false);
    report.add(Found::Absent, "four", "", true);
    report.add(Found::Missing, "an item with a name longer than its column", "x", true);
    report.section("Another (with: details)");
    report.add(Found::Ok, "ffmpeg", "/usr/bin/ffmpeg — no libsoxr (resampling falls back to swr)", true);
    let printed = format!("{}missing: {}\n", report.text(), report.missing_required);
    assert_eq!(printed, text(&reference()["values"]["report"]));
}
