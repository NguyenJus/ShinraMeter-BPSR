//! Shared test-only helpers used across the workspace's crate test suites.
//! Not linked into any production binary — every consuming crate pulls this
//! in only as a `dev-dependency`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, never-yet-existing path under the OS temp dir, unique per call
/// (even across threads and across the crates sharing this counter) so
/// parallel tests never collide. `label` is folded into the filename purely
/// to make a failing test's leftover file identifiable by eye.
pub fn scratch_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bpsr-test-{label}-{n}.json"))
}
