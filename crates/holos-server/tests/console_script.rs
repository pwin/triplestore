//! The console's configuration script, checked as JavaScript.
//!
//! The script is JavaScript assembled by Rust, with the endpoint and the tile template
//! substituted in, so `cargo test` can assert what text it contains but not that a browser
//! would parse it. `node --check` can, and does here, for both shapes the script takes —
//! a tile host and a sealed, blank basemap.
//!
//! # When Node is absent
//!
//! The test skips rather than fails. Node is not a build dependency of this workspace and
//! should not become one on the strength of a console script; anyone who has it gets the
//! check, and CI has it.

use std::process::Command;

#[path = "../src/ui.rs"]
mod ui;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_console_script_parses_as_javascript_in_both_shapes() {
    if !node_available() {
        eprintln!("skipping: node is not installed, so the script cannot be parsed");
        return;
    }
    for (label, tiles) in [("tiles", Some(ui::DEFAULT_TILES)), ("sealed", None)] {
        let script = ui::script("/query", tiles);
        let path =
            std::env::temp_dir().join(format!("holos-console-{label}-{}.js", std::process::id()));
        std::fs::write(&path, &script).expect("writing the script");
        let output = Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
            .expect("running node");
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "the {label} script does not parse:\n{}\n{script}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
