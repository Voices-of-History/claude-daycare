//! Collision-proof scratch paths for fixtures.
//!
//! Shared by both sides of the test boundary from one implementation: the unit
//! tests reach it as `#[cfg(test)] mod testdir` in `lib.rs`, the integration
//! tests `#[path]`-include this same file from `tests/support/mod.rs`. It is
//! never compiled into the shipped binary.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes fixtures taken by different threads of the same process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch path that no other fixture can be handed, under any test runner.
///
/// The process id separates concurrent test binaries; the counter separates
/// threads inside one binary. **A timestamp cannot do the second job.** macOS
/// `SystemTime` has microsecond resolution, so `pid + nanos` fixtures collided
/// whenever two tests entered the same microsecond — and the moment one of them
/// removed a file, its neighbour failed on a file it had every right to expect.
/// That was an intermittent single-test failure in roughly 1 run in 13, in
/// whichever test happened to lose the race.
///
/// The path is cleared before it is returned, so a directory left behind by a
/// killed run at a since-recycled pid cannot leak into a later fixture.
pub fn unique_path(prefix: &str) -> PathBuf {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    cleared(std::env::temp_dir().join(format!("{prefix}-{}-{seq}", std::process::id())))
}

/// Removes anything already at `path`, so a fixture never inherits a directory
/// left behind by a killed run whose pid has since been recycled.
pub fn cleared(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// [`unique_path`] plus the directory itself, for fixtures that expect one.
pub fn unique_dir(prefix: &str) -> PathBuf {
    let path = unique_path(prefix);
    std::fs::create_dir_all(&path).unwrap();
    path
}

// This file's own tests live in `testdir_tests.rs`, declared only from
// `lib.rs`. A `mod tests` here would be compiled into every integration test
// binary that `#[path]`-includes this file, and run four extra times.
