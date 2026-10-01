//! Temporary directories for tests that open a tree. The tree always lists the parent of the
//! directory it starts in, so each one sits alone inside a fresh parent, never straight in `/tmp`.

use std::path::{Path, PathBuf};

pub struct TestDir {
    root: PathBuf,
    _parent: tempfile::TempDir,
}

impl TestDir {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

pub fn tempdir() -> TestDir {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    std::fs::create_dir(&root).unwrap();
    TestDir {
        root,
        _parent: parent,
    }
}
