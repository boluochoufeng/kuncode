//! Shared scaffolding for the filesystem tool tests.
//!
//! A unique, self-cleaning temp directory plus a [`Workspace`] rooted in it,
//! used by every tool's `#[cfg(test)] mod tests`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::workspace::Workspace;

pub(super) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub(super) fn new() -> Self {
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
            "kuncode-filesystem-tool-test-{}-{stamp}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self { path }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) async fn workspace(&self) -> Workspace {
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
