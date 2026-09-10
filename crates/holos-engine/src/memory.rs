//! How much memory a query has taken, and the ceiling past which it is stopped.
//!
//! # The failure this exists to prevent
//!
//! A SPARQL query can ask for more memory than the machine has, and some of them do it
//! without looking like they will. `SELECT (COUNT(DISTINCT *) AS ?n) WHERE { ?s ?p ?o }`
//! reads like a counting query; `spareval` evaluates it by holding every distinct solution
//! in a hash set, so on a large store it asks for tens of gigabytes. The same is true of
//! `SELECT DISTINCT`, of `GROUP BY` over a high-cardinality key, and of `ORDER BY` without
//! a `LIMIT`.
//!
//! When that allocation fails, Rust calls `handle_alloc_error`, which **aborts the
//! process**. On a server that is the worst available outcome: one client's careless query
//! takes down every other client's in-flight work, the HTTP listener, and any write in
//! progress. It is not a crash that can be caught — by the time the allocator has failed
//! there is no unwinding to do.
//!
//! So the fix has to come *before* the allocation. This module counts live bytes as they
//! are allocated, and [`crate::options::Deadline`] samples the count and cancels the query
//! through the same token a timeout uses. Cancellation is checked once per quad the scan
//! produces (`spareval`'s `EvalDataset::internal_quads_for_pattern`), so a query
//! accumulating into a hash set is stopped between rows, while the set is still small
//! enough to free.
//!
//! # What the number means
//!
//! [`live_bytes`] is **process-wide**, not per-query: it is the sum of every live
//! allocation, including the in-memory store's own data. A ceiling applied to it directly
//! would refuse every query on a server holding a large dataset in memory, which is the
//! opposite of useful. So the guard measures a *delta* — bytes live now, less bytes live
//! when the query started — which is what "how much did this query take" means.
//!
//! Two consequences worth stating rather than discovering:
//!
//! - **Concurrent queries share the process, and each measures its own delta.** Two queries
//!   each under the limit can still exhaust the machine together. The ceiling bounds one
//!   query's appetite, not the server's total.
//! - **Freed memory counts as freed**, even if the allocator has not returned it to the
//!   operating system. That is the right measure for "will the next allocation fit", which
//!   is the question being asked.
//!
//! # Where the counting happens, and why not here
//!
//! Counting requires wrapping the global allocator, and a global allocator requires
//! `unsafe`. Every library crate in this workspace carries `#![forbid(unsafe_code)]`, which
//! is worth more than the convenience of keeping the allocator next to the counter — so
//! this module holds the arithmetic, the ceiling and the error, and the *binary* holds the
//! allocator, calling [`record_alloc`] and [`record_dealloc`] as it works. `holos-server`
//! has the one implementation, because it is the process a runaway query can take down
//! along with everybody else's work; a CLI invocation that dies takes only itself.
//!
//! [`mark_tracking`] is a separate call rather than something the allocator infers, and the
//! reason is cost: the allocator's hot path must not carry a store to a second shared
//! location just to announce that it exists. A binary that installs an allocator and forgets
//! the call gets a guard that never fires, so [`tracking`] exists for a caller to check
//! rather than assume — `holos-server` refuses to promise a limit it cannot enforce.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Live allocated bytes, maintained by the binary's allocator through [`record_alloc`].
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Whether a binary has installed a counting allocator and said so.
static TRACKING: AtomicBool = AtomicBool::new(false);

/// Bytes live when the process finished starting up, so the ceiling measures growth.
static RESTING: AtomicUsize = AtomicUsize::new(0);

/// How far past [`RESTING`] the process may grow before reads are refused. Zero disables.
static CEILING: AtomicUsize = AtomicUsize::new(0);

/// Records bytes just handed out by the allocator.
///
/// `Relaxed` is sufficient because nothing is being *ordered* by this counter: the watchdog
/// samples it, and a sample a few allocations stale is a sample taken a few microseconds
/// ago, which against a ceiling in gigabytes is not a difference. The cost is one atomic
/// read-modify-write per allocation.
#[inline]
pub fn record_alloc(bytes: usize) {
    LIVE.fetch_add(bytes, Ordering::Relaxed);
}

/// Records bytes just returned to the allocator.
#[inline]
pub fn record_dealloc(bytes: usize) {
    LIVE.fetch_sub(bytes, Ordering::Relaxed);
}

/// Records a resize, which is a difference rather than a second block.
///
/// Split out so the caller does not have to get the signed arithmetic right in `unsafe`
/// code: a `realloc` that grows a block by a byte has allocated a byte, not a block.
#[inline]
pub fn record_realloc(old_size: usize, new_size: usize) {
    if new_size >= old_size {
        LIVE.fetch_add(new_size - old_size, Ordering::Relaxed);
    } else {
        LIVE.fetch_sub(old_size - new_size, Ordering::Relaxed);
    }
}

/// Records that a binary has installed a counting global allocator.
///
/// Call once, early in `main`. Calling it without having installed the allocator makes
/// [`live_bytes`] read as a constant zero and every memory ceiling ineffective, so do not.
pub fn mark_tracking() {
    TRACKING.store(true, Ordering::Relaxed);
}

/// Whether allocation is being counted.
///
/// A guard built on a counter nobody is maintaining would silently never fire, which is a
/// worse failure than not offering the guard. Callers that promise a limit should check
/// this and say so when it is false.
#[must_use]
pub fn tracking() -> bool {
    TRACKING.load(Ordering::Relaxed)
}

/// Live allocated bytes across the process, or zero if nothing is counting.
#[must_use]
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Sets the ceiling, measured from what is live now.
///
/// Called once, after the store is open and any startup data is loaded, so the resting set
/// — the dataset, the block cache, the indexes — is the baseline rather than part of the
/// budget. `limit` of zero disables the ceiling.
pub fn set_ceiling(limit: usize) {
    RESTING.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    CEILING.store(limit, Ordering::Relaxed);
}

/// The ceiling in force, and what is live above the resting set.
#[must_use]
pub fn ceiling() -> (usize, usize) {
    let resting = RESTING.load(Ordering::Relaxed);
    (
        CEILING.load(Ordering::Relaxed),
        LIVE.load(Ordering::Relaxed).saturating_sub(resting),
    )
}

/// Whether the process has grown past its ceiling.
///
/// # Why this is read at the scan rather than sampled by a watchdog
///
/// The first version of the ceiling cancelled a query from a thread that woke every 10 ms
/// and compared the counter. It did not work, and a real crash showed why: an 8 GiB ceiling,
/// a `Vec` that had grown to 8 GiB, and a fatal request for 16 GiB. A doubling container
/// goes from *at* the ceiling to *fatally past* it in a single allocation — and it asks for
/// the new block before releasing the old, so crossing an 8 GiB ceiling needs 24 GiB in
/// hand. There is no sampling interval short enough to fit inside that step.
///
/// So the question is asked where every read already asks a question: `decide`, in the
/// dataset view, which `DESIGN.md` §14 makes the one route to the indexes. Two relaxed loads
/// per quad, against a policy decision that is already happening. An operator that buffers
/// — `ORDER BY`, a hash-join build side, `DISTINCT` — is filling that buffer *from* the
/// scan, so it is asking this question thousands of times a second while it grows.
///
/// It cannot catch a query that allocates without reading. Nothing short of a fallible
/// allocator could, and Rust's `handle_alloc_error` aborts rather than unwinding. What it
/// can do is make the ceiling bite while the query is still small enough to survive, which
/// is why the ceiling has to sit well below the memory available — see `--max-query-memory`.
#[must_use]
pub fn over_ceiling() -> bool {
    let limit = CEILING.load(Ordering::Relaxed);
    if limit == 0 {
        return false;
    }
    LIVE.load(Ordering::Relaxed)
        .saturating_sub(RESTING.load(Ordering::Relaxed))
        > limit
}

/// A query stopped for asking for more memory than it was allowed.
///
/// Carried out of the evaluator as `spareval::QueryEvaluationError::Dataset`, which is the
/// variant for "the thing underneath refused", and is rendered transparently — so a client
/// sees the numbers rather than the word `Dataset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Bytes the query had allocated beyond what was live when it started.
    pub used: usize,
    /// The ceiling it passed.
    pub limit: usize,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "query cancelled after allocating {}, over the {} limit \
             (raise or remove it with --max-query-memory)",
            human(self.used),
            human(self.limit)
        )
    }
}

impl std::error::Error for LimitExceeded {}

/// Bytes as something a person can read at a glance.
#[must_use]
pub fn human(bytes: usize) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a display figure rounded to one decimal place"
    )]
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter is only meaningful if the test binary installed the allocator, which it
    /// does not — so what is asserted here is the arithmetic and the reporting, and the
    /// end-to-end behaviour is asserted in `tests/memory_limit.rs` against a binary that
    /// does install it.
    #[test]
    fn an_uninstalled_counter_reports_that_it_is_not_tracking() {
        // `mark_tracking` is deliberately not called here: a test that flipped the flag
        // would flip it for every other test in the binary, since it is process-global.
        assert!(!tracking());
        assert_eq!(live_bytes(), 0);
    }

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(human(8 * 1024 * 1024 * 1024), "8.0 GiB");
    }

    /// The message has to name the flag, because the person reading it is deciding whether
    /// their query is wrong or the ceiling is.
    #[test]
    fn the_error_says_what_was_used_and_what_the_limit_was() {
        let message = LimitExceeded {
            used: 9 * 1024 * 1024 * 1024,
            limit: 8 * 1024 * 1024 * 1024,
        }
        .to_string();
        assert!(message.contains("9.0 GiB"), "{message}");
        assert!(message.contains("8.0 GiB"), "{message}");
        assert!(message.contains("--max-query-memory"), "{message}");
    }

    /// The accounting has to balance, and a resize has to count as a difference.
    ///
    /// A `realloc` counted as a fresh allocation is the mistake that makes a counter drift
    /// upward forever: every `Vec` that ever grew would be counted at every size it passed
    /// through, and the ceiling would fire on a query that had freed everything it took.
    #[test]
    fn the_counter_balances_and_a_resize_counts_only_the_difference() {
        let before = live_bytes();

        record_alloc(4096);
        assert_eq!(live_bytes(), before + 4096);

        record_realloc(4096, 8192);
        assert_eq!(
            live_bytes(),
            before + 8192,
            "growing must count the difference, not a second block"
        );

        record_realloc(8192, 1024);
        assert_eq!(live_bytes(), before + 1024, "shrinking must subtract");

        record_dealloc(1024);
        assert_eq!(
            live_bytes(),
            before,
            "every byte allocated was accounted for"
        );
    }
}
