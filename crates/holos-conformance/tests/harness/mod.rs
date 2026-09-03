//! Starting a real `holos-server` for the protocol suites.
//!
//! Shared by [`protocol.rs`](../protocol.rs) and [`sparql_protocol.rs`](../sparql_protocol.rs).
//! Both replay scripted HTTP conversations, and both need the same thing: a server on a
//! port nobody else is using, over an empty store, that goes away when the test does.
//!
//! This lives in a subdirectory so Cargo treats it as a module rather than as a third test
//! binary.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A server running for the duration of one test.
pub struct Server {
    child: Child,
    /// `host:port`, ready to hand to [`holos_conformance::protocol::send`].
    pub address: String,
}

impl Server {
    /// Starts one on the given port, waiting until it says it is listening.
    ///
    /// `extra` carries the flags a particular suite needs — the Graph Store path and base,
    /// for both suites, since the SPARQL Protocol suite loads its dataset through it.
    ///
    /// Returns `None` when the server does not come up, which the caller reports as a skip
    /// rather than a failure: a port collision is not a conformance result.
    pub fn start(binary: &Path, port: u16, extra: &[&str]) -> Option<Self> {
        let address = format!("127.0.0.1:{port}");
        let mut args = vec!["--listen", &address, "--no-ui"];
        args.extend_from_slice(extra);
        let mut child = Command::new(binary)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        // Wait for the "listening" line rather than sleeping: a fixed sleep is either too
        // short on a loaded machine or wasted on an idle one.
        let stderr = child.stderr.take()?;
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        for _ in 0..40 {
            line.clear();
            if reader.read_line(&mut line).ok()? == 0 {
                break;
            }
            if line.contains("listening on") {
                // Keep draining. Stopping here leaves the pipe to fill, and a server that
                // blocks writing to stderr stops answering requests — which surfaces as
                // "connection forcibly closed" partway through a conversation and looks
                // like a server bug rather than a harness one.
                std::thread::spawn(move || {
                    let mut sink = String::new();
                    while reader.read_line(&mut sink).unwrap_or(0) > 0 {
                        sink.clear();
                    }
                });
                return Some(Self { child, address });
            }
        }
        let _ = child.kill();
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where `cargo` put the server binary.
pub fn server_binary() -> Option<PathBuf> {
    // The test binary lives in target/<profile>/deps/, so the server is two levels up.
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let name = if cfg!(windows) {
        "holos-server.exe"
    } else {
        "holos-server"
    };
    let path = dir.join(name);
    path.is_file().then_some(path)
}

/// Percent-encodes an IRI for use as a query-parameter value.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Prints a request when `HOLOS_PROTOCOL_DEBUG=1`, body escaped.
///
/// Worth having: a status code is all a failure otherwise reports, and the cause is usually
/// in bytes the manifest went to some trouble to specify. One failure here was a checkout
/// translating line endings, visible only as `\r\r\n` in a body that looked perfectly
/// ordinary at every other level.
pub fn trace(request: &holos_conformance::protocol::ScriptedRequest) {
    if std::env::var("HOLOS_PROTOCOL_DEBUG").is_err() {
        return;
    }
    eprintln!(
        "  -> {} {} headers={:?}",
        request.method, request.path, request.headers
    );
    if let Some(body) = &request.body {
        eprintln!(
            "     {} bytes: {:?}",
            body.len(),
            &body[..body.len().min(160)]
        );
    }
}
