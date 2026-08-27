//! What fraction of each W3C suite is actually run, broken down by test type.
//!
//! The headline conformance numbers report `passed / attempted`, which is the right
//! measure of *correctness* but says nothing about **coverage**: a suite where 300 of 625
//! tests are skipped can still report 100%. Both numbers are needed to describe a store
//! honestly, so this prints them side by side.
//!
//! ```text
//! cargo run --release -p holos-conformance --example coverage
//! ```

use holos_conformance::{manifest, run_sparql_test, testsuite_root, Outcome};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Clone, Copy)]
struct Tally {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Tally {
    fn total(self) -> usize {
        self.passed + self.failed + self.skipped
    }
    fn attempted(self) -> usize {
        self.passed + self.failed
    }
}

fn suite(label: &str, manifest_path: &Path) {
    if !manifest_path.is_file() {
        eprintln!(
            "{label}: {} is absent — run scripts/fetch-testsuites.sh",
            manifest_path.display()
        );
        return;
    }
    let tests = match manifest::load(manifest_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{label}: could not read manifests: {e:#}");
            return;
        }
    };

    let mut by_kind: BTreeMap<String, Tally> = BTreeMap::new();
    let mut skip_reasons: BTreeMap<String, usize> = BTreeMap::new();

    for test in &tests {
        let kind = test
            .kind
            .rsplit(['#', '/'])
            .next()
            .unwrap_or(&test.kind)
            .to_owned();
        let entry = by_kind.entry(kind.clone()).or_default();

        // The protocol suites are scripted HTTP conversations, so `run_sparql_test` cannot
        // answer for them — but they *are* run, by `cargo test -p holos-conformance --test
        // protocol` and `--test sparql_protocol`. Counting them as skipped here would
        // understate coverage by 47 tests. Their results come from the ratcheted baselines,
        // which those runs keep honest: a test that starts or stops failing fails the run
        // until the baseline is updated.
        if let Some(baseline) = baseline_for(&kind) {
            match known_failures(baseline) {
                Some(failures) => {
                    if failures.contains(test.short_id()) {
                        entry.failed += 1;
                    } else {
                        entry.passed += 1;
                    }
                    continue;
                }
                None => {
                    entry.skipped += 1;
                    *skip_reasons
                        .entry(format!("{kind}: no baseline — run the protocol harness"))
                        .or_default() += 1;
                    continue;
                }
            }
        }

        match run_sparql_test(test) {
            Outcome::Passed => entry.passed += 1,
            Outcome::Failed(_) => entry.failed += 1,
            Outcome::Skipped(why) => {
                entry.skipped += 1;
                // "UpdateEvaluationTest: not implemented yet" -> the part before the colon
                // is the type, which is already the row; the reason is what varies.
                let reason = why.split(':').next().unwrap_or(&why).to_owned();
                *skip_reasons.entry(reason).or_default() += 1;
            }
        }
    }

    let overall = by_kind.values().fold(Tally::default(), |mut a, t| {
        a.passed += t.passed;
        a.failed += t.failed;
        a.skipped += t.skipped;
        a
    });

    println!("\n{label}");
    println!("{}", "=".repeat(label.len()));
    println!(
        "\n{:<32} {:>7} {:>7} {:>7} {:>7}",
        "test type", "total", "passed", "failed", "skipped"
    );
    println!("{}", "-".repeat(64));
    for (kind, t) in &by_kind {
        println!(
            "{:<32} {:>7} {:>7} {:>7} {:>7}",
            kind,
            t.total(),
            t.passed,
            t.failed,
            t.skipped
        );
    }
    println!("{}", "-".repeat(64));
    println!(
        "{:<32} {:>7} {:>7} {:>7} {:>7}",
        "all",
        overall.total(),
        overall.passed,
        overall.failed,
        overall.skipped
    );

    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            n as f64 * 100.0 / d as f64
        }
    };
    println!(
        "\n  correctness  {}/{} of what is run passes  ({:.1}%)",
        overall.passed,
        overall.attempted(),
        pct(overall.passed, overall.attempted())
    );
    println!(
        "  coverage     {}/{} of the suite is run      ({:.1}%)",
        overall.attempted(),
        overall.total(),
        pct(overall.attempted(), overall.total())
    );

    if !skip_reasons.is_empty() {
        println!("\n  not run, by reason:");
        let mut reasons: Vec<_> = skip_reasons.into_iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(&a.1));
        for (reason, n) in reasons {
            println!("    {n:>4}  {reason}");
        }
    }
}

/// Which ratcheted baseline holds the results for a test type, if it is run over HTTP.
fn baseline_for(kind: &str) -> Option<&'static str> {
    match kind {
        "GraphStoreProtocolTest" => Some("graph-store-protocol"),
        "ProtocolTest" => Some("sparql-protocol"),
        _ => None,
    }
}

/// The tests a suite's baseline records as failing, or `None` when there is no baseline.
///
/// An absent file means the harness has never been run, which is a skip rather than a pass:
/// claiming coverage from a run that did not happen is exactly the dishonesty this example
/// exists to prevent.
fn known_failures(suite: &str) -> Option<std::collections::HashSet<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("conformance")
        .join(format!("{suite}.failures"));
    let body = std::fs::read_to_string(path).ok()?;
    Some(
        body.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split('\t').next().unwrap_or(l).trim().to_owned())
            .collect(),
    )
}

fn main() {
    let Some(root) = testsuite_root() else {
        eprintln!("no test suites found — run scripts/fetch-testsuites.sh first");
        return;
    };
    let sparql = root.join("sparql");
    suite(
        "SPARQL 1.1",
        &sparql.join("sparql11").join("manifest-all.ttl"),
    );
    suite("SPARQL 1.2", &sparql.join("sparql12").join("manifest.ttl"));
    suite(
        "SPARQL 1.0",
        &sparql.join("sparql10").join("manifest-evaluation.ttl"),
    );
}
