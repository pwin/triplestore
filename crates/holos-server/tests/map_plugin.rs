//! The console's map plugin, checked by running it.
//!
//! The plugin is JavaScript embedded in a Rust string, so `cargo test` can assert that the
//! text is present but not that it *works*. The interesting part — turning a GeoSPARQL WKT
//! literal into coordinates Leaflet will accept — is exactly the part a text assertion
//! cannot reach, and its failure mode is a map that renders happily in the wrong place.
//!
//! So this renders the console page, then runs [`map_plugin.js`](map_plugin.js) over it
//! under Node with Leaflet stubbed out. The stub records what it was handed, which makes
//! coordinate order observable.
//!
//! # When Node is absent
//!
//! The test skips rather than fails. Node is not a build dependency of this workspace and
//! should not become one on the strength of a console plugin; anyone who has it gets the
//! check, and CI has it.

use std::path::PathBuf;
use std::process::Command;

#[path = "../src/ui.rs"]
mod ui;

/// Whether a working `node` is on the path.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_wkt_parser_produces_the_coordinates_leaflet_expects() {
    if !node_available() {
        eprintln!("skipping: node is not installed, so the plugin's JavaScript cannot be run");
        return;
    }

    let checker = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("map_plugin.js");
    assert!(checker.is_file(), "{} is missing", checker.display());

    // The page is what a browser would receive, not a hand-assembled fragment: if the
    // plugin ever stops being served, this fails rather than testing a copy of it.
    let page = ui::console("/query", "HOLOS");
    let page_path = std::env::temp_dir().join("holos-console-under-test.html");
    std::fs::write(&page_path, page).expect("writing the console page");

    let output = Command::new("node")
        .arg(&checker)
        .arg(&page_path)
        .output()
        .expect("running node");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    assert!(
        output.status.success(),
        "the plugin's parser failed its checks:\n{stdout}\n{stderr}"
    );
}
