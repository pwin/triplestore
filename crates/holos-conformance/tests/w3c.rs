//! The W3C conformance run, as a ratchet.
//!
//! Each suite has a checked-in list of tests known to fail. The run fails if a test that
//! was passing starts failing, **and** if a test on the list starts passing — the second
//! direction matters as much as the first, because a stale list is a list nobody trusts.
//!
//! To re-baseline after deliberate work:
//!
//! ```text
//! HOLOS_UPDATE_CONFORMANCE=1 cargo test -p holos-conformance
//! ```
//!
//! The suites are not committed to this tree. Fetch them with `scripts/fetch-testsuites.sh` (or the
//! `.ps1`); without them these tests skip rather than fail, so a fresh checkout still
//! builds and tests green.

use holos_conformance::{manifest, run_rdf_test, run_sparql_test, testsuite_root, Outcome, Report};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the known-failure lists live.
fn baseline_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is two levels below the workspace root")
        .join("conformance")
}

fn run_suite(
    name: &str,
    manifest_path: &Path,
    runner: fn(&manifest::TestEntry) -> Outcome,
) -> Option<Report> {
    if !manifest_path.is_file() {
        eprintln!("skipping {name}: {} is absent", manifest_path.display());
        return None;
    }
    let tests = manifest::load(manifest_path)
        .unwrap_or_else(|e| panic!("{name}: could not read manifests: {e:#}"));
    let mut report = Report::default();
    for test in &tests {
        report.record(test, runner(test));
    }
    Some(report)
}

/// Compares a run against its checked-in baseline, and fails on drift in either direction.
fn ratchet(name: &str, report: &Report) {
    let path = baseline_dir().join(format!("{name}.failures"));
    let actual: BTreeSet<String> = report.failed.iter().map(|(id, _)| id.clone()).collect();

    if std::env::var("HOLOS_UPDATE_CONFORMANCE").is_ok() {
        let mut body = format!(
            "# {name} — tests known to fail. Regenerate with HOLOS_UPDATE_CONFORMANCE=1.\n\
             # {}\n",
            report.summary()
        );
        for (id, why) in &report.failed {
            body.push_str(&format!("{id}\t{}\n", why.replace(['\n', '\t'], " ")));
        }
        std::fs::create_dir_all(baseline_dir()).expect("creating the baseline directory");
        std::fs::write(&path, body).expect("writing the baseline");
        eprintln!("{name}: baseline rewritten — {}", report.summary());
        return;
    }

    let expected: BTreeSet<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split('\t').next().unwrap_or(l).trim().to_owned())
        .collect();

    let regressions: Vec<_> = actual.difference(&expected).cloned().collect();
    let fixed: Vec<_> = expected.difference(&actual).cloned().collect();

    println!("\n{name}: {}", report.summary());
    if !report.failed.is_empty() {
        println!("{}", report.failure_detail(12));
    }

    assert!(
        regressions.is_empty(),
        "{name}: {} test(s) regressed:\n{}\n\nFull detail above. If these are expected, \
         re-baseline with HOLOS_UPDATE_CONFORMANCE=1.",
        regressions.len(),
        regressions
            .iter()
            .map(|id| format!("  {id}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        fixed.is_empty(),
        "{name}: {} test(s) on the known-failure list now pass. Re-baseline with \
         HOLOS_UPDATE_CONFORMANCE=1 so the list keeps meaning something:\n{}",
        fixed.len(),
        fixed
            .iter()
            .map(|id| format!("  {id}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// RDF 1.2 — the suite that exercises triple terms, and therefore the part of the term
/// encoding `DESIGN.md` §5 warns is expensive to get wrong later.
#[test]
fn rdf12_store_round_trip() {
    let Some(root) = testsuite_root() else {
        eprintln!("skipping: run scripts/fetch-testsuites.sh first");
        return;
    };
    let Some(report) = run_suite(
        "rdf12",
        &root.join("rdf").join("rdf12").join("manifest.ttl"),
        run_rdf_test,
    ) else {
        return;
    };
    ratchet("rdf12", &report);
}

/// RDF 1.1 — the literal zoo, in bulk.
#[test]
fn rdf11_store_round_trip() {
    let Some(root) = testsuite_root() else {
        return;
    };
    let Some(report) = run_suite(
        "rdf11",
        &root.join("rdf").join("rdf11").join("manifest.ttl"),
        run_rdf_test,
    ) else {
        return;
    };
    ratchet("rdf11", &report);
}

/// SPARQL 1.1 — query evaluation through the dataset view.
#[test]
fn sparql11_query_evaluation() {
    let Some(root) = testsuite_root() else {
        return;
    };
    let Some(report) = run_suite(
        "sparql11",
        &root.join("sparql").join("sparql11").join("manifest-all.ttl"),
        run_sparql_test,
    ) else {
        return;
    };
    ratchet("sparql11", &report);
}

/// SPARQL 1.2 — triple terms in queries, `VERSION`, base direction.
#[test]
fn sparql12_query_evaluation() {
    let Some(root) = testsuite_root() else {
        return;
    };
    let Some(report) = run_suite(
        "sparql12",
        &root.join("sparql").join("sparql12").join("manifest.ttl"),
        run_sparql_test,
    ) else {
        return;
    };
    ratchet("sparql12", &report);
}

/// The RDF suites again, this time round-tripping through RocksDB.
///
/// `DESIGN.md` §12 rates two storage implementations the most likely source of silent
/// corruption. Running the same 2,447 tests through the persistent tier is the cheapest
/// possible answer: any divergence in the term encoding, the key ordering or the nine
/// column families shows up as a failure here and nowhere else.
#[cfg(feature = "rocksdb")]
#[test]
fn rdf_store_round_trip_on_rocksdb() {
    use holos_conformance::{run_rdf_test_on, Tier};
    let Some(root) = testsuite_root() else {
        return;
    };
    for (name, relative) in [("rdf11", "rdf11"), ("rdf12", "rdf12")] {
        let Some(report) = run_suite(
            name,
            &root.join("rdf").join(relative).join("manifest.ttl"),
            |t| run_rdf_test_on(t, Tier::RocksDb),
        ) else {
            continue;
        };
        // Held to the same baseline as the in-memory run: the tiers must not differ.
        ratchet(name, &report);
    }
}

/// The W3C SHACL Core suite — `DESIGN.md` §8.
///
/// Each test file holds its own data graph, shapes graph and expected validation report,
/// so this exercises the whole L4 path: compile the shapes once, validate against the
/// store's native indexes, render a deterministic report, compare up to isomorphism.
fn run_shacl_suite(name: &str, relative: &str) {
    use holos_conformance::shacl;
    let Some(root) = shacl::suite_root() else {
        eprintln!("skipping {name}: run scripts/fetch-testsuites.sh first");
        return;
    };
    let manifest = root.join(relative).join("tests").join("core").join("manifest.ttl");
    if !manifest.is_file() {
        eprintln!("skipping {name}: {} is absent", manifest.display());
        return;
    }
    let tests = shacl::load(&manifest)
        .unwrap_or_else(|e| panic!("{name}: could not read manifests: {e:#}"));
    let mut report = Report::default();
    for test in &tests {
        report.record_named(&test.id, shacl::run(test));
    }
    ratchet(name, &report);
}

#[test]
fn shacl_core() {
    run_shacl_suite("shacl-core", "data-shapes-test-suite");
}

#[test]
fn shacl12_core() {
    run_shacl_suite("shacl12-core", "shacl12-test-suite");
}

/// The same SHACL suites, through the adapted engine rather than the native evaluator.
///
/// Running both against the same expectations is the only honest way to compare them:
/// the number that matters is not "how many does each pass" in isolation but how much
/// coverage the adapted engine actually adds for the bridging cost it charges.
fn run_shacl_suite_with(name: &str, relative: &str, engine: holos_conformance::shacl::Engine) {
    use holos_conformance::shacl;
    let Some(root) = shacl::suite_root() else {
        return;
    };
    let manifest = root.join(relative).join("tests").join("core").join("manifest.ttl");
    if !manifest.is_file() {
        return;
    }
    let tests = shacl::load(&manifest)
        .unwrap_or_else(|e| panic!("{name}: could not read manifests: {e:#}"));
    let mut report = Report::default();
    for test in &tests {
        report.record_named(&test.id, shacl::run_with(test, engine));
    }
    ratchet(name, &report);
}

#[test]
fn shacl_core_adapted() {
    run_shacl_suite_with(
        "shacl-core-adapted",
        "data-shapes-test-suite",
        holos_conformance::shacl::Engine::Adapted,
    );
}

#[test]
fn shacl12_core_adapted() {
    run_shacl_suite_with(
        "shacl12-core-adapted",
        "shacl12-test-suite",
        holos_conformance::shacl::Engine::Adapted,
    );
}
