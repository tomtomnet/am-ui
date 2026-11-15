//! Self-contained checks of the cavern-lab analysis tools (tools/cavern-lab/analyze.py, bands.py,
//! consistency.py), the Rust side of tests/test_render_artifact.py.
//!
//! The tools are Python programs built on numpy. They are not part of the engine and stay what
//! they are, so each test runs a few lines of Python against them (`python3 -I -B`, nothing is
//! written into the source tree) and checks the numbers that come back. The signals are
//! synthetic: neither the media cache nor the .NET probe is needed. On a machine whose python3
//! has no numpy the tests say so and pass. When a real lab render exists under
//! var/tmp/cavern-lab/baseline/, one extra test asserts the documented burst metrics of the
//! baseline; it is ignored by default.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// What every test starts from: the tools imported from the folder named first on the command
/// line, and the synthetic render of the Python tests.
const PRELUDE: &str = r#"
import importlib, json, math, sys
from pathlib import Path

import numpy as np

sys.path.insert(0, sys.argv[1])
analyze = importlib.import_module("analyze")
bands = importlib.import_module("bands")
consistency = importlib.import_module("consistency")
RATE = 48000


def synthetic_render(seconds=3.0, burst=(1.2, 1.6), seed=7):
    """16-channel render: music in SL/SR, a jagged noise burst in FL/FR between the given times."""
    rng = np.random.default_rng(seed)
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    x = np.zeros((n, 16), dtype=np.float32)
    tone = 0.1 * np.sin(2 * math.pi * 220 * t) + 0.05 * np.sin(2 * math.pi * 330 * t)
    x[:, analyze.NAMES.index("SL")] = tone
    x[:, analyze.NAMES.index("SR")] = tone
    a, b = int(burst[0] * RATE), int(burst[1] * RATE)
    noise = rng.standard_normal(b - a).astype(np.float32) * 0.03
    x[a:b, analyze.NAMES.index("FL")] = noise
    x[a:b, analyze.NAMES.index("FR")] = noise
    return x

"#;

/// The am-ui folder this crate sits in.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Run `body` after the prelude; what it leaves in `result` comes back. `None` (with a note)
/// when this machine has no python3 with numpy.
fn lab(body: &str, arguments: &[&Path]) -> Option<Value> {
    let script = format!("{PRELUDE}{body}\nprint(json.dumps(result))\n");
    let mut python = Command::new("python3");
    python.args(["-I", "-B", "-c"]).arg(script).arg(root().join("tools/cavern-lab")).args(arguments);
    let Ok(done) = python.stdin(Stdio::null()).output() else {
        eprintln!("skipped: python3 needed");
        return None;
    };
    let errors = String::from_utf8_lossy(&done.stderr);
    if !done.status.success() && errors.contains("No module named 'numpy'") {
        eprintln!("skipped: python3 with numpy needed");
        return None;
    }
    assert!(done.status.success(), "the lab tools failed:\n{errors}");
    Some(serde_json::from_slice(&done.stdout).expect("the lab tools answer with JSON"))
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// Python's `assertAlmostEqual(a, b, places=places)`.
fn almost_equal(a: f64, b: f64, places: i32) -> bool {
    ((a - b).abs() * 10f64.powi(places)).round() == 0.0
}

/// The centre channel's share of each side; the tools write it as 0.70710678.
const MINUS_3_DB: f64 = std::f64::consts::FRAC_1_SQRT_2;

// --- fold-down

#[test]
fn peqdb_fold_places_every_channel() {
    let body = r#"
x = np.zeros((10, 16), dtype=np.float32)
result = []
for index, name in enumerate(analyze.NAMES):
    x[:] = 0
    x[:, index] = 1
    fold = analyze.fold_down(x)
    result.append([name, float(fold[0, 0]), float(fold[0, 1]), float(fold.sum())])
"#;
    let Some(folds) = lab(body, &[]) else { return };
    let folds = folds.as_array().unwrap();
    assert_eq!(folds.len(), 16);
    for fold in folds {
        let name = fold[0].as_str().unwrap();
        let (left, right, sum) = (number(&fold[1]), number(&fold[2]), number(&fold[3]));
        if name == "LFE" {
            assert_eq!(sum, 0.0, "the LFE path is handled separately");
        } else if name == "FC" {
            assert!(almost_equal(left, MINUS_3_DB, 6), "{left}");
            assert!(almost_equal(right, MINUS_3_DB, 6), "{right}");
        } else if name.trim_end_matches('W').ends_with('L') {
            assert_eq!((left, right), (1.0, 0.0), "{name}");
        } else {
            assert_eq!((left, right), (0.0, 1.0), "{name}");
        }
    }
}

#[test]
fn fold_accepts_other_layouts() {
    let body = r#"
names = "FL FR FC LFE SL SR".split()
x = np.ones((4, 6), dtype=np.float32)
fold = analyze.fold_down(x, names)
result = float(fold[0, 0])
"#;
    let Some(left) = lab(body, &[]) else { return };
    assert!(almost_equal(number(&left), 2.70710678, 6), "{left}");
}

// --- alignment

#[test]
fn offset_recovers_reference_lead_in() {
    let body = r#"
render = synthetic_render()
fold = analyze.fold_down(render)
lead_in = 1234
ref = np.zeros((len(fold) + lead_in, 2))
ref[lead_in:] = fold
result = analyze.find_offset(fold, ref, seconds=3.0, max_lag=RATE)
"#;
    let Some(offset) = lab(body, &[]) else { return };
    assert_eq!(offset, 1234);
}

#[test]
fn bin_metrics_align_with_offset() {
    let body = r#"
render = synthetic_render()
fold = analyze.fold_down(render)
lead_in = 777
ref = np.zeros((len(fold) + lead_in, 2))
ref[lead_in:] = fold
rows = analyze.bin_metrics(render, fold, ref, lead_in, 0.0, 2.9)
result = min(r["ncc"] for r in rows)
"#;
    let Some(lowest) = lab(body, &[]) else { return };
    assert!(number(&lowest) > 0.999, "{lowest}");
}

// --- burst metrics

#[test]
fn burst_is_detected_in_front_channels_only() {
    let body = r#"
render = synthetic_render()
fold = analyze.fold_down(render)
rows = analyze.bin_metrics(render, fold, None, 0, 0.0, 3.0)
result = analyze.summarise(rows, burst=(1.2, 1.6), clean=(0.0, 1.0))
"#;
    let Some(summary) = lab(body, &[]) else { return };
    assert!(almost_equal(number(&summary["clean_front_rms"]), 0.0, 7), "{summary}");
    assert!(number(&summary["burst_front_rms"]) > 0.02, "{summary}");
    assert!(number(&summary["burst_front_hf_share_mean"]) > 0.5, "white noise has most energy above 6 kHz");
    assert!(number(&summary["burst_front_max_jump"]) > 0.05, "{summary}");
}

// --- band tools

#[test]
fn butterworth_lowpass_attenuates_above_cutoff() {
    let body = r#"
t = np.arange(RATE) / RATE
low = np.sin(2 * math.pi * 30 * t)
high = np.sin(2 * math.pi * 1000 * t)
result = [float(np.abs(bands.butterworth4_lowpass(low, 125.0))[RATE // 2:].max()),
          float(np.abs(bands.butterworth4_lowpass(high, 125.0))[RATE // 2:].max())]
"#;
    let Some(peaks) = lab(body, &[]) else { return };
    assert!(number(&peaks[0]) > 0.9, "{peaks}");
    assert!(number(&peaks[1]) < 0.01, "{peaks}");
}

#[test]
fn band_levels_peak_in_the_right_band() {
    let body = r#"
t = np.arange(2 * RATE) / RATE
levels = bands.band_levels(np.sin(2 * math.pi * 1000 * t))
result = bands.THIRD_OCTAVE[int(np.argmax(levels))]
"#;
    let Some(band) = lab(body, &[]) else { return };
    assert_eq!(band, 1000);
}

#[test]
fn bed_fold_matrix() {
    let body = r#"
bed = np.zeros((4, 6))
bed[:, 2] = 1  # FC
fold = consistency.bed_fold(bed)
result = [float(fold[0, 0]), float(fold[0, 1])]
"#;
    let Some(fold) = lab(body, &[]) else { return };
    assert!(almost_equal(number(&fold[0]), MINUS_3_DB, 6), "{fold}");
    assert!(almost_equal(number(&fold[1]), MINUS_3_DB, 6), "{fold}");
}

// --- documented numbers of the song A baseline (research/cavern-artifact.md), checked only when
// the render exists

#[test]
#[ignore = "needs a lab baseline render under var/tmp/cavern-lab/baseline (tools/cavern-lab/run-variants.sh)"]
fn documented_burst_metrics() {
    let render = root().join("var/tmp/cavern-lab/baseline/atmos-916.f32le");
    if !render.exists() {
        eprintln!("skipped: no lab baseline render under var/tmp/cavern-lab/baseline");
        return;
    }
    let body = r#"
render = analyze.load(Path(sys.argv[2]), 16)
fold = analyze.fold_down(render)
rows = analyze.bin_metrics(render, fold, None, 0, 4.0, 7.9)
result = analyze.summarise(rows)
"#;
    let Some(summary) = lab(body, &[&render]) else { return };
    assert!(almost_equal(number(&summary["burst_front_rms"]), 0.0287, 3), "{summary}");
    assert!(almost_equal(number(&summary["burst_front_peak"]), 0.261, 2), "{summary}");
    assert!(number(&summary["clean_front_rms"]) < 0.007, "{summary}");
    if let Ok(text) = std::fs::read_to_string(render.with_file_name("render-summary.json")) {
        let data: Value = serde_json::from_str(&text).expect("the render summary is JSON");
        assert_eq!(data["channel_fallback_blocks"], 0);
        assert_eq!(data["frames"], 7639278);
    }
}
