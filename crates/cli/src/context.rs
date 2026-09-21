//! Shared plumbing: root discovery, author resolution, the default review
//! name, and the one way an interactive frontend (TUI, engine, web) opens
//! its [`Application`]: from the review file when there is one, or as an
//! unsaved review when `--commit` / `--stack` name a comparison and no
//! review file exists yet.

use std::path::Path;

use ambidiff_core::git_source::GitSource;
use ambidiff_core::review::{ReviewFile, Source};
use ambidiff_core::source::SourceMode;
use ambidiff_core::store::{Store, find_review_root};
use ambidiff_core::sys::now_rfc3339;
use anyhow::{Context, Result, bail};

use crate::application::Application;
use crate::args::SourceFlags;

const NOT_FOUND: &str =
    "no .ambidiff.json found here or in any parent directory (run `ambidiff init`)";

/// Locate the review root by walking up from the current directory.
pub fn resolve_store() -> Result<Store> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let root = find_review_root(&cwd).ok_or_else(|| anyhow::anyhow!(NOT_FOUND))?;
    Ok(Store::new(root))
}

/// Open the application for a frontend. With a review file: open it, and
/// when flags were given they must agree with the file's source (a
/// disagreement fails rather than silently overriding what the review was
/// created for). Without one: flags open an unsaved review rooted at the
/// git top level (the first comment creates the file); no flags is the
/// usual "run `ambidiff init`" error.
pub fn open_application(flags: &SourceFlags) -> Result<Application> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    match find_review_root(&cwd) {
        Some(root) => {
            let store = Store::new(root);
            if let Some(wanted) = flags.source() {
                let outcome = store.load()?;
                check_source_agreement(&outcome.review.source, &wanted, &store.review_path())?;
            }
            Ok(Application::open(store))
        }
        None => match flags.source() {
            Some(source) => {
                let root = GitSource::toplevel(&cwd).unwrap_or(cwd);
                let review = ReviewFile::new(default_review_name(&root), source, &now_rfc3339());
                Ok(Application::open_ephemeral(Store::new(root), review))
            }
            None => bail!(NOT_FOUND),
        },
    }
}

/// Flags and an existing review file must select the same comparison: the
/// same commit spec, or both a stack (an absent upstream on either side
/// defers to the other).
fn check_source_agreement(existing: &Source, wanted: &Source, path: &Path) -> Result<()> {
    let agree = match (existing.mode(), wanted.mode()) {
        (SourceMode::Commit { spec: have }, SourceMode::Commit { spec: want }) => have == want,
        (SourceMode::Stack { upstream: have }, SourceMode::Stack { upstream: want }) => {
            match (have, want) {
                (Some(have), Some(want)) => have == want,
                _ => true,
            }
        }
        _ => false,
    };
    if agree {
        return Ok(());
    }
    bail!(
        "this review is configured as {}; you asked for {}. Run `ambidiff init --commit/--stack` in a fresh directory, or delete {}",
        existing.describe(),
        wanted.describe(),
        path.display()
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_agreement_table() {
        let path = Path::new("/tmp/review/.ambidiff.json");
        let ok = |have: Source, want: Source| check_source_agreement(&have, &want, path).is_ok();
        assert!(ok(Source::git_commit("HEAD"), Source::git_commit("HEAD")));
        assert!(!ok(
            Source::git_commit("HEAD"),
            Source::git_commit("HEAD~1")
        ));
        assert!(ok(Source::git_stack(None), Source::git_stack(None)));
        assert!(ok(
            Source::git_stack(Some("main".into())),
            Source::git_stack(None)
        ));
        assert!(ok(
            Source::git_stack(None),
            Source::git_stack(Some("main".into()))
        ));
        assert!(!ok(
            Source::git_stack(Some("main".into())),
            Source::git_stack(Some("dev".into()))
        ));
        assert!(!ok(
            Source::git(Some("main".into())),
            Source::git_stack(None)
        ));
        assert!(!ok(Source::git_stack(None), Source::git_commit("HEAD")));

        let err = check_source_agreement(
            &Source::git(Some("main".into())),
            &Source::git_stack(None),
            path,
        )
        .expect_err("disagree");
        let text = err.to_string();
        assert!(text.contains("configured as git base main"), "{text}");
        assert!(text.contains("asked for git stack"), "{text}");
        assert!(text.contains(".ambidiff.json"), "{text}");
    }
}
