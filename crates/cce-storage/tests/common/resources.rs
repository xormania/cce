//! Test-owned directories shared by unit and integration tests.

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Declare before dependent stores and drivers, so their handles close before cleanup.
pub struct TestDir {
    path: PathBuf,
    retained: bool,
}

impl TestDir {
    pub fn new(label: &str) -> Self {
        Self::in_root(&std::env::temp_dir().join("cce-e2e"), label)
            .expect("allocate test directory")
    }

    pub fn in_root(root: &Path, label: &str) -> io::Result<Self> {
        allocate(root, label, || NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Keep evidence when a failure was captured instead of unwinding this scope.
    pub fn retain(&mut self) {
        self.retained = true;
    }
}

fn allocate(root: &Path, label: &str, mut next: impl FnMut() -> u64) -> io::Result<TestDir> {
        std::fs::create_dir_all(root)?;
        let label: String = label.chars().take(80).map(|c| {
            if c.is_ascii_alphanumeric() { c } else { '-' }
        }).collect();
        loop {
            let n = next();
            let path = root.join(format!("{label}-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TestDir { path, retained: false }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path { &self.path }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path { &self.path }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if self.retained || std::thread::panicking() {
            eprintln!("test directory kept for inspection: {}", self.path.display());
        } else if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!("could not remove test directory {}: {e}", self.path.display());
        }
    }
}

pub fn test_dir(label: &str) -> TestDir { TestDir::new(label) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_and_concurrent_labels_own_different_directories() {
        let first = TestDir::new("same-label");
        std::fs::write(first.join("sentinel"), b"original").unwrap();
        let others = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8).map(|_| scope.spawn(|| TestDir::new("same-label"))).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect::<Vec<_>>()
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
}
