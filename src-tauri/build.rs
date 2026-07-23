use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    assemble_portal_dist();
    tauri_build::build()
}

/// Assemble the Portal webview payload (`../portal-dist`) from an explicit
/// runtime allowlist before the crate compiles.
///
/// `tauri::generate_context!` resolves `frontendDist` at compile time and
/// `portal-dist/` is generated, not committed, so every build path — plain
/// `cargo`, `cargo tauri`, and rust-analyzer — needs the directory to exist
/// without an external shell step (the previous `beforeBuildCommand` hook only
/// ran under the Tauri CLI and depended on `bash`). Only the file classes named
/// here can reach the signed bundle; `*.test.mjs` is excluded by construction.
fn assemble_portal_dist() {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set for build scripts");
    let source_dir = Path::new(&manifest_dir).join("../portal");
    let dist_dir = Path::new(&manifest_dir).join("../portal-dist");

    // Fixed, non-module runtime entry points. Anything not matched here or by
    // the module sweep below does not ship.
    let mut wanted: BTreeSet<String> = [
        "index.html",
        "style.css",
        "main.js",
        "codecs.js",
        "audio-worklet.js",
    ]
    .iter()
    .map(|name| (*name).to_string())
    .collect();
    for name in &wanted {
        if !source_dir.join(name).is_file() {
            panic!("allowlisted portal payload file is missing: {name}");
        }
    }

    // Every top-level ES module except test suites.
    for entry in fs::read_dir(&source_dir).expect("portal source directory must exist") {
        let entry = entry.expect("readable portal source entry");
        let is_file = entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false);
        if !is_file {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".mjs") && !name.ends_with(".test.mjs") {
            wanted.insert(name);
        }
    }

    fs::create_dir_all(&dist_dir).expect("portal-dist directory is creatable");

    // Copy the allowlist, overwriting stale content, and rebuild whenever any
    // staged source changes.
    for name in &wanted {
        fs::copy(source_dir.join(name), dist_dir.join(name))
            .unwrap_or_else(|error| panic!("failed to stage portal payload {name}: {error}"));
        println!("cargo:rerun-if-changed=../portal/{name}");
    }

    // Prune anything previously staged that is no longer allowlisted.
    for entry in fs::read_dir(&dist_dir).expect("portal-dist directory is readable") {
        let entry = entry.expect("readable portal-dist entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if !wanted.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }

    // Watch the directory itself so added or removed source files re-run this.
    println!("cargo:rerun-if-changed=../portal");
}
