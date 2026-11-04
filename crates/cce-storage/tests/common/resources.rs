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

pub(crate) fn allocate(root: &Path, label: &str, mut next: impl FnMut() -> u64) -> io::Result<TestDir> {
    std::fs::create_dir_all(root)?;
    let label: String = label
        .chars()
        .take(80)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    loop {
        let n = next();
        let path = root.join(format!("{label}-{}-{n}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                return Ok(TestDir {
                    path,
                    retained: false,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if self.retained || std::thread::panicking() {
            eprintln!(
                "test directory kept for inspection: {}",
                self.path.display()
            );
        } else if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "could not remove test directory {}: {e}",
                self.path.display()
            );
        }
    }
}

pub fn test_dir(label: &str) -> TestDir {
    TestDir::new(label)
}
