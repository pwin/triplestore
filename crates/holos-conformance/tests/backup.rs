//! An in-memory server backs up to a snapshot file, and a server started on that file holds
//! the same store.
//!
//! This is the whole of the "memory as the store" mode, end to end over HTTP: serve from
//! memory, `POST /backup` from a scheduler, restart with `--data` on the newest snapshot. A
//! unit test of the dump would not show the part that matters, which is that the two halves
//! -- the server's writer and its own start-up reader -- agree.

use holos_conformance::protocol::{self, HttpResponse, ScriptedRequest};

mod harness;
use harness::{Server, server_binary};

const EX: &str = "http://holos.example/";

fn send(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> HttpResponse {
    let request = ScriptedRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        body: body.map(str::to_owned),
        expected_status: Vec::new(),
        expected_status_class: Vec::new(),
        expected_boolean: None,
        expected_format: None,
    };
    protocol::send(address, &request).expect("a response")
}

/// The value of a top-level JSON number field, read without a JSON dependency.
fn number(body: &str, field: &str) -> Option<u64> {
    let start = body.find(&format!(r#""{field}":"#))? + field.len() + 3;
    body[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// The value of a top-level JSON string field, unescaping only what a Windows path needs.
fn string(body: &str, field: &str) -> Option<String> {
    let start = body.find(&format!(r#""{field}":""#))? + field.len() + 4;
    let end = body[start..].find('"')? + start;
    Some(body[start..end].replace("\\\\", "\\"))
}

#[test]
fn an_in_memory_server_snapshots_to_a_file_it_can_restart_from() {
    let Some(binary) = server_binary() else {
        eprintln!("skipping: holos-server is not built");
        return;
    };
    let dir = std::env::temp_dir().join(format!("holos-backup-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let data = dir.join("data.nq");
    std::fs::write(
        &data,
        format!(
            "<{EX}a> <{EX}p> <{EX}o> .\n\
             <{EX}b> <{EX}p> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> <{EX}g1> .\n\
             <{EX}c> <{EX}p> \"x\"@en <{EX}g2> .\n"
        ),
    )
    .expect("writing the data");
    let backups = dir.join("backups");
    let data_arg = data.display().to_string();
    let backups_arg = backups.display().to_string();

    let Some(server) = Server::start(
        &binary,
        18_421,
        &[
            "--data",
            &data_arg,
            "--backup-dir",
            &backups_arg,
            "--backup-role",
            "ops",
            "--trust-forwarded-identity",
        ],
    ) else {
        eprintln!("skipping: no server on port 18421");
        return;
    };

    // Without the role there is no backup, and nothing is written.
    let refused = send(
        &server.address,
        "POST",
        "/backup",
        &[("X-Holos-Principal", "alice")],
        None,
    );
    assert_eq!(refused.status, 403, "{}", refused.body);

    let ops = [("X-Holos-Principal", "scheduler"), ("X-Holos-Roles", "ops")];
    let made = send(&server.address, "POST", "/backup", &ops, None);
    assert_eq!(made.status, 201, "{}", made.body);
    assert_eq!(number(&made.body, "quads"), Some(3), "{}", made.body);
    let snapshot = string(&made.body, "path").expect("the snapshot's path");
    assert!(snapshot.ends_with(".nq.gz"), "{snapshot}");
    assert!(std::path::Path::new(&snapshot).is_file(), "{snapshot}");
    let leftovers: Vec<_> = std::fs::read_dir(&backups)
        .expect("the backup directory")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".partial"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a finished snapshot leaves no partial file: {leftovers:?}"
    );

    // A second backup in the same second would overwrite the first, and is refused instead.
    let again = send(&server.address, "POST", "/backup", &ops, None);
    assert!(again.status == 201 || again.status == 409, "{}", again.body);

    // Restart from the snapshot: the same quads, in the same graphs.
    let Some(restored) = Server::start(&binary, 18_422, &["--data", &snapshot]) else {
        eprintln!("skipping: no server on port 18422");
        return;
    };
    let stats = send(&restored.address, "GET", "/stats", &[], None);
    assert_eq!(stats.status, 200, "{}", stats.body);
    assert_eq!(number(&stats.body, "quads"), Some(3), "{}", stats.body);
    assert_eq!(
        number(&stats.body, "namedGraphs"),
        Some(2),
        "{}",
        stats.body
    );
    let query = format!("ASK {{ GRAPH <{EX}g2> {{ <{EX}c> <{EX}p> \"x\"@en }} }}");
    let asked = send(
        &restored.address,
        "POST",
        "/query",
        &[
            ("Content-Type", "application/sparql-query"),
            ("Accept", "application/sparql-results+json"),
        ],
        Some(&query),
    );
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert!(
        asked.body.contains("true"),
        "the language-tagged literal kept its graph: {}",
        asked.body
    );

    drop(server);
    drop(restored);
    let _ = std::fs::remove_dir_all(&dir);
}
