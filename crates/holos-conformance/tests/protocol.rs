//! The Graph Store Protocol suite, run against a live server.
//!
//! Every other suite in this harness evaluates something in-process. These cannot: a
//! `GraphStoreProtocolTest` is a scripted HTTP conversation, and what it is testing is the
//! *protocol* — status codes, content negotiation, which verb replaces and which merges.
//! Short-circuiting the socket would mean testing the handler rather than the server.
//!
//! So this starts the real `holos-server` binary on an ephemeral port, replays each test's
//! request list against it, and checks the status of each response.
//!
//! ```text
//! cargo test -p holos-conformance --test protocol
//! ```
//!
//! # Fresh server per test, and why
//!
//! The scripts build on their own effects — `PUT` then `GET` then `DELETE` on the same
//! graph — so a store carried over from a previous test would make results depend on
//! execution order. Each test gets its own server over an empty in-memory store.

use holos_conformance::{manifest, protocol, testsuite_root};

mod harness;
use harness::{percent_encode, server_binary, trace, Server};

#[test]
fn graph_store_protocol() {
    let Some(root) = testsuite_root() else {
        eprintln!("skipping: run scripts/fetch-testsuites.sh first");
        return;
    };
    let Some(binary) = server_binary() else {
        eprintln!(
            "skipping: holos-server is not built. `cargo build -p holos-server` first, or run \
             `cargo test --workspace` which builds it."
        );
        return;
    };

    let manifest_path = root
        .join("sparql")
        .join("sparql11")
        .join("graph-store-protocol")
        .join("manifest.ttl");
    if !manifest_path.is_file() {
        eprintln!("skipping: {} is absent", manifest_path.display());
        return;
    }

    let tests = manifest::load(&manifest_path).expect("reading the manifests");
    let scripted: Vec<_> = tests
        .iter()
        .filter(|t| t.kind.ends_with("GraphStoreProtocolTest"))
        .collect();
    assert!(
        !scripted.is_empty(),
        "the manifest holds no GraphStoreProtocolTest entries"
    );

    let mut passed = Vec::new();
    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    let mut port = 18_080_u16;

    for test in &scripted {
        let Some(script) = &test.script else {
            skipped.push((test.short_id().to_owned(), "no ht:Connection in the action".to_owned()));
            continue;
        };
        if script.requests.is_empty() {
            skipped.push((test.short_id().to_owned(), "no requests in the script".to_owned()));
            continue;
        }

        port += 1;
        // The suite addresses the protocol at /gsp, and direct graph identification needs
        // a base — the suite's authority is `www.example`, and without it the direct tests
        // cannot resolve a name.
        let Some(server) = Server::start(
            &binary,
            port,
            &["--gsp-path", "/gsp", "--gsp-base", "http://www.example"],
        ) else {
            skipped.push((test.short_id().to_owned(), format!("no server on port {port}")));
            continue;
        };

        match replay(&server.address, script) {
            Ok(()) => passed.push(test.short_id().to_owned()),
            Err(why) => failed.push((test.short_id().to_owned(), why)),
        }
    }

    let attempted = passed.len() + failed.len();
    println!(
        "\ngraph-store-protocol: {}/{} passed, {} failed, {} skipped\n",
        passed.len(),
        attempted,
        failed.len(),
        skipped.len()
    );
    for (id, why) in &failed {
        println!("  {id}\n      {why}");
    }
    for (id, why) in &skipped {
        println!("  {id} (skipped)\n      {why}");
    }

    holos_conformance::ratchet_named("graph-store-protocol", &failed);
}

/// Replays one conversation, checking each response's status.
///
/// Bodies are not compared. The suite's expected bodies are Turtle that has to match up to
/// isomorphism *and* up to whatever the server chose to serialise, and a mismatch there
/// says something about the serialiser rather than about the protocol. Status codes are
/// what these tests are actually about, and they are checked strictly.
fn replay(address: &str, script: &protocol::Script) -> Result<(), String> {
    // `$LOCATION$` in a path means "the Location header the server last sent". The suite
    // uses it to follow a graph the server chose the name of.
    let mut last_location: Option<String> = None;

    for (i, request) in script.requests.iter().enumerate() {
        let mut request = request.clone();
        if request.path.contains("$LOCATION$") {
            let Some(location) = &last_location else {
                return Err(format!(
                    "request {} uses $LOCATION$ but no previous response sent a Location                      header",
                    i + 1
                ));
            };
            request.path = request.path.replace("$LOCATION$", &percent_encode(location));
        }
        let request = &request;

        trace(request);
        let response = protocol::send(address, request)
            .map_err(|e| format!("request {} ({} {}): {e}", i + 1, request.method, request.path))?;
        if let Some(location) = response.header("location") {
            last_location = Some(location.to_owned());
        }

        if !request.accepts_status(response.status) {
            return Err(format!(
                "request {} ({} {}) answered {} — the specification accepts {}",
                i + 1,
                request.method,
                request.path,
                response.status,
                request.expectation()
            ));
        }
    }
    Ok(())
}
