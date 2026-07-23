use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Component, Path};

const PORTAL_PAYLOAD_MANIFEST: &str = "runtime-files.txt";

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

    let manifest_path = source_dir.join(PORTAL_PAYLOAD_MANIFEST);
    let manifest = fs::read_to_string(&manifest_path)
        .expect("portal runtime payload manifest must be readable");
    let mut wanted = BTreeSet::new();
    for (line_index, raw_name) in manifest.lines().enumerate() {
        let line_number = line_index + 1;
        let name = raw_name.trim();
        if name.is_empty() || name.starts_with('#') {
            continue;
        }
        if name != raw_name {
            panic!("portal runtime payload manifest line {line_number} has surrounding whitespace");
        }
        let path = Path::new(name);
        if !matches!(
            path.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            panic!(
                "portal runtime payload manifest line {line_number} is not one top-level file: {name}"
            );
        }
        if name.ends_with(".test.mjs") {
            panic!(
                "portal runtime payload manifest line {line_number} includes a test suite: {name}"
            );
        }
        if !wanted.insert(name.to_owned()) {
            panic!("portal runtime payload manifest repeats {name}");
        }
    }
    if wanted.is_empty() {
        panic!("portal runtime payload manifest must name at least one file");
    }
    for name in &wanted {
        let metadata = fs::symlink_metadata(source_dir.join(name)).unwrap_or_else(|error| {
            panic!("allowlisted portal payload file {name} is missing: {error}")
        });
        if !metadata.file_type().is_file() {
            panic!("allowlisted portal payload file is missing: {name}");
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
            let file_type = entry
                .file_type()
                .expect("portal-dist entry type is readable");
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path()).unwrap_or_else(|error| {
                    panic!("failed to prune portal-dist directory {name}: {error}")
                });
            } else {
                fs::remove_file(entry.path()).unwrap_or_else(|error| {
                    panic!("failed to prune portal-dist file {name}: {error}")
                });
            }
        }
    }

    // Watch the directory itself so added or removed source files re-run this.
    println!("cargo:rerun-if-changed=../portal/{PORTAL_PAYLOAD_MANIFEST}");
    println!("cargo:rerun-if-changed=../portal");
}
