//! `holos validate` as a script sees it: its exit status, and which validator it ran.
//!
//! Both were wrong in a way no unit test could see. It exited 0 whether or not the data
//! conformed, so a CI step built on it passed on the violations it was there to stop; and it
//! defaulted to the native validator where Python's `Store.validate` defaults to the adapted
//! one, so the same shapes were refused here and validated there. These run the real binary.

use std::path::Path;
use std::process::{Command, Output};

const SHAPES: &str = "@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
";

/// The same constraint as a SPARQL constraint: beyond SHACL Core, so the native validator
/// refuses it and the adapted one checks it.
const SPARQL_SHAPES: &str = "@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
  sh:sparql [ sh:select \"SELECT $this WHERE { FILTER NOT EXISTS { $this <http://example.com/name> ?n } }\" ] .
";

const CONFORMING: &str = "@prefix ex: <http://example.com/> .
ex:alice a ex:Person ; ex:name \"Alice\" .
";

/// Bob has no name.
const VIOLATING: &str = "@prefix ex: <http://example.com/> .
ex:alice a ex:Person ; ex:name \"Alice\" .
ex:bob a ex:Person .
";

fn write(dir: &Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    path.to_string_lossy().into_owned()
}

fn validate(data: &str, shapes: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_holos"))
        .args(["validate", "--data", data, "--shapes", shapes])
        .args(extra)
        .output()
        .expect("run holos")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("an exit code")
}

struct Files {
    _dir: tempfile::TempDir,
    conforming: String,
    violating: String,
    shapes: String,
    sparql_shapes: String,
}

fn files() -> Files {
    let dir = tempfile::tempdir().expect("temp dir");
    Files {
        conforming: write(dir.path(), "ok.ttl", CONFORMING),
        violating: write(dir.path(), "bad.ttl", VIOLATING),
        shapes: write(dir.path(), "shapes.ttl", SHAPES),
        sparql_shapes: write(dir.path(), "sparql.ttl", SPARQL_SHAPES),
        _dir: dir,
    }
}

#[test]
fn conforming_data_exits_0_and_violations_exit_1_on_every_engine() {
    let f = files();
    for engine in [&[][..], &["--engine", "adapted"], &["--engine", "native"]] {
        assert_eq!(
            code(&validate(&f.conforming, &f.shapes, engine)),
            0,
            "{engine:?}"
        );
        assert_eq!(
            code(&validate(&f.violating, &f.shapes, engine)),
            1,
            "{engine:?}"
        );
    }
}

#[test]
fn the_default_is_the_adapted_validator() {
    let f = files();
    let output = validate(&f.violating, &f.sparql_shapes, &[]);
    assert_eq!(
        code(&output),
        1,
        "the SPARQL constraint was checked, and Bob violates it: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("bridged+compiled"),
        "the adapted validator's summary"
    );
}

#[test]
fn a_shapes_graph_the_engine_refuses_exits_2_not_1() {
    let f = files();
    let output = validate(&f.violating, &f.sparql_shapes, &["--engine", "native"]);
    assert_eq!(
        code(&output),
        2,
        "refused, so no answer: not the same as 'does not conform'"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("sh:sparql"));
}

#[test]
fn an_engine_it_does_not_know_exits_2_and_names_the_choices() {
    let f = files();
    let output = validate(&f.violating, &f.shapes, &["--engine", "adaptd"]);
    assert_eq!(
        code(&output),
        2,
        "it used to run the native validator instead"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`adapted` or `native`"), "{stderr}");
}

#[test]
fn a_missing_shapes_file_exits_2() {
    let f = files();
    assert_eq!(code(&validate(&f.violating, "no-such-shapes.ttl", &[])), 2);
}

#[test]
fn the_old_spelling_still_selects_the_adapted_validator() {
    let f = files();
    let output = validate(&f.violating, &f.sparql_shapes, &["--engine", "vendored"]);
    assert_eq!(code(&output), 1);
}

/// Every other command keeps the status a failure always had.
#[test]
fn other_commands_still_exit_1_on_failure() {
    let f = files();
    let output = Command::new(env!("CARGO_BIN_EXE_holos"))
        .args(["query", "--data", &f.conforming, "--query", "SELECT WHERE"])
        .output()
        .expect("run holos");
    assert_eq!(code(&output), 1);
}
