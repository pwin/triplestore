//! A running server says which build it is.
//!
//! Three places, all from one `holos_build::Build`: the `Server` header on every response
//! (RFC 9110 §10.2.4), the `version` member of `/stats`, and `--version` on the binary.
//! The build is stamped by the binary's build script, so what this asserts on is the
//! shape — a product token, a version that parses, a commit when there is one — rather
//! than a value that changes with every commit.

use std::process::Command;

use holos_conformance::protocol::{self, ScriptedRequest};

mod harness;
use harness::{server_binary, Server};

fn get(address: &str, path: &str) -> protocol::HttpResponse {
    let request = ScriptedRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        headers: Vec::new(),
        body: None,
        expected_status: Vec::new(),
        expected_status_class: Vec::new(),
        expected_boolean: None,
        expected_format: None,
    };
    protocol::send(address, &request).expect("a response")
}

/// `0.13.0` from `holos/0.13.0 (d4b4fdd, modified)` or `holos-server 0.13.0`.
fn version_of(text: &str) -> &str {
    text.split(['/', ' '])
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("")
}

#[test]
fn the_server_names_its_build_on_the_wire() {
    let Some(binary) = server_binary() else {
        eprintln!("skipping: holos-server is not built");
        return;
    };
    let Some(server) = Server::start(&binary, 18_402, &["--read-only"]) else {
        eprintln!("skipping: no server on port 18402");
        return;
    };
    let address = &server.address;

    // --version, from the same binary the server is running.
    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("the binary runs");
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(printed.starts_with("holos-server "), "{printed}");
    let version = version_of(&printed).to_owned();
    assert_eq!(
        version.split('.').count(),
        3,
        "a version has three parts: {printed}"
    );

    // The Server header, on a response that has nothing to do with the version.
    let response = get(address, "/health");
    assert_eq!(response.status, 200);
    let header = response.header("server").expect("a Server header");
    assert!(header.starts_with("holos/"), "{header}");
    assert_eq!(version_of(header), version, "{header}");
    assert_ne!(header, "tiny-http (Rust)", "the library's own name is not ours");

    // And /stats carries it as data.
    let response = get(address, "/stats");
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        response
            .body
            .contains(&format!(r#""version":"{version}""#)),
        "{}",
        response.body
    );
    assert!(
        response.body.contains(r#""modified":"#),
        "{}",
        response.body
    );
    // A commit is present exactly when the printed version names one.
    assert_eq!(
        response.body.contains(r#""commit":""#),
        printed.contains('('),
        "{printed} against {}",
        response.body
    );
}
