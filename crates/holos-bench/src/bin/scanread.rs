//! How far should a scan read ahead? Swept against a real store rather than guessed.
//!
//! RocksDB's adaptive readahead for an iterator starts small and doubles to a cap. That is
//! right for a local NVMe, where a read costs microseconds, and wrong for a device where it
//! costs milliseconds — and this store's reference platform is the latter, a Samsung PM871
//! mSATA SSD behind a Realtek USB bridge. A scan of one predicate's slice of an index is the
//! most sequential access the store makes, so it is where an explicit readahead should show
//! up if it shows up anywhere.
//!
//! The first sweep found nothing: every readahead from adaptive to 16 MiB landed within
//! 1.3% on the reference platform, and a CPU sample during the scan explained why — it sits
//! at exactly 1.00 core for its whole duration, so it never waits for the disk and there is
//! no stall for readahead to hide. What a CPU-bound scan might answer to is work per block,
//! so the second dimension became RocksDB's per-block checksum.
//!
//! One process, one open store, the same scan at each setting, so nothing differs between
//! the arms but the setting. Two passes: the first warms the page cache, the second is the
//! one to read — otherwise the first arm pays for warming and every later arm looks good.
//! Within a pass the arms run in order and then in reverse, and the two halves are averaged,
//! so a drift in the disk over the run lands on the arms evenly instead of favouring
//! whichever went last.
//!
//! ```text
//! scanread <store> [predicate]
//! ```

use holos_store::{GraphFilter, RocksStorage, Storage};
use oxrdf::{NamedNode, TermRef};
use std::time::Instant;

/// Readahead sizes to try, in bytes. Zero is RocksDB's own adaptive behaviour.
const SIZES: [usize; 3] = [0, 1 << 20, 8 << 20];

fn human(bytes: usize) -> String {
    match bytes {
        0 => "adaptive".to_owned(),
        b if b >= 1 << 20 => format!("{} MiB", b >> 20),
        b => format!("{} KiB", b >> 10),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let store_path = args.next().ok_or("scanread <store> [predicate]")?;
    let predicate = args
        .next()
        .unwrap_or_else(|| "http://schema.org/deathDate".to_owned());

    // One handle, reconfigured between arms rather than reopened: a reopen would change
    // what RocksDB has cached and make the arms incomparable.
    let mut storage = RocksStorage::open(&store_path)?;
    let predicate_id = storage
        .lookup(TermRef::from(&NamedNode::new(predicate.clone())?))?
        .ok_or("the store has never seen that predicate")?;

    println!("{store_path}, predicate {predicate}\n");

    // One timed scan of the whole predicate slice at the current setting.
    let scan = |storage: &RocksStorage| -> (u64, f64) {
        let started = Instant::now();
        let rows = storage
            .scan(None, Some(predicate_id), None, GraphFilter::Default)
            .count() as u64;
        (rows, started.elapsed().as_secs_f64())
    };

    for verify in [true, false] {
        println!(
            "=== checksums {} ===",
            if verify { "verified" } else { "skipped" }
        );
        for pass in 1..=2 {
            let mut forward = Vec::new();
            for size in SIZES {
                storage.set_scan_readahead(size);
                storage.set_scan_verify_checksums(verify);
                forward.push(scan(&storage));
            }
            let mut backward = Vec::new();
            for size in SIZES.iter().rev() {
                storage.set_scan_readahead(*size);
                storage.set_scan_verify_checksums(verify);
                backward.push(scan(&storage));
            }
            backward.reverse();

            if pass == 1 {
                println!("  pass 1 (warming), skipped");
                continue;
            }
            let mut best = (f64::MAX, 0);
            for (i, size) in SIZES.iter().enumerate() {
                let mean = (forward[i].1 + backward[i].1) / 2.0;
                assert_eq!(forward[i].0, backward[i].0, "the arms saw different rows");
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "a rate printed to two decimal places"
                )]
                let rate = forward[i].0 as f64 / mean / 1e6;
                println!(
                    "  {:>9}  {:6.2} s  (fwd {:5.2}, rev {:5.2})  {:.2} M rows/s",
                    human(*size),
                    mean,
                    forward[i].1,
                    backward[i].1,
                    rate
                );
                if mean < best.0 {
                    best = (mean, *size);
                }
            }
            println!("  best: {} at {:.2} s\n", human(best.1), best.0);
        }
    }
    Ok(())
}
