//! Shared plumbing: root discovery, author resolution, and the default
//! review name. Every transport (including the TUI) goes through
//! `Application` for review state and git access; this module only resolves
//! the review root and the comment author.

use ambidiff_core::store::{Store, find_review_root};
use anyhow::{Context, Result};

/// Locate the review root by walking up from the current directory.
pub fn resolve_store() -> Result<Store> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let root = find_review_root(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "no .ambidiff.json found here or in any parent directory (run `ambidiff init`)"
        )
    })?;
    Ok(Store::new(root))
}

/// Author for CLI-created comments: $AMBIDIFF_AUTHOR, then $USER, then a
/// fixed fallback.
pub fn resolve_author(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("AMBIDIFF_AUTHOR").ok())
        .or_else(|| std::env::var("USER").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Default review name: the root directory's basename.
pub fn default_review_name(root: &std::path::Path) -> String {
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("review")
        .to_string()
}
