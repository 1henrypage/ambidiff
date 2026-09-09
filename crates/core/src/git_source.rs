//! Git diff source: shells out to git with the hardening the inspo tools
//! earned the hard way.
//!
//! Every command runs through one hardened runner (`cwd = root`,
//! `LC_ALL=C GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0`, `--no-pager
//! --literal-pathspecs`, a killer thread enforcing `SourceLimits::git_deadline`)
//! and reads paths exclusively from `-z` NUL-terminated listings (never from
//! patch headers). Rename detection is forced (`-M`) so old paths always
//! populate regardless of user config. A numstat preflight, plus a raw-byte
//! budget on the patch text itself, skips diffs that would swamp the
//! viewer. The comparison (which two endpoints a diff is taken between) is
//! resolved once per [`GitSource::open`] and cached; [`GitSource::try_signature`]
//! re-resolves independently, without disturbing that cache, so watch
//! polling reacts to a moved `HEAD` even though the open session's own view
//! stays stable for its lifetime.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use crate::model::{FileDiff, FileDiffKind, FileEntry, FileStatus};
use crate::parser::parse_file_diff;
use crate::review::Side;
use crate::rootio::{self, ReviewRoot, RootError};
use crate::source::{
    Comparison, DiffSource, Endpoint, FileDiffRequest, Listing, SkipReason, SkippedPath,
    SourceError,
};

/// Size and time budgets a [`GitSource`] enforces so one file, one signature
/// pass, or one hung git process cannot swamp the viewer.
#[derive(Debug, Clone)]
pub struct SourceLimits {
    /// Total changed lines (numstat preflight) above which a file's diff is
    /// reported as too large rather than parsed.
    pub changed_lines: u64,
    /// Byte size above which an untracked file is skipped as too large (its
    /// listing entry still appears; content and diff do not).
    pub untracked_bytes: u64,
    /// Byte size above which a diff's raw patch text (or an object read) is
    /// abandoned as too large, even when the numstat preflight let it
    /// through (a single enormous line has few "lines" but many bytes).
    pub raw_bytes: u64,
    /// How long any single git invocation may run before it is killed.
    pub git_deadline: Duration,
}

impl Default for SourceLimits {
    fn default() -> Self {
        SourceLimits {
            changed_lines: 10_000,
            untracked_bytes: 2 * 1024 * 1024,
            raw_bytes: 8 * 1024 * 1024,
            git_deadline: Duration::from_secs(10),
        }
    }
}

/// Comparison plus the repo-root prefix, resolved once and cached for the
/// life of a `GitSource` (see the module docs on why `try_signature`
/// re-resolves independently instead of refreshing this cache).
#[derive(Debug, Clone)]
struct Resolved {
    /// Repo-root-relative prefix of the review root (e.g. `"sub/dir/"`,
    /// empty at the repo top), from `git rev-parse --show-prefix`. Object
    /// reads (`ls-tree`, `ls-files --stage`) need repo-relative paths;
    /// `--relative` listings and diff pathspecs are already root-relative
    /// because `cwd = root`.
    prefix: String,
    comparison: Comparison,
}

static DEFAULT_COMPARISON: LazyLock<Comparison> = LazyLock::new(|| Comparison {
    old: Endpoint::Index,
    new: Endpoint::Worktree,
});

/// Git-backed diff source rooted at a working tree (or worktree checkout).
#[derive(Debug, Clone)]
pub struct GitSource {
    root: PathBuf,
    /// The comparison spec as given: `None` diffs the working tree against
    /// the index; `Some("REF")` diffs the working tree against REF;
    /// `Some("A..B")` / `Some("A...B")` diff ranges.
    base: Option<String>,
    staged: bool,
    limits: SourceLimits,
    git: PathBuf,
    resolved: Arc<OnceLock<Resolved>>,
}

impl GitSource {
    /// Open a git-backed source, resolving the comparison and repo prefix
    /// now (a bad ref, an orphan `...` range, or a non-repository root all
    /// surface here rather than on first use).
    pub fn open(
        root: impl Into<PathBuf>,
        base: Option<String>,
        staged: bool,
    ) -> Result<GitSource, SourceError> {
        let root = root.into();
        let base = base.filter(|b| !b.is_empty());
        let git = PathBuf::from("git");
        let limits = SourceLimits::default();
        let resolved = resolve_now(&root, &git, &base, staged, limits.git_deadline)?;
        let cell = OnceLock::new();
        let _ = cell.set(resolved);
        Ok(GitSource {
            root,
            base,
            staged,
            limits,
            git,
            resolved: Arc::new(cell),
        })
    }

    pub fn with_limits(mut self, limits: SourceLimits) -> GitSource {
        self.limits = limits;
        self
    }

    /// Override the git program (a path or a wrapper script); a test hook
    /// for exercising `git_deadline`.
    pub fn with_git_program(mut self, program: impl Into<PathBuf>) -> GitSource {
        self.git = program.into();
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True when `root` is inside a git working tree.
    pub fn is_repo(root: &Path) -> bool {
        is_repo_with(Path::new("git"), root)
    }

    fn resolved(&self) -> Result<&Resolved, SourceError> {
        if let Some(r) = self.resolved.get() {
            return Ok(r);
        }
        let r = resolve_now(
            &self.root,
            &self.git,
            &self.base,
            self.staged,
            self.limits.git_deadline,
        )?;
        let _ = self.resolved.set(r);
        Ok(self
            .resolved
            .get()
            .unwrap_or_else(|| unreachable_resolved()))
    }

    /// The resolved comparison. Compat-shaped as infallible: a resolution
    /// failure on the lazy (`new`) path falls back to the default
    /// `index..worktree` comparison rather than panicking.
    pub fn comparison(&self) -> &Comparison {
        self.resolved()
            .map(|r| &r.comparison)
            .unwrap_or(&DEFAULT_COMPARISON)
    }

    pub fn prefix(&self) -> &str {
        self.resolved().map(|r| r.prefix.as_str()).unwrap_or("")
    }

    fn root_reader(&self) -> Result<ReviewRoot, SourceError> {
        ReviewRoot::open(&self.root).map_err(|e| SourceError::Io {
            context: format!("open review root {}", self.root.display()),
            source: e,
        })
    }

    fn diff_prefix_args(&self) -> Vec<String> {
        vec![
            "-c".into(),
            "core.quotePath=true".into(),
            "diff".into(),
            "--no-color".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--relative".into(),
            "-M".into(),
        ]
    }

    fn run(&self, args: &[String]) -> Result<RunResult, SourceError> {
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        let cap = self.limits.raw_bytes;
        let context = args.join(" ");
        let result = run_git(
            &self.git,
            &self.root,
            &arg_refs,
            self.limits.git_deadline,
            None,
            &mut |chunk| {
                if out.len() as u64 + chunk.len() as u64 > cap {
                    return Err(SourceError::TooLarge {
                        context: context.clone(),
                        limit_bytes: cap,
                    });
                }
                out.extend_from_slice(chunk);
                Ok(())
            },
        )?;
        Ok(RunResult {
            status_ok: result.status_ok,
            stderr: result.stderr,
            stdout: out,
        })
    }

    fn run_ok_bytes(&self, args: &[String]) -> Result<Vec<u8>, SourceError> {
        let result = self.run(args)?;
        if !result.status_ok {
            return Err(SourceError::GitFailed {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
            });
        }
        Ok(result.stdout)
    }

    fn is_tracked(&self, path: &str) -> Result<bool, SourceError> {
        let out = self.run_ok_bytes(&[
            "ls-files".into(),
            "-z".into(),
            "--".into(),
            path.to_string(),
        ])?;
        Ok(!split_nul(&out).is_empty())
    }

    /// `path` is root-relative: `ls-files` resolves pathspecs against the
    /// cwd, which is the review root.
    fn blob_oid_from_index(&self, path: &str) -> Result<Option<String>, SourceError> {
        let out = self.run_ok_bytes(&[
            "ls-files".into(),
            "-z".into(),
            "--stage".into(),
            "--".into(),
            path.to_string(),
        ])?;
        Ok(first_field_after_tab(&out, 1))
    }

    /// `path` is root-relative: without `--full-tree`, `ls-tree` resolves
    /// pathspecs against the cwd like every other command.
    fn blob_oid_from_tree(
        &self,
        tree_ish: &str,
        path: &str,
    ) -> Result<Option<String>, SourceError> {
        let out = self.run_ok_bytes(&[
            "ls-tree".into(),
            "-z".into(),
            tree_ish.to_string(),
            "--".into(),
            path.to_string(),
        ])?;
        Ok(first_field_after_tab(&out, 2))
    }

    /// Stream a blob's content in bounded chunks; `cap` bounds total bytes
    /// read (a `TooLarge` past it), never the whole blob buffered at once.
    fn stream_blob(
        &self,
        oid: &str,
        cap: u64,
        mut consume: impl FnMut(&[u8]) + Send,
    ) -> Result<(), SourceError> {
        let args = ["cat-file".to_string(), "blob".to_string(), oid.to_string()];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut total = 0u64;
        let context = format!("read blob {oid}");
        let result = run_git(
            &self.git,
            &self.root,
            &arg_refs,
            self.limits.git_deadline,
            None,
            &mut |chunk| {
                total += chunk.len() as u64;
                if total > cap {
                    return Err(SourceError::TooLarge {
                        context: context.clone(),
                        limit_bytes: cap,
                    });
                }
                consume(chunk);
                Ok(())
            },
        )?;
        if !result.status_ok {
            return Err(SourceError::GitFailed {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Stream a path's content on one side, wherever it lives (a commit
    /// tree, the index, or the working tree). Returns `false` when the path
    /// does not exist on that side. Every root-relative path is validated
    /// before any syscall or git invocation runs, on every endpoint kind,
    /// not only the working tree.
    fn stream_side(
        &self,
        endpoint: &Endpoint,
        path: &str,
        cap: u64,
        mut consume: impl FnMut(&[u8]) + Send,
    ) -> Result<bool, SourceError> {
        if let Err(reason) = rootio::check_relative_path(path) {
            return Err(SourceError::InvalidPath {
                path: path.to_string(),
                reason,
            });
        }
        match endpoint {
            Endpoint::Worktree => {
                let root = self.root_reader()?;
                match root
                    .read(path, cap)
                    .map_err(|e| root_err_to_source(e, path))?
                {
                    None => Ok(false),
                    Some(entry) => {
                        let bytes = match entry {
                            rootio::RootEntry::File(bytes) => bytes,
                            // A symlink entry reads as its link text, matching
                            // how Git itself treats a symlink blob.
                            rootio::RootEntry::Symlink(text) => text.into_bytes(),
                        };
                        if bytes.len() as u64 > cap {
                            return Err(SourceError::TooLarge {
                                context: format!("read {path}"),
                                limit_bytes: cap,
                            });
                        }
                        consume(&bytes);
                        Ok(true)
                    }
                }
            }
            Endpoint::Index => {
                // Pathspecs resolve against the cwd (the review root), for
                // `ls-files` and `ls-tree` alike; the repo prefix is NOT
                // prepended (from a subdirectory root that finds nothing).
                match self.blob_oid_from_index(path)? {
                    Some(oid) => {
                        self.stream_blob(&oid, cap, consume)?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            Endpoint::Commit { oid } | Endpoint::EmptyTree { oid } => {
                match self.blob_oid_from_tree(oid, path)? {
                    Some(blob_oid) => {
                        self.stream_blob(&blob_oid, cap, consume)?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
        }
    }

    fn read_side_impl(&self, side: Side, path: &str) -> Result<Option<String>, SourceError> {
        let endpoint = self.resolved()?.comparison.endpoint(side).clone();
        let mut buf = Vec::new();
        let found = self.stream_side(&endpoint, path, self.limits.raw_bytes, |chunk| {
            buf.extend_from_slice(chunk);
        })?;
        if !found {
            return Ok(None);
        }
        Ok(String::from_utf8(buf).ok())
    }

    fn read_side_lines_impl(
        &self,
        side: Side,
        path: &str,
        first: u32,
        last: u32,
    ) -> Result<Option<Vec<String>>, SourceError> {
        let content = self.read_side_impl(side, path)?;
        Ok(content.map(|s| {
            s.lines()
                .enumerate()
                .filter_map(|(i, line)| {
                    let n = i as u32 + 1;
                    (n >= first && n <= last).then(|| line.to_string())
                })
                .collect()
        }))
    }

    /// Line count of one side's blob, streamed (never buffered whole), for
    /// the trailing gap row after the last hunk. Best-effort: any failure
    /// (including the object being too large to bother counting) yields
    /// `None` rather than failing the surrounding diff.
    fn old_total_lines(&self, req: &FileDiffRequest) -> Option<u32> {
        let endpoint = self.resolved().ok()?.comparison.old.clone();
        let path = req.path_on(Side::Old);
        let mut counter = LineCounter::default();
        let found = self
            .stream_side(&endpoint, path, self.limits.raw_bytes, |chunk| {
                counter.feed(chunk);
            })
            .ok()?;
        found.then(|| counter.total() as u32)
    }

    fn untracked_diff_raw(&self, req: &FileDiffRequest) -> Result<RawDiff, SourceError> {
        let root = self.root_reader()?;
        let size = root
            .size(&req.path)
            .map_err(|e| root_err_to_source(e, &req.path))?;
        let Some(size) = size else {
            return Ok(RawDiff::TooLarge { adds: 0, dels: 0 });
        };
        if size > self.limits.untracked_bytes {
            return Ok(RawDiff::TooLarge { adds: 0, dels: 0 });
        }
        let mut content = Vec::new();
        root.read_with(&req.path, self.limits.untracked_bytes, &mut |chunk| {
            content.extend_from_slice(chunk);
            Ok(())
        })
        .map_err(|e| root_err_to_source(e, &req.path))?;

        let args = [
            "-c".to_string(),
            "core.quotePath=true".to_string(),
            "diff".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            "--no-index".to_string(),
            format!("-U{}", req.context),
            "--".to_string(),
            "/dev/null".to_string(),
            "-".to_string(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        // `git diff --no-index` exits 1 when the files differ: the normal case.
        let result = run_git(
            &self.git,
            &self.root,
            &arg_refs,
            self.limits.git_deadline,
            Some(&content),
            &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            },
        )?;
        if !result.status_ok && out.is_empty() {
            return Err(SourceError::GitFailed {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
            });
        }
        Ok(RawDiff::Text {
            raw: String::from_utf8_lossy(&out).into_owned(),
            old_total_lines: None,
        })
    }

    fn listing_impl(&self) -> Result<Listing, SourceError> {
        let comparison = self.resolved()?.comparison.clone();

        let mut ns_args = self.diff_prefix_args();
        ns_args.extend(endpoint_args(&comparison));
        ns_args.extend(["--name-status".into(), "-z".into(), "--".into()]);
        let ns_bytes = self.run_ok_bytes(&ns_args)?;
        let (mut entries, mut skipped) = parse_name_status_bytes(&ns_bytes);

        let mut num_args = self.diff_prefix_args();
        num_args.extend(endpoint_args(&comparison));
        num_args.extend(["--numstat".into(), "-z".into(), "--".into()]);
        let num_bytes = self.run_ok_bytes(&num_args)?;
        let counts = parse_numstat_bytes(&num_bytes);
        for entry in &mut entries {
            if let Some((adds, dels)) = counts.get(&entry.path) {
                entry.adds = *adds;
                entry.dels = *dels;
            }
        }

        if comparison.new_is_worktree() {
            let out = self.run_ok_bytes(&[
                "ls-files".into(),
                "--others".into(),
                "--exclude-standard".into(),
                "-z".into(),
            ])?;
            let root = self.root_reader()?;
            for path_bytes in split_nul(&out) {
                match std::str::from_utf8(path_bytes) {
                    Ok(path) => {
                        let adds = match root.size(path) {
                            Ok(Some(size)) if size <= self.limits.untracked_bytes => {
                                self.count_untracked_lines(&root, path).ok().flatten()
                            }
                            _ => None,
                        };
                        entries.push(FileEntry {
                            path: path.to_string(),
                            old_path: None,
                            status: FileStatus::Untracked,
                            adds,
                            dels: Some(0),
                        });
                    }
                    Err(_) => skipped.push(SkippedPath {
                        display: escape_display(path_bytes),
                        reason: SkipReason::NonUtf8,
                    }),
                }
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Listing { entries, skipped })
    }

    fn count_untracked_lines(
        &self,
        root: &ReviewRoot,
        path: &str,
    ) -> Result<Option<u64>, SourceError> {
        let mut counter = LineCounter::default();
        let mut binary = false;
        let found = root
            .read_with(path, self.limits.untracked_bytes, &mut |chunk| {
                if !binary && chunk.contains(&0) {
                    binary = true;
                }
                counter.feed(chunk);
                Ok(())
            })
            .map_err(|e| root_err_to_source(e, path))?;
        if found.is_none() || binary {
            return Ok(None);
        }
        Ok(Some(counter.total()))
    }
}

/// Raw diff material for frontends that parse in wasm (the browser). The
/// native `file_diff` parses exactly this, so both sides see identical
/// bytes (the drift firewall extends to the transport).
#[derive(Debug, Clone)]
pub enum RawDiff {
    Text {
        raw: String,
        old_total_lines: Option<u32>,
    },
    TooLarge {
        adds: u64,
        dels: u64,
    },
}

impl GitSource {
    /// Fetch one file's raw diff text with the numstat preflight applied,
    /// plus a raw-byte budget on the patch text itself (a single enormous
    /// line has few "lines" by the preflight's count but many bytes).
    pub fn file_diff_raw(&self, req: &FileDiffRequest) -> Result<RawDiff, SourceError> {
        let comparison = self.resolved()?.comparison.clone();

        if comparison.new_is_worktree() && !self.is_tracked(&req.path)? {
            let root = self.root_reader()?;
            let exists = root
                .size(&req.path)
                .map_err(|e| root_err_to_source(e, &req.path))?
                .is_some();
            if exists {
                return self.untracked_diff_raw(req);
            }
        }

        let mut num_args = self.diff_prefix_args();
        num_args.extend(endpoint_args(&comparison));
        num_args.extend(["--numstat".into(), "-z".into()]);
        num_args.extend(path_args(req));
        let counts = parse_numstat_bytes(&self.run_ok_bytes(&num_args)?);
        let preflight = counts.get(&req.path).copied().unwrap_or((None, None));
        if let (Some(adds), Some(dels)) = preflight
            && adds + dels > self.limits.changed_lines
        {
            return Ok(RawDiff::TooLarge { adds, dels });
        }

        let mut args = self.diff_prefix_args();
        args.extend(endpoint_args(&comparison));
        args.push(format!("-U{}", req.context));
        args.extend(path_args(req));
        match self.run_ok_bytes(&args) {
            Ok(raw_bytes) => Ok(RawDiff::Text {
                raw: String::from_utf8_lossy(&raw_bytes).into_owned(),
                old_total_lines: self.old_total_lines(req),
            }),
            Err(SourceError::TooLarge { .. }) => Ok(RawDiff::TooLarge {
                adds: preflight.0.unwrap_or(0),
                dels: preflight.1.unwrap_or(0),
            }),
            Err(e) => Err(e),
        }
    }
}

fn parse_diff_output(raw: &str) -> Result<FileDiff, SourceError> {
    // The parser is total over tolerated input; hunk-header overflow is the
    // only typed failure and indicates a corrupt diff worth surfacing.
    parse_file_diff(raw).map_err(|e| SourceError::GitFailed {
        args: "diff parse".into(),
        stderr: e.to_string(),
    })
}

impl DiffSource for GitSource {
    fn listing(&self) -> Result<Listing, SourceError> {
        self.listing_impl()
    }

    fn file_diff(&self, req: &FileDiffRequest) -> Result<FileDiff, SourceError> {
        match self.file_diff_raw(req)? {
            RawDiff::TooLarge { adds, dels } => Ok(FileDiff {
                kind: FileDiffKind::TooLarge { adds, dels },
                ..FileDiff::empty()
            }),
            RawDiff::Text {
                raw,
                old_total_lines,
            } => {
                let mut diff = parse_diff_output(&raw)?;
                if diff.kind == FileDiffKind::Text && !diff.hunks.is_empty() {
                    diff.old_total_lines = old_total_lines;
                }
                Ok(diff)
            }
        }
    }

    fn read_side(&self, side: Side, path: &str) -> Result<Option<String>, SourceError> {
        self.read_side_impl(side, path)
    }

    fn read_side_lines(
        &self,
        side: Side,
        path: &str,
        first: u32,
        last: u32,
    ) -> Result<Option<Vec<String>>, SourceError> {
        self.read_side_lines_impl(side, path, first, last)
    }
}

impl GitSource {
    /// Change signature: an FNV-1a hash fed streamed content, never an
    /// unbounded buffered patch. Independently re-resolves the comparison
    /// (see the module docs) so a moved `HEAD` is detected even though the
    /// open session's cached comparison stays put. The watch controller
    /// re-runs this after filesystem events and refreshes only when it
    /// moves (the gate against touch-without-change noise).
    pub fn try_signature(&self) -> Result<u64, SourceError> {
        let comparison = resolve_comparison(
            &self.root,
            &self.git,
            &self.base,
            self.staged,
            self.limits.git_deadline,
        )?;
        let mut hasher = Fnv1a::new();
        hasher.feed(b"cmp\0");
        hasher.feed(comparison.to_string().as_bytes());

        let mut args = self.diff_prefix_args();
        args.extend(endpoint_args(&comparison));
        args.push("--".into());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_git(
            &self.git,
            &self.root,
            &arg_refs,
            self.limits.git_deadline,
            None,
            &mut |chunk| {
                hasher.feed(chunk);
                Ok(())
            },
        )?;
        if !result.status_ok {
            return Err(SourceError::GitFailed {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
            });
        }

        if comparison.new_is_worktree() {
            let mut listing_bytes = Vec::new();
            let out_args = ["ls-files", "--others", "--exclude-standard", "-z"];
            run_git(
                &self.git,
                &self.root,
                &out_args,
                self.limits.git_deadline,
                None,
                &mut |chunk| {
                    listing_bytes.extend_from_slice(chunk);
                    Ok(())
                },
            )?;
            hasher.feed(&listing_bytes);

            let root = self.root_reader()?;
            for path_bytes in split_nul(&listing_bytes) {
                let Ok(path) = std::str::from_utf8(path_bytes) else {
                    continue;
                };
                hasher.feed(path.as_bytes());
                match root.size(path) {
                    Ok(Some(size)) if size <= self.limits.untracked_bytes => {
                        let _ = root.read_with(path, self.limits.untracked_bytes, &mut |chunk| {
                            hasher.feed(chunk);
                            Ok(())
                        });
                    }
                    Ok(Some(size)) => hasher.feed(format!("toolarge:{size}").as_bytes()),
                    _ => {}
                }
            }
        }
        Ok(hasher.finish())
    }

    /// Add the review file and its sidecars to `.git/info/exclude` so review
    /// state never lands in version control. Worktree-aware via
    /// `git rev-parse --git-path`. Idempotent; refuses to edit through a
    /// symlink and preserves bytes it does not own.
    pub fn ensure_git_exclude(root: &Path) -> io::Result<()> {
        let exclude_path = git_exclude_path(root)?;

        let existing = match std::fs::symlink_metadata(&exclude_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::other(format!(
                    "{} is a symlink; refusing to edit",
                    exclude_path.display()
                )));
            }
            Ok(_) => std::fs::read(&exclude_path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };

        const WANTED: [&str; 4] = [
            ".ambidiff.json",
            ".ambidiff.json.lock",
            ".ambidiff.json.guard",
            ".ambidiff.json.tmp.*",
        ];
        let existing_lines: Vec<&[u8]> = existing.split(|&b| b == b'\n').map(trim_bytes).collect();
        let missing: Vec<&str> = WANTED
            .iter()
            .copied()
            .filter(|w| !existing_lines.contains(&w.as_bytes()))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }

        let parent = match exclude_path.parent() {
            Some(p) => p,
            None => return Err(io::Error::other("git exclude path has no parent directory")),
        };
        std::fs::create_dir_all(parent)?;

        let mut content = existing;
        if !content.is_empty() && content.last() != Some(&b'\n') {
            content.push(b'\n');
        }
        for line in missing {
            content.extend_from_slice(line.as_bytes());
            content.push(b'\n');
        }

        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&content)?;
        tmp.persist(&exclude_path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn git_exclude_path(root: &Path) -> io::Result<PathBuf> {
    // `--git-path info/exclude` resolves through a symlink AT info/exclude
    // itself (git realpath-resolves the whole thing), which would silently
    // defeat the symlink guard below by handing back the resolved target
    // instead of the exclude path. Ask only for the git-common-dir (whose
    // own symlink-ness is a separate, pre-existing trust boundary) and
    // append the literal `info/exclude` component ourselves, purely
    // lexically, so a symlink planted there is the path we actually check.
    let out = run_capture_ok(
        Path::new("git"),
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        Duration::from_secs(10),
        None,
    )
    .map_err(source_error_to_io)?;
    Ok(PathBuf::from(out.trim()).join("info").join("exclude"))
}

fn trim_bytes(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|&b| !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    if start >= end { &[] } else { &line[start..end] }
}

fn source_error_to_io(e: SourceError) -> io::Error {
    match e {
        SourceError::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}

fn root_err_to_source(e: RootError, path: &str) -> SourceError {
    match e {
        RootError::InvalidPath(reason) => SourceError::InvalidPath {
            path: path.to_string(),
            reason,
        },
        RootError::NotRegularFile => SourceError::Io {
            context: format!("read {path}"),
            source: io::Error::other("not a regular file"),
        },
        RootError::Io(e) => SourceError::Io {
            context: format!("read {path}"),
            source: e,
        },
    }
}

fn path_args(req: &FileDiffRequest) -> Vec<String> {
    match &req.old_path {
        Some(old) if old != &req.path => {
            vec!["--".into(), old.clone(), req.path.clone()]
        }
        _ => vec!["--".into(), req.path.clone()],
    }
}

/// Endpoint args for the diff command: nothing for index..worktree, the
/// single oid for a commit against the worktree, `--cached <oid>` for a
/// commit against the index, both oids for a commit range. Always followed
/// by the caller's own `--`.
fn endpoint_args(cmp: &Comparison) -> Vec<String> {
    match (&cmp.old, &cmp.new) {
        (Endpoint::Index, Endpoint::Worktree) => Vec::new(),
        (old, Endpoint::Worktree) => vec![endpoint_oid(old)],
        (old, Endpoint::Index) => vec!["--cached".to_string(), endpoint_oid(old)],
        (old, new) => vec![endpoint_oid(old), endpoint_oid(new)],
    }
}

fn endpoint_oid(e: &Endpoint) -> String {
    match e {
        Endpoint::Commit { oid } | Endpoint::EmptyTree { oid } => oid.clone(),
        // Index/Worktree never appear where an oid is needed; the match
        // arms in `endpoint_args` only ever call this on Commit/EmptyTree.
        Endpoint::Index => "index".to_string(),
        Endpoint::Worktree => "worktree".to_string(),
    }
}

// ---------------------------------------------------------------------
// Comparison resolution
// ---------------------------------------------------------------------

fn resolve_now(
    root: &Path,
    git: &Path,
    base: &Option<String>,
    staged: bool,
    deadline: Duration,
) -> Result<Resolved, SourceError> {
    if !is_repo_with(git, root) {
        return Err(SourceError::NotARepo {
            root: root.display().to_string(),
        });
    }
    let prefix = run_capture_ok(git, root, &["rev-parse", "--show-prefix"], deadline, None)?
        .trim()
        .to_string();
    let comparison = resolve_comparison(root, git, base, staged, deadline)?;
    Ok(Resolved { prefix, comparison })
}

fn resolve_comparison(
    root: &Path,
    git: &Path,
    base: &Option<String>,
    staged: bool,
    deadline: Duration,
) -> Result<Comparison, SourceError> {
    if staged {
        // `git diff --cached <ref>` is well formed: the old side is the ref
        // (HEAD, or the empty tree on an unborn branch, when none is given)
        // and the new side is the index. A range has no index side to
        // compare against, so staged + range is the one refused shape.
        let old = match base.as_deref() {
            None => resolve_head_or_empty_tree(root, git, deadline)?,
            Some(spec) if spec.contains("..") => {
                return Err(SourceError::UnsupportedComparison {
                    base: spec.to_string(),
                    reason: "a staged comparison compares one ref with the index; a range has no index side".into(),
                });
            }
            Some(spec) => Endpoint::Commit {
                oid: resolve_ref(root, git, spec, deadline)?,
            },
        };
        return Ok(Comparison {
            old,
            new: Endpoint::Index,
        });
    }
    match base.as_deref() {
        None => Ok(Comparison {
            old: Endpoint::Index,
            new: Endpoint::Worktree,
        }),
        Some(spec) => {
            if let Some((l, r)) = spec.split_once("...") {
                let left_text = if l.is_empty() { "HEAD" } else { l };
                let right_text = if r.is_empty() { "HEAD" } else { r };
                let left_oid = resolve_ref(root, git, left_text, deadline)?;
                let right_oid = resolve_ref(root, git, right_text, deadline)?;
                let base_oid =
                    merge_base(root, git, &left_oid, &right_oid, deadline).map_err(|_| {
                        SourceError::NoMergeBase {
                            left: left_text.to_string(),
                            right: right_text.to_string(),
                        }
                    })?;
                Ok(Comparison {
                    old: Endpoint::Commit { oid: base_oid },
                    new: Endpoint::Commit { oid: right_oid },
                })
            } else if let Some((l, r)) = spec.split_once("..") {
                let left_text = if l.is_empty() { "HEAD" } else { l };
                let right_text = if r.is_empty() { "HEAD" } else { r };
                let left_oid = resolve_ref(root, git, left_text, deadline)?;
                let right_oid = resolve_ref(root, git, right_text, deadline)?;
                Ok(Comparison {
                    old: Endpoint::Commit { oid: left_oid },
                    new: Endpoint::Commit { oid: right_oid },
                })
            } else {
                let oid = resolve_ref(root, git, spec, deadline)?;
                Ok(Comparison {
                    old: Endpoint::Commit { oid },
                    new: Endpoint::Worktree,
                })
            }
        }
    }
}

fn resolve_ref(
    root: &Path,
    git: &Path,
    spec: &str,
    deadline: Duration,
) -> Result<String, SourceError> {
    let commit_spec = format!("{spec}^{{commit}}");
    let args = [
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        commit_spec.as_str(),
    ];
    match run_capture_ok(git, root, &args, deadline, None) {
        Ok(out) => {
            let oid = out.trim();
            if oid.is_empty() {
                Err(SourceError::InvalidRef {
                    spec: spec.to_string(),
                })
            } else {
                Ok(oid.to_string())
            }
        }
        Err(_) => Err(SourceError::InvalidRef {
            spec: spec.to_string(),
        }),
    }
}

fn merge_base(
    root: &Path,
    git: &Path,
    left: &str,
    right: &str,
    deadline: Duration,
) -> Result<String, SourceError> {
    let out = run_capture_ok(git, root, &["merge-base", left, right], deadline, None)?;
    let oid = out.trim();
    if oid.is_empty() {
        return Err(SourceError::GitFailed {
            args: format!("merge-base {left} {right}"),
            stderr: "empty output".into(),
        });
    }
    Ok(oid.to_string())
}

fn resolve_head_or_empty_tree(
    root: &Path,
    git: &Path,
    deadline: Duration,
) -> Result<Endpoint, SourceError> {
    match resolve_ref(root, git, "HEAD", deadline) {
        Ok(oid) => Ok(Endpoint::Commit { oid }),
        Err(SourceError::InvalidRef { .. }) => {
            let out = run_capture_ok(
                git,
                root,
                &["hash-object", "-t", "tree", "--stdin"],
                deadline,
                Some(&[]),
            )?;
            Ok(Endpoint::EmptyTree {
                oid: out.trim().to_string(),
            })
        }
        Err(e) => Err(e),
    }
}

fn is_repo_with(git: &Path, root: &Path) -> bool {
    let mut out = Vec::new();
    match run_git(
        git,
        root,
        &["rev-parse", "--is-inside-work-tree"],
        Duration::from_secs(10),
        None,
        &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        },
    ) {
        Ok(r) => r.status_ok && String::from_utf8_lossy(&out).trim() == "true",
        Err(_) => false,
    }
}

fn unreachable_resolved() -> &'static Resolved {
    // `resolved()` always calls `OnceLock::set` immediately before this is
    // reached; a poisoned/lost race would mean another thread's `set` won,
    // and `get()` still returns `Some`.
    static FALLBACK: LazyLock<Resolved> = LazyLock::new(|| Resolved {
        prefix: String::new(),
        comparison: Comparison {
            old: Endpoint::Index,
            new: Endpoint::Worktree,
        },
    });
    &FALLBACK
}

// ---------------------------------------------------------------------
// The hardened process runner
// ---------------------------------------------------------------------

struct RunOutput {
    status_ok: bool,
    stderr: Vec<u8>,
}

struct RunResult {
    status_ok: bool,
    stderr: Vec<u8>,
    stdout: Vec<u8>,
}

/// Run one git invocation with piped stdio in a scoped thread set: a reader
/// thread feeds stdout to `sink` in bounded chunks (never buffering the
/// whole output itself; that is `sink`'s job), a stderr thread captures
/// diagnostics, an optional stdin thread feeds `stdin_data`, and a killer
/// thread enforces `deadline` by killing the child. All threads are joined
/// before `wait`.
fn run_git(
    git: &Path,
    root: &Path,
    args: &[&str],
    deadline: Duration,
    stdin_data: Option<&[u8]>,
    sink: &mut (dyn FnMut(&[u8]) -> Result<(), SourceError> + Send),
) -> Result<RunOutput, SourceError> {
    let mut cmd = Command::new(git);
    cmd.arg("--no-pager");
    cmd.arg("--literal-pathspecs");
    cmd.args(args);
    cmd.current_dir(root);
    cmd.env("LC_ALL", "C");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let context = || format!("run {} {}", git.display(), args.join(" "));
    let mut child = cmd.spawn().map_err(|e| SourceError::Io {
        context: context(),
        source: e,
    })?;

    let mut child_stdin = child.stdin.take();
    let mut child_stdout = child.stdout.take().ok_or_else(|| SourceError::Io {
        context: context(),
        source: io::Error::other("git spawned without a stdout pipe"),
    })?;
    let mut child_stderr = child.stderr.take().ok_or_else(|| SourceError::Io {
        context: context(),
        source: io::Error::other("git spawned without a stderr pipe"),
    })?;

    let child_mutex = Mutex::new(child);
    let timed_out = AtomicBool::new(false);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // Scoped threads borrow the environment; `done_rx` and the timeout flag
    // must move into the killer thread alone (it needs sole, Send-friendly
    // ownership of the non-Sync `Receiver`), so pass `child_mutex` and
    // `timed_out` in as plain shared references rather than moving the
    // originals, which the stdout thread also needs.
    let child_ref = &child_mutex;
    let timed_out_ref = &timed_out;

    let (stdout_result, stderr_bytes) = thread::scope(|scope| {
        let stdout_handle = scope.spawn(move || -> Result<(), SourceError> {
            let mut buf = [0u8; 65536];
            loop {
                let n = child_stdout.read(&mut buf).map_err(|e| SourceError::Io {
                    context: "read git stdout".into(),
                    source: e,
                })?;
                if n == 0 {
                    return Ok(());
                }
                if let Err(e) = sink(&buf[..n]) {
                    if let Ok(mut c) = child_ref.lock() {
                        let _ = c.kill();
                    }
                    return Err(e);
                }
            }
        });
        let stderr_handle = scope.spawn(move || -> Vec<u8> {
            let mut buf = Vec::new();
            let _ = child_stderr.read_to_end(&mut buf);
            buf
        });
        let stdin_handle = stdin_data.map(|data| {
            scope.spawn(move || {
                if let Some(mut w) = child_stdin.take() {
                    let _ = w.write_all(data);
                }
                // `w` drops here, closing the pipe so a stdin-reading git
                // (`--no-index -`) sees EOF.
            })
        });
        let killer_handle = scope.spawn(move || {
            if done_rx.recv_timeout(deadline).is_err() {
                timed_out_ref.store(true, Ordering::SeqCst);
                if let Ok(mut c) = child_ref.lock() {
                    let _ = c.kill();
                }
            }
        });

        let stdout_result = stdout_handle.join().unwrap_or_else(|_| {
            Err(SourceError::Io {
                context: "git stdout reader thread panicked".into(),
                source: io::Error::other("panic"),
            })
        });
        let stderr_bytes = stderr_handle.join().unwrap_or_default();
        if let Some(h) = stdin_handle {
            let _ = h.join();
        }
        let _ = done_tx.send(());
        let _ = killer_handle.join();
        (stdout_result, stderr_bytes)
    });

    if timed_out.load(Ordering::SeqCst) {
        return Err(SourceError::Timeout {
            args: args.join(" "),
            seconds: deadline.as_secs(),
        });
    }
    stdout_result?;

    let status = child_mutex
        .into_inner()
        .map_err(|_| SourceError::Io {
            context: context(),
            source: io::Error::other("git child mutex poisoned"),
        })?
        .wait()
        .map_err(|e| SourceError::Io {
            context: context(),
            source: e,
        })?;

    Ok(RunOutput {
        status_ok: status.success(),
        stderr: stderr_bytes,
    })
}

/// One-shot capture of a small git command's stdout as text, for
/// resolution (`rev-parse`, `merge-base`, `hash-object`) where there is no
/// `GitSource` yet to hang a raw-byte cap off of.
fn run_capture_ok(
    git: &Path,
    root: &Path,
    args: &[&str],
    deadline: Duration,
    stdin_data: Option<&[u8]>,
) -> Result<String, SourceError> {
    let mut out = Vec::new();
    let result = run_git(git, root, args, deadline, stdin_data, &mut |chunk| {
        out.extend_from_slice(chunk);
        Ok(())
    })?;
    if !result.status_ok {
        return Err(SourceError::GitFailed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------
// Byte-level parsers: paths come from -z listings, never patch headers,
// and a non-UTF-8 path is reported rather than guessed at.
// ---------------------------------------------------------------------

fn split_nul(bytes: &[u8]) -> Vec<&[u8]> {
    let mut b = bytes;
    if b.last() == Some(&0) {
        b = &b[..b.len() - 1];
    }
    if b.is_empty() {
        return Vec::new();
    }
    b.split(|&c| c == 0).collect()
}

/// Escape a raw path as display text: valid UTF-8 runs pass through, each
/// invalid byte becomes `\xNN`. Never guesses at a non-UTF-8 path's meaning.
fn escape_display(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if let Ok(s) = std::str::from_utf8(&rest[..valid_up_to]) {
                    out.push_str(s);
                }
                match rest.get(valid_up_to) {
                    Some(&bad) => {
                        out.push_str(&format!("\\x{bad:02X}"));
                        rest = &rest[valid_up_to + 1..];
                    }
                    None => break,
                }
            }
        }
    }
    out
}

fn utf8_record(path_bytes: &[u8], old_bytes: Option<&[u8]>) -> Option<(String, Option<String>)> {
    let path = std::str::from_utf8(path_bytes).ok()?.to_string();
    let old = match old_bytes {
        Some(b) => Some(std::str::from_utf8(b).ok()?.to_string()),
        None => None,
    };
    Some((path, old))
}

/// Parse NUL-separated `git diff --name-status -z` output. Renames and
/// copies (`R<score>`, `C<score>`) consume two paths: origin then current.
/// A record with either path invalid as UTF-8 is skipped, not guessed at.
fn parse_name_status_bytes(bytes: &[u8]) -> (Vec<FileEntry>, Vec<SkippedPath>) {
    let fields = split_nul(bytes);
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let raw_status = fields[i];
        if raw_status.is_empty() {
            i += 1;
            continue;
        }
        i += 1;
        if i >= fields.len() {
            break;
        }
        let mut path_bytes = fields[i];
        i += 1;
        let status_char = raw_status.first().map(|&b| b as char).unwrap_or('?');
        let mut old_bytes: Option<&[u8]> = None;
        if matches!(status_char, 'R' | 'C') && i < fields.len() {
            old_bytes = Some(path_bytes);
            path_bytes = fields[i];
            i += 1;
        }
        match utf8_record(path_bytes, old_bytes) {
            Some((path, old_path)) => {
                let status = match status_char {
                    'A' => FileStatus::Added,
                    'M' => FileStatus::Modified,
                    'D' => FileStatus::Deleted,
                    'R' => FileStatus::Renamed,
                    'C' => FileStatus::Copied,
                    _ => FileStatus::Other,
                };
                entries.push(FileEntry {
                    path,
                    old_path,
                    status,
                    adds: None,
                    dels: None,
                });
            }
            None => {
                let display = match old_bytes {
                    Some(ob) => format!("{} -> {}", escape_display(ob), escape_display(path_bytes)),
                    None => escape_display(path_bytes),
                };
                skipped.push(SkippedPath {
                    display,
                    reason: SkipReason::NonUtf8,
                });
            }
        }
    }
    (entries, skipped)
}

/// Parse NUL-separated `git diff --numstat -z` output into per-path counts,
/// keyed by current path. Binary files report `-` counts, mapped to `None`.
/// With `-z`, renames emit `adds\tdels\t` then origin and current path as
/// two NUL fields. A non-UTF-8 path is silently absent from the map; its
/// name-status record already carries the reported skip.
fn parse_numstat_bytes(bytes: &[u8]) -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    let fields = split_nul(bytes);
    let mut map = BTreeMap::new();
    let mut i = 0;
    while i < fields.len() {
        let record = fields[i];
        if record.is_empty() {
            i += 1;
            continue;
        }
        let mut parts = record.splitn(3, |&c| c == b'\t');
        let adds_b = parts.next().unwrap_or(b"");
        let dels_b = parts.next().unwrap_or(b"");
        let path_b = parts.next().unwrap_or(b"");
        let adds = std::str::from_utf8(adds_b)
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let dels = std::str::from_utf8(dels_b)
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        if path_b.is_empty() {
            if i + 2 < fields.len() {
                if let Ok(p) = std::str::from_utf8(fields[i + 2]) {
                    map.insert(p.to_string(), (adds, dels));
                }
                i += 3;
                continue;
            }
            break;
        }
        if let Ok(p) = std::str::from_utf8(path_b) {
            map.insert(p.to_string(), (adds, dels));
        }
        i += 1;
    }
    map
}

/// Pull the `field_index`-th whitespace-separated token before the first tab
/// of each `-z` record (`<mode> <oid> <stage>\t<path>` for `ls-files
/// --stage`, `<mode> <type> <oid>\t<path>` for `ls-tree`), returning the
/// first record found (there is at most one for an exact pathspec).
fn first_field_after_tab(bytes: &[u8], field_index: usize) -> Option<String> {
    for record in split_nul(bytes) {
        let tab = record.iter().position(|&b| b == b'\t')?;
        let meta = std::str::from_utf8(&record[..tab]).ok()?;
        if let Some(field) = meta.split_whitespace().nth(field_index) {
            return Some(field.to_string());
        }
    }
    None
}

#[derive(Default)]
struct LineCounter {
    newline_count: u64,
    saw_any: bool,
    ended_with_newline: bool,
}

impl LineCounter {
    fn feed(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.saw_any = true;
        self.newline_count += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        self.ended_with_newline = chunk.last() == Some(&b'\n');
    }

    fn total(&self) -> u64 {
        if !self.saw_any {
            return 0;
        }
        if self.ended_with_newline {
            self.newline_count
        } else {
            self.newline_count + 1
        }
    }
}

/// Streaming FNV-1a: fed incrementally so a signature pass never buffers an
/// unbounded patch.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }

    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_status_plain_rename_and_non_utf8() {
        let mut out = b"M\0src/a.rs\0R100\0old.rs\0new.rs\0A\0added.txt\0".to_vec();
        out.extend_from_slice(b"A\0bad_");
        out.push(0xFF);
        out.push(b'\0');
        let (entries, skipped) = parse_name_status_bytes(&out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, FileStatus::Modified);
        assert_eq!(entries[0].path, "src/a.rs");
        assert_eq!(entries[1].status, FileStatus::Renamed);
        assert_eq!(entries[1].path, "new.rs");
        assert_eq!(entries[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(entries[2].status, FileStatus::Added);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::NonUtf8);
        assert!(skipped[0].display.contains("bad_"));
        assert!(skipped[0].display.contains("\\xFF"));
    }

    #[test]
    fn parse_numstat_plain_binary_and_rename() {
        let out = b"10\t2\tsrc/a.rs\0-\t-\timg.png\x005\t1\t\0old.rs\0new.rs\0";
        let map = parse_numstat_bytes(out);
        assert_eq!(map["src/a.rs"], (Some(10), Some(2)));
        assert_eq!(map["img.png"], (None, None));
        assert_eq!(map["new.rs"], (Some(5), Some(1)));
    }

    #[test]
    fn line_counter_handles_missing_trailing_newline() {
        let mut c = LineCounter::default();
        assert_eq!(c.total(), 0);
        c.feed(b"a\n");
        assert_eq!(c.total(), 1);
        let mut c2 = LineCounter::default();
        c2.feed(b"a\nb");
        assert_eq!(c2.total(), 2);
    }

    #[test]
    fn escape_display_passes_through_ascii_and_escapes_bad_bytes() {
        assert_eq!(escape_display(b"plain.txt"), "plain.txt");
        let mut bytes = b"caf\xC3\xA9".to_vec(); // valid UTF-8 "café"
        assert_eq!(escape_display(&bytes), "caf\u{e9}");
        bytes = vec![b'a', 0xFF, b'b'];
        assert_eq!(escape_display(&bytes), "a\\xFFb");
    }

    #[test]
    fn endpoint_args_per_comparison_shape() {
        assert!(
            endpoint_args(&Comparison {
                old: Endpoint::Index,
                new: Endpoint::Worktree
            })
            .is_empty()
        );
        assert_eq!(
            endpoint_args(&Comparison {
                old: Endpoint::Commit { oid: "abc".into() },
                new: Endpoint::Worktree
            }),
            vec!["abc".to_string()]
        );
        assert_eq!(
            endpoint_args(&Comparison {
                old: Endpoint::Commit { oid: "abc".into() },
                new: Endpoint::Index
            }),
            vec!["--cached".to_string(), "abc".to_string()]
        );
        assert_eq!(
            endpoint_args(&Comparison {
                old: Endpoint::Commit { oid: "a".into() },
                new: Endpoint::Commit { oid: "b".into() }
            }),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn trim_bytes_strips_ascii_whitespace_both_ends() {
        assert_eq!(trim_bytes(b"  a.txt \t"), b"a.txt");
        assert_eq!(trim_bytes(b""), b"");
        assert_eq!(trim_bytes(b"   "), b"");
    }
}
