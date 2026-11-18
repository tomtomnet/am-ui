//! Packs the application's own icons into the program (GResource) and records which commit it was
//! built from, for the About dialog.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok().filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|text| !text.is_empty())
}

fn main() {
    glib_build_tools::compile_resources(&["resources"], "resources/resources.gresource.xml", "am-ui.gresource");
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let changed = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
    println!("cargo:rustc-env=AMUI_BUILD={commit}{}", if changed { "+" } else { "" });
    // built again after a commit, a checkout or a change of the tracked files' state
    for name in ["HEAD", "index", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=resources");
}
