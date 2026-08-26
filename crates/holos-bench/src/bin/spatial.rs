//! What the spatial index is worth: an R-tree probe against a full scan.
//!
//! ```text
//! cargo run --release -p holos-bench --bin spatial
//! cargo run --release -p holos-bench --bin spatial -- 200000
//! ```
//!
//! §17 says "a measurement before either is called fast". This is that measurement, and it
//! compares the two things a topology relation against a constant region can do:
//!
//! * **scan** — parse every geometry in the store and run the exact predicate on it, which is
//!   what happens without an index;
//! * **probe** — ask the R-tree which bounding boxes overlap, then run the exact predicate on
//!   only those.
//!
//! Both report how many geometries actually satisfy the relation, and the two counts must
//! agree. A faster answer that is a different answer is not an optimisation, and printing
//! them side by side means the run itself checks that rather than the reader assuming it.

use geo::{Contains, Geometry, Rect};
use holos_engine::spatial::SpatialIndex;
use holos_engine::{geo_ext, Engine};
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use std::time::Instant;

fn main() {
    let scales: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let scales = if scales.is_empty() {
        vec![10_000, 50_000, 200_000]
    } else {
        scales
    };

    println!("# Spatial index\n");
    println!("A point cloud spread over 1000 x 1000 degrees, probed with a 10 x 10 window —");
    println!("about 0.01% of the extent, which is the shape of a realistic 'what is near");
    println!("here' query rather than a whole-world one.\n");
    println!(
        "| Geometries | Build | Matches | Scan | Probe | Candidates | Speed-up |\n\
         |---:|---:|---:|---:|---:|---:|---:|"
    );

    for n in scales {
        let mut engine = Engine::new();
        engine
            .bulk_load(points(n).as_bytes(), RdfFormat::Turtle, None)
            .expect("load");
        let store = engine.store();

        let started = Instant::now();
        let index = SpatialIndex::build(store).expect("build");
        let build = started.elapsed();

        // The window sits in the middle of the cloud, so it is neither empty nor everything.
        let window = Rect::new((495.0, 495.0), (505.0, 505.0));
        let region: Geometry = window.to_polygon().into();

        // --- scan: every geometry, decoded, parsed and tested -------------------------
        let started = Instant::now();
        let mut scanned = 0usize;
        let mut scan_matches = 0usize;
        let mut seen = rustc_hash::FxHashSet::default();
        for encoded in store.quads_for_pattern(None, None, None, GraphFilter::Any) {
            let object = encoded.expect("scan").object;
            if !seen.insert(object) {
                continue;
            }
            let Some(term) = store.decode_term(object).expect("decode") else {
                continue;
            };
            let Some(geometry) = geo_ext::geometry_of(&term) else {
                continue;
            };
            scanned += 1;
            if region.contains(&geometry) {
                scan_matches += 1;
            }
        }
        let scan = started.elapsed();

        // --- probe: the tree narrows, then the same exact predicate decides -----------
        let started = Instant::now();
        let candidates = index.candidates(&region);
        let mut probe_matches = 0usize;
        for term in &candidates {
            let Some(term) = store.decode_term(*term).expect("decode") else {
                continue;
            };
            let Some(geometry) = geo_ext::geometry_of(&term) else {
                continue;
            };
            // Refinement, which the index does not replace: a box that overlaps is a
            // candidate, not an answer.
            if region.contains(&geometry) {
                probe_matches += 1;
            }
        }
        let probe = started.elapsed();

        assert_eq!(
            scan_matches, probe_matches,
            "the index changed the answer at {n} geometries: scan found {scan_matches}, \
             probe found {probe_matches}"
        );

        let speedup = scan.as_secs_f64() / probe.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "| {scanned} | {:.2?} | {scan_matches} | {:.2?} | {:.2?} | {} | **{speedup:.0}x** |",
            build,
            scan,
            probe,
            candidates.len()
        );
    }

    println!("\nThe two match counts agree at every scale, which the run asserts rather than");
    println!("reports. `Candidates` is what the tree proposed; the exact predicate still ran");
    println!("on each one, and the difference between it and `Matches` is the refinement the");
    println!("index cannot do for you.");
}

/// A deterministic point cloud over a 1000 x 1000 extent.
fn points(n: usize) -> String {
    let mut turtle = String::from(
        "@prefix ex:  <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    // A cheap LCG rather than a dependency: the distribution only has to be spread out and
    // the same on every run.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    for i in 0..n {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let x = (state >> 11) as f64 / (1u64 << 53) as f64 * 1000.0;
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let y = (state >> 11) as f64 / (1u64 << 53) as f64 * 1000.0;
        turtle.push_str(&format!(
            "ex:p{i} geo:asWKT \"POINT({x:.4} {y:.4})\"^^geo:wktLiteral .\n"
        ));
    }
    turtle
}
