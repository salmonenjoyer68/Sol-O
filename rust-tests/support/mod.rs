//! Shared test support for the black-box API tests.

pub mod fake_provider;

use std::path::{Path, PathBuf};

/// An empty env file inside the test's temp dir, so a developer `.env` never
/// leaks into a black-box server.
pub fn hermetic_env_file(database: &Path) -> PathBuf {
    let path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("empty.env");
    std::fs::write(&path, []).expect("write empty test env file");
    path
}
