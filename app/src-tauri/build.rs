use std::process::Command;

fn main() {
    version();
    tauri_build::build()
}

/// The app's version, the same for a commit on every machine and in CI: major.minor from
/// tauri.conf.json, then the number of commits on HEAD (CI writes the same into the bundle's
/// metadata). APP_VERSION = "0.1.44", APP_COMMIT = "8ff8ea1" (empty without git).
fn version() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let conf = std::fs::read_to_string("tauri.conf.json").unwrap_or_default();
    let base = conf
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"version\": \"")?.split('"').next().map(str::to_string))
        .unwrap_or_else(|| "0.1.0".into());
    let (count, commit) = (git(&["rev-list", "--count", "HEAD"]), git(&["rev-parse", "--short=7", "HEAD"]));
    let version = match &count {
        Some(n) => format!("{}.{n}", base.rsplit_once('.').map_or(base.as_str(), |(mm, _)| mm)),
        None => base,
    };
    println!("cargo:rustc-env=APP_VERSION={version}");
    println!("cargo:rustc-env=APP_COMMIT={}", commit.unwrap_or_default());
    // Recomputed when HEAD moves (a commit, a checkout).
    println!("cargo:rerun-if-changed=tauri.conf.json");
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "refs/heads", "packed-refs"] {
            let p = std::path::Path::new(&dir).join(f);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
}
