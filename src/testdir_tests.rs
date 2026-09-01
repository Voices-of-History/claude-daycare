//! Tests for `testdir.rs`, kept in their own file so the integration test
//! binaries that `#[path]`-include the helper do not also re-run them.

use crate::testdir::{unique_dir, unique_path};
use std::collections::HashSet;
use std::path::PathBuf;

/// The property the flake violated: two fixtures taken in the same microsecond
/// must still be different directories.
#[test]
fn concurrent_fixtures_never_share_a_path() {
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..64)
                    .map(|_| unique_path("daycare-uniqueness"))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let paths: Vec<PathBuf> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    let distinct: HashSet<&PathBuf> = paths.iter().collect();
    assert_eq!(distinct.len(), paths.len(), "two fixtures shared a path");
}

/// A directory left behind by a killed run must not leak into a later fixture
/// that lands on the same recycled pid.
#[test]
fn a_recycled_path_is_handed_over_empty() {
    let dir = unique_dir("daycare-recycled");
    std::fs::write(dir.join("stale.txt"), "from a previous run").unwrap();
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

    // What a later run does when it lands on this same name.
    let reused = crate::testdir::cleared(dir.clone());
    assert!(!reused.exists(), "a stale fixture survived into a new run");

    let _ = std::fs::remove_dir_all(&dir);
}
