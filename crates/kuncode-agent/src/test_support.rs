//! Shared scaffolding for this crate's `#[cfg(test)]` modules.
//!
//! One copy of the temp-directory helper instead of per-module clones: the
//! uniqueness scheme (pid + nanosecond stamp + a process-wide counter) exists
//! precisely because parallel tests can collide on the stamp and `Drop`-delete
//! each other's directories — a fix like that must not fork per module.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::workspace::Workspace;

/// A unique, self-cleaning temp directory.
pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub(crate) fn new() -> Self {
        // pid + timestamp keep names unique across separate test-binary runs; a
        // process-wide counter keeps them unique across *parallel* tests in one
        // run, where the nanosecond stamp can collide and one test's `Drop`
        // would otherwise `remove_dir_all` another's directory.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kuncode-agent-test-{}-{stamp}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// A [`Workspace`] rooted in this directory, for the filesystem tools.
    pub(crate) async fn workspace(&self) -> Workspace {
        Workspace::new(&self.path)
            .await
            .expect("test workspace should be valid")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
