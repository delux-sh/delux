//! Embeds the short Git commit, so builds sharing a version can be told apart.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // A source archive unpacked inside some other repository must not report
    // that repository's commit: only this crate's own checkout counts.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(std::path::PathBuf::from);
    let own_checkout = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| {
        manifest.is_some_and(|manifest| {
            Path::new(&top).canonicalize().ok() == manifest.canonicalize().ok()
        })
    });
    if !own_checkout {
        return;
    }
    if let Some(commit) = git(&["rev-parse", "--short=9", "HEAD"]) {
        println!("cargo:rustc-env=DELUX_COMMIT={commit}");
    }
    // The reflog changes on every commit and checkout, in worktrees too. A
    // missing path would make Cargo rerun this script on every build.
    for name in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
