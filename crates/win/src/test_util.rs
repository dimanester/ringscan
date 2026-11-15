//! Scratch directories for the Windows unit tests:
//! `%TEMP%\ringscan-dev\win-tests-<pid>\<name>`, removed on drop together
//! with the per-process parent once that is empty.

use std::path::{Path, PathBuf};

pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("win-tests-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
