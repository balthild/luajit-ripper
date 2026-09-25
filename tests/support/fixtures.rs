//! The Lua sources under `tests/fixtures`.
//!
//! These are the repository's own fixtures: small hand written programs that
//! are compiled with the local LuaJIT and checked against. They are committed,
//! unlike the corpora, so a test over them runs everywhere.

use std::path::{Path, PathBuf};

use luajit_ripper::path::PathExt;

/// The directory the fixture sources live in.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The names of the fixtures, without their `.lua` extension, in path order.
pub fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("the fixtures directory should exist")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            path.has_extension("lua")
                .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// The path of one fixture source.
pub fn fixture_path(name: &str) -> PathBuf {
    fixtures_dir().join(format!("{name}.lua"))
}
