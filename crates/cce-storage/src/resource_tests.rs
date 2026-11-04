//! Directory ownership checks, executed once by the unit-test target.

use std::path::Path;

use crate::test_support::resources::{TestDir, allocate};

#[test]
fn repeated_and_concurrent_labels_own_different_directories() {
    let first = TestDir::new("same-label");
    std::fs::write(first.join("sentinel"), b"original").unwrap();
    let others = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| TestDir::new("same-label")))
            .collect();
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut paths: Vec<_> = others.iter().map(|d| d.to_path_buf()).collect();
    paths.push(first.to_path_buf());
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 9);
    drop(others);
    assert_eq!(std::fs::read(first.join("sentinel")).unwrap(), b"original");
    let path = first.to_path_buf();
    drop(first);
    assert!(!path.exists());
}

#[test]
fn occupied_paths_are_preserved_and_failure_evidence_is_retained() {
    let root = TestDir::new("owner-root");
    let existing = root.join(format!("occupied-{}-0", std::process::id()));
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("sentinel"), b"keep").unwrap();
    let mut tickets = [0, 1].into_iter();
    let mut owned = allocate(&root, "occupied", || tickets.next().unwrap()).unwrap();
    assert_ne!(owned.as_ref(), existing.as_path());
    let path = owned.to_path_buf();
    owned.retain();
    drop(owned);
    assert!(path.is_dir());
    assert_eq!(std::fs::read(existing.join("sentinel")).unwrap(), b"keep");
    let failed = root.join("failed-path");
    let result = std::panic::catch_unwind(|| {
        let d = TestDir::in_root(&root, "unwind").unwrap();
        std::fs::write(&failed, d.to_string_lossy().as_bytes()).unwrap();
        panic!("intentional resource witness");
    });
    assert!(result.is_err());
    assert!(Path::new(&std::fs::read_to_string(&failed).unwrap()).is_dir());
}
