//! Non-unix stand-in for `rootio`: every read reports `Unsupported` so a
//! native build on a platform without `openat`/`readlinkat` fails loudly
//! instead of reading the wrong file (there is no safe root-scoped walk to
//! fall back to). The wasm32 build never reaches this file: it never
//! enables `native`.

use std::io;
use std::path::Path;

pub use crate::source::PathReason;

/// A file, or the link text of a symlink entry, read from the review root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootEntry {
    File(Vec<u8>),
    Symlink(String),
}

/// Failure reading a root-relative path.
#[derive(Debug)]
pub enum RootError {
    InvalidPath(PathReason),
    NotRegularFile,
    Io(io::Error),
}

fn unsupported() -> RootError {
    RootError::Io(io::Error::other(
        "root-relative filesystem reads are unsupported on this platform",
    ))
}

/// Split and validate a root-relative path string into components. Pure, so
/// it matches the unix implementation even where reads themselves are
/// unsupported.
pub fn check_relative_path(path: &str) -> Result<Vec<&str>, PathReason> {
    if path.is_empty() {
        return Err(PathReason::Empty);
    }
    if path.contains('\0') {
        return Err(PathReason::ContainsNul);
    }
    if path.starts_with('/') {
        return Err(PathReason::Absolute);
    }
    if path.ends_with('/') {
        return Err(PathReason::TrailingSlash);
    }
    let mut parts = Vec::new();
    for component in path.split('/') {
        match component {
            "" => return Err(PathReason::EmptyComponent),
            "." => return Err(PathReason::CurrentComponent),
            ".." => return Err(PathReason::ParentComponent),
            other => parts.push(other),
        }
    }
    Ok(parts)
}

/// An open handle on the review root. Always unsupported on this platform.
pub struct ReviewRoot;

impl ReviewRoot {
    pub fn open(_root: &Path) -> io::Result<Self> {
        Err(io::Error::other(
            "root-relative filesystem reads are unsupported on this platform",
        ))
    }

    pub fn read(&self, rel: &str, _limit: u64) -> Result<Option<RootEntry>, RootError> {
        check_relative_path(rel).map_err(RootError::InvalidPath)?;
        Err(unsupported())
    }

    pub fn size(&self, rel: &str) -> Result<Option<u64>, RootError> {
        check_relative_path(rel).map_err(RootError::InvalidPath)?;
        Err(unsupported())
    }

    pub fn read_with(
        &self,
        rel: &str,
        _limit: u64,
        _sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<Option<()>, RootError> {
        check_relative_path(rel).map_err(RootError::InvalidPath)?;
        Err(unsupported())
    }
}
