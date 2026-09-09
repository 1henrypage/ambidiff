//! CLI verb implementations: the agent surface.
//!
//! Every verb has a --json twin with a stable schema. Exit code contract:
//! 0 clean, 1 error, and for `status` specifically 10 when actionable
//! comments (open or reopened) exist, so scripts and agent loops can branch
//! without parsing.
//!
//! Every mutation goes through [`Application::execute`]; read-only verbs
//! (`status`, `list`, `show`) load the review alone and never touch git.
//! Human-facing output passes each line through the terminal sanitiser;
//! JSON output and the file on disk keep the raw text.

use std::io::Write;
use std::path::{Path, PathBuf};

use ambidiff_core::git_source::GitSource;
use ambidiff_core::protocol::{
    CommentAddRequest, CommentDeleteRequest, CommentEditRequest, LifecycleRequest,
};
use ambidiff_core::review::{Action, Actor, Comment, ReviewFile, Side, Source, Status};
use ambidiff_core::sanitize::sanitize_line;
use ambidiff_core::store::Store;
use ambidiff_core::sys::now_rfc3339;
use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::application::{Application, Outcome, OutcomeValue, ReviewCommand};
use crate::args::{
    AgentSetupArgs, CommentAddArgs, CommentAddressedArgs, CommentEditArgs, CommentIdArgs,
    CommentListArgs, InitArgs, RevBumpArgs, StatusArgs,
};
use crate::context::{default_review_name, resolve_store};

/// Exit code signaling actionable comments exist (status verb).
pub const EXIT_TODO: i32 = 10;

fn status_symbol(status: Status) -> &'static str {
    match status {
        Status::Open => "\u{25cb}",      // ○
        Status::Addressed => "\u{25d0}", // ◐
        Status::Resolved => "\u{25cf}",  // ●
        Status::Reopened => "\u{21ba}",  // ↺
    }
}

/// One sanitised line to stdout (human output only).
fn say(line: impl AsRef<str>) {
    println!("{}", sanitize_line(line.as_ref()));
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        for line in warning.lines() {
            eprintln!("ambidiff: warning: {}", sanitize_line(line));
        }
    }
}

fn comment_location(comment: &Comment) -> String {
    match (&comment.path, comment.line) {
        (None, _) => "(review)".to_string(),
        (Some(path), None) => path.clone(),
        (Some(path), Some(line)) => {
            let side = match comment.side {
                Some(Side::Old) => " (old)",
                _ => "",
            };
            match comment.end_line {
                Some(end) if end != line => format!("{path}:{line}-{end}{side}"),
                _ => format!("{path}:{line}{side}"),
            }
        }
    }
}

fn print_comment_human(comment: &Comment) {
    say(format!(
        "{} {}  rev {}  {}  {}",
        status_symbol(comment.status),
        comment.id,
        comment.rev,
        comment.status.as_str(),
        comment_location(comment),
    ));
    for line in comment.body.lines() {
        say(format!("    {line}"));
    }
    if let Some(response) = &comment.response {
        for (i, line) in response.lines().enumerate() {
            let prefix = if i == 0 { "\u{21b3} " } else { "  " };
            say(format!("    {prefix}{line}"));
        }
    }
}

fn print_outcome_comment(outcome: &Outcome, json: bool) -> Result<()> {
    print_warnings(&outcome.warnings);
    let OutcomeValue::Comment(comment) = &outcome.value else {
        bail!("unexpected outcome");
    };
    if json {
        println!("{}", serde_json::to_string(comment)?);
    } else {
        print_comment_human(comment);
    }
    Ok(())
}

pub fn init(args: InitArgs) -> Result<i32> {
    let root = std::env::current_dir().context("resolve current directory")?;
    let store = Store::new(&root);
    let name = args
        .review
        .clone()
        .unwrap_or_else(|| default_review_name(&root));

    let mut source = Source::git(args.base.clone());
    if args.staged {
        source
            .extra
            .insert("staged".to_string(), serde_json::Value::Bool(true));
    }

    let review = ReviewFile::new(name.clone(), source, &now_rfc3339());
    store.init(&review)?;

    // Review state is never git-tracked. A failure here is reported, not
    // hidden: the review exists, and the user can fix the exclusion before
    // committing.
    let mut warnings = Vec::new();
    if GitSource::is_repo(&root)
        && let Err(err) = GitSource::ensure_git_exclude(&root)
    {
        warnings.push(format!(
            "could not add the review sidecars to .git/info/exclude: {err}"
        ));
    }

    if args.json {
        println!(
            "{}",
            json!({
                "review": name,
                "root": root.display().to_string(),
                "file": store.review_path().display().to_string(),
                "warnings": warnings,
            })
        );
    } else {
        print_warnings(&warnings);
        say(format!(
            "initialized review {:?} at {}",
            name,
            store.review_path().display()
        ));
        say("next: open `ambidiff`, or run `ambidiff agent-setup` to brief agents");
    }
    Ok(0)
}

pub fn status(args: StatusArgs) -> Result<i32> {
    let app = Application::open(resolve_store()?);
    let outcome = app.load_review()?;
    let review = &outcome.review;
    let counts = review.counts();
    let todo = counts.todo();

    if args.json {
        println!(
            "{}",
            json!({
                "review": review.review,
                "revision": review.revision,
                "source": review.source,
                "counts": {
                    "open": counts.open,
                    "addressed": counts.addressed,
                    "resolved": counts.resolved,
                    "reopened": counts.reopened,
                },
                "todo": todo,
                "quarantined": review.quarantined.len(),
                "readOnly": outcome.read_only,
                "readOnlyReason": outcome.read_only_reason,
                "warnings": outcome.warnings,
            })
        );
    } else {
        print_warnings(&outcome.warnings);
        let base = review
            .source
            .base
            .as_deref()
            .map(|b| format!(" base {b}"))
            .unwrap_or_default();
        say(format!(
            "review {:?}  rev {}  {}{base}",
            review.review, review.revision, review.source.kind
        ));
        say(format!(
            "  {} {} open   {} {} reopened   {} {} addressed   {} {} resolved",
            status_symbol(Status::Open),
            counts.open,
            status_symbol(Status::Reopened),
            counts.reopened,
            status_symbol(Status::Addressed),
            counts.addressed,
            status_symbol(Status::Resolved),
            counts.resolved,
        ));
        if !review.quarantined.is_empty() {
            say(format!(
                "  ! {} quarantined record(s) need attention",
                review.quarantined.len()
            ));
        }
        if outcome.read_only {
            say(format!(
                "  ! read-only: {}",
                outcome
                    .read_only_reason
                    .as_deref()
                    .unwrap_or("see warnings")
            ));
        }
        say(format!("  agent to-do: {todo}"));
    }
    Ok(if todo > 0 { EXIT_TODO } else { 0 })
}

pub fn comment_add(args: CommentAddArgs) -> Result<i32> {
    let side = match args.side.as_deref() {
        Some("old") => Some(Side::Old),
        Some(_) => Some(Side::New),
        None => args.line.map(|_| Side::New),
    };
    let request = CommentAddRequest {
        path: args.path.clone(),
        side,
        line: args.line,
        end_line: args.end_line,
        body: args.body.clone(),
        author: args.author.clone(),
    };
    let mut app = Application::open(resolve_store()?);
    let outcome = app.execute(ReviewCommand::Add(request))?;
    print_outcome_comment(&outcome, args.json)?;
    Ok(0)
}

fn parse_status_filter(filter: &str) -> Result<Vec<Status>> {
    filter
        .split(',')
        .map(|s| match s.trim() {
            "open" => Ok(Status::Open),
            "addressed" => Ok(Status::Addressed),
            "resolved" => Ok(Status::Resolved),
            "reopened" => Ok(Status::Reopened),
            other => bail!("unknown status {other:?} (open, addressed, resolved, reopened)"),
        })
        .collect()
}

pub fn comment_list(args: CommentListArgs) -> Result<i32> {
    let app = Application::open(resolve_store()?);
    let outcome = app.load_review()?;
    let statuses = match (&args.status, args.todo) {
        (Some(_), true) => bail!("--status and --todo are mutually exclusive"),
        (Some(filter), false) => Some(parse_status_filter(filter)?),
        (None, true) => Some(vec![Status::Open, Status::Reopened]),
        (None, false) => None,
    };

    let comments: Vec<&Comment> = outcome
        .review
        .comments
        .iter()
        .filter(|c| {
            statuses
                .as_ref()
                .is_none_or(|wanted| wanted.contains(&c.status))
        })
        .filter(|c| {
            args.path
                .as_deref()
                .is_none_or(|path| c.path.as_deref() == Some(path))
        })
        .collect();

    if args.json {
        println!("{}", json!({ "comments": comments }));
    } else {
        print_warnings(&outcome.warnings);
        if comments.is_empty() {
            say("no comments match");
        }
        for comment in &comments {
            print_comment_human(comment);
        }
    }
    Ok(0)
}

pub fn comment_show(args: CommentIdArgs) -> Result<i32> {
    let app = Application::open(resolve_store()?);
    let outcome = app.load_review()?;
    let comment = outcome
        .review
        .find_comment(&args.id)
        .ok_or_else(|| anyhow::anyhow!("no comment with id {}", args.id))?;
    if args.json {
        println!("{}", serde_json::to_string(comment)?);
    } else {
        print_warnings(&outcome.warnings);
        print_comment_human(comment);
        if let Some(snippet) = &comment.snippet {
            say(format!("    snippet: {snippet}"));
        }
    }
    Ok(0)
}

fn lifecycle_verb(
    id: &str,
    action: Action,
    actor: Actor,
    response: Option<String>,
    json: bool,
) -> Result<i32> {
    let mut app = Application::open(resolve_store()?);
    let req = LifecycleRequest {
        id: id.to_string(),
        // An empty response is the same as none: nothing is stored.
        response: response.filter(|r| !r.trim().is_empty()),
    };
    let outcome = app.execute(ReviewCommand::Lifecycle { action, actor, req })?;
    print_outcome_comment(&outcome, json)?;
    Ok(0)
}

pub fn comment_addressed(args: CommentAddressedArgs) -> Result<i32> {
    lifecycle_verb(
        &args.id,
        Action::Address,
        Actor::Agent,
        args.response,
        args.json,
    )
}

pub fn comment_resolve(args: CommentIdArgs) -> Result<i32> {
    // Human-only by contract: this verb is documented as such and the
    // agent-setup block forbids agents from running it.
    lifecycle_verb(&args.id, Action::Resolve, Actor::Human, None, args.json)
}

pub fn comment_reopen(args: CommentIdArgs) -> Result<i32> {
    lifecycle_verb(&args.id, Action::Reopen, Actor::Human, None, args.json)
}

pub fn comment_edit(args: CommentEditArgs) -> Result<i32> {
    let mut app = Application::open(resolve_store()?);
    let outcome = app.execute(ReviewCommand::Edit(CommentEditRequest {
        id: args.id.clone(),
        body: args.body.clone(),
    }))?;
    print_outcome_comment(&outcome, args.json)?;
    Ok(0)
}

pub fn comment_delete(args: CommentIdArgs) -> Result<i32> {
    let mut app = Application::open(resolve_store()?);
    let outcome = app.execute(ReviewCommand::Delete(CommentDeleteRequest {
        id: args.id.clone(),
    }))?;
    print_warnings(&outcome.warnings);
    let OutcomeValue::Deleted(removed) = outcome.value else {
        bail!("unexpected outcome");
    };
    if args.json {
        println!("{}", json!({ "deleted": removed }));
    } else {
        say(format!("deleted {removed}"));
    }
    Ok(0)
}

pub fn rev_bump(args: RevBumpArgs) -> Result<i32> {
    let mut app = Application::open(resolve_store()?);
    let outcome = app.execute(ReviewCommand::RevBump)?;
    print_warnings(&outcome.warnings);
    let OutcomeValue::Revision(revision) = outcome.value else {
        bail!("unexpected outcome");
    };
    if args.json {
        println!("{}", json!({ "revision": revision }));
    } else {
        say(format!("review pass is now rev {revision}"));
    }
    Ok(0)
}

// ------------------------------------------------------------ agent setup

/// Version marker inside the BEGIN line; bump when the block content
/// changes so `--check` can detect stale installs.
const AGENT_BLOCK_VERSION: u32 = 1;

fn agent_block() -> String {
    format!(
        "<!-- BEGIN ambidiff (v{AGENT_BLOCK_VERSION}) -->\n\
## Code review loop (ambidiff)\n\
\n\
This tree is under review: the reviewer leaves comments in `.ambidiff.json`\n\
at the review root, you address them, and the reviewer re-reviews. Work the\n\
loop with the `ambidiff` CLI rather than editing that file directly.\n\
\n\
- `ambidiff status --json`: overview; exit code 10 means comments await you.\n\
- `ambidiff comment list --todo --json`: your to-do list (open + reopened).\n\
- For each to-do: make the change it asks for, then run\n\
  `ambidiff comment addressed <id> -m \"one line on what you did\"`.\n\
- A body containing `??` is a question. Answer it in the response instead of\n\
  changing code, unless the answer itself implies a fix.\n\
- NEVER run `ambidiff comment resolve` or `ambidiff comment reopen`; those\n\
  verdicts belong to the human reviewer alone.\n\
- Never edit or delete the reviewer's comments, and never change `revision`.\n\
- You may add findings of your own: `ambidiff comment add -p <file> -l <line>\n\
  -m \"...\" --author agent`.\n\
- A line comment's `snippet` is the code as it looked when the comment was\n\
  written; if lines have moved, find the code by content, not line number.\n\
\n\
When nothing is left in the to-do list, stop and report back; the human\n\
re-reviews from there.\n\
<!-- END ambidiff -->"
    )
}

/// The Claude Code skill, embedded so `agent-setup --claude-skill` can
/// install it without a checkout.
const CLAUDE_SKILL: &str = include_str!("../../../skills/ambidiff-review/SKILL.md");

const BLOCK_BEGIN_PREFIX: &str = "<!-- BEGIN ambidiff";
const BLOCK_END: &str = "<!-- END ambidiff -->";

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum BlockError {
    #[error("a `{BLOCK_BEGIN_PREFIX}` marker has no matching `{BLOCK_END}` marker")]
    MissingEnd,
    #[error("more than one `{BLOCK_BEGIN_PREFIX}` marker")]
    DuplicateBegin,
    #[error("a `{BLOCK_END}` marker precedes the `{BLOCK_BEGIN_PREFIX}` marker")]
    EndBeforeBegin,
}

/// Install or refresh the managed block in one file's content. Returns the
/// new content and whether anything changed; malformed delimiters are an
/// error so a half-block is never overwritten by guesswork.
fn upsert_block(content: &str, block: &str) -> Result<(String, bool), BlockError> {
    let begins: Vec<usize> = content
        .match_indices(BLOCK_BEGIN_PREFIX)
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<usize> = content.match_indices(BLOCK_END).map(|(i, _)| i).collect();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => {}
        ([start], [end_at]) => {
            if end_at < start {
                return Err(BlockError::EndBeforeBegin);
            }
            let end = end_at + BLOCK_END.len();
            let existing = &content[*start..end];
            if existing == block {
                return Ok((content.to_string(), false));
            }
            let mut next = String::with_capacity(content.len());
            next.push_str(&content[..*start]);
            next.push_str(block);
            next.push_str(&content[end..]);
            return Ok((next, true));
        }
        ([_, _, ..], _) => return Err(BlockError::DuplicateBegin),
        ([_], _) => return Err(BlockError::MissingEnd),
        ([], [_, ..]) => return Err(BlockError::EndBeforeBegin),
    }
    let mut next = content.to_string();
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(block);
    next.push('\n');
    Ok((next, true))
}

/// A setup target after preflight: what it holds now and what it should
/// hold. Every target is read and checked before any write.
struct SetupTarget {
    path: PathBuf,
    state: &'static str,
    /// New content when a write is needed.
    next: Option<String>,
}

/// Read a setup target as UTF-8 text; absence is an empty file, a symlink
/// or unreadable bytes are errors naming the path.
fn read_target(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "{} is a symlink; refusing to write through it",
                path.display()
            )
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is not valid UTF-8; refusing to rewrite it",
            path.display()
        )
    })?;
    Ok(Some(text))
}

fn preflight_block_target(path: PathBuf, block: &str) -> Result<SetupTarget> {
    let existing = read_target(&path)?;
    let (next, changed) = upsert_block(existing.as_deref().unwrap_or_default(), block)
        .with_context(|| format!("{}: malformed managed block", path.display()))?;
    let state = if !changed {
        "current"
    } else if existing.is_some_and(|e| e.contains(BLOCK_BEGIN_PREFIX)) {
        "updated"
    } else {
        "installed"
    };
    Ok(SetupTarget {
        path,
        state,
        next: changed.then_some(next),
    })
}

fn preflight_skill_target(path: PathBuf) -> Result<SetupTarget> {
    let existing = read_target(&path)?;
    let state = match existing.as_deref() {
        Some(e) if e == CLAUDE_SKILL => "current",
        Some(_) => "updated",
        None => "installed",
    };
    Ok(SetupTarget {
        path,
        state,
        next: (state != "current").then(|| CLAUDE_SKILL.to_string()),
    })
}

/// Atomic replace: exclusive temp file in the target's directory, the
/// existing mode bits copied, then rename. A failure leaves the target as
/// it was.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("temp file in {}", parent.display()))?;
    temp.write_all(content.as_bytes())
        .and_then(|_| temp.as_file().sync_all())
        .with_context(|| format!("write {}", path.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(temp.path(), meta.permissions());
    }
    temp.persist(path)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("replace {}: {}", path.display(), e.error))
}

pub fn agent_setup(args: AgentSetupArgs) -> Result<i32> {
    let store = resolve_store()?;
    let root = store.root();
    let block = agent_block();

    // AGENTS.md is created if absent; CLAUDE.md only updated when present
    // (context files are baked at session load, so discovery needs a
    // standing instruction in whichever file the agent actually reads).
    // Every target is preflighted before the first write so a refused
    // target leaves the others untouched too.
    let mut targets = vec![preflight_block_target(root.join("AGENTS.md"), &block)?];
    let claude = root.join("CLAUDE.md");
    if std::fs::symlink_metadata(&claude).is_ok() {
        targets.push(preflight_block_target(claude, &block)?);
    }
    if args.claude_skill {
        targets.push(preflight_skill_target(
            root.join(".claude/skills/ambidiff-review/SKILL.md"),
        )?);
    }

    let needs_setup = targets.iter().any(|t| t.next.is_some());
    if !args.check {
        for target in &targets {
            if let Some(next) = &target.next {
                write_atomic(&target.path, next)?;
            }
        }
    }

    if args.json {
        let files: Vec<_> = targets
            .iter()
            .map(|t| json!({ "path": t.path.display().to_string(), "state": t.state }))
            .collect();
        println!("{}", json!({ "check": args.check, "files": files }));
    } else {
        for target in &targets {
            say(format!("{}: {}", target.state, target.path.display()));
        }
    }
    if args.check && needs_setup {
        return Ok(1);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_block_appends_replaces_and_idempotent() {
        let block = agent_block();
        let (v1, changed) = upsert_block("# Project\n", &block).expect("ok");
        assert!(changed);
        assert!(v1.contains("## Code review loop"));

        let (v2, changed) = upsert_block(&v1, &block).expect("ok");
        assert!(!changed, "second run is a no-op");
        assert_eq!(v1, v2);

        // A stale block (different version marker) gets replaced in place.
        let stale = v1.replace("(v1)", "(v0)");
        let (v3, changed) = upsert_block(&stale, &block).expect("ok");
        assert!(changed);
        assert_eq!(v3.matches(BLOCK_BEGIN_PREFIX).count(), 1);
        assert!(v3.contains("(v1)"));
    }

    #[test]
    fn upsert_preserves_surrounding_content() {
        let block = agent_block();
        let content = format!("before\n\n{block}\n\nafter\n");
        let (next, changed) = upsert_block(&content, &block).expect("ok");
        assert!(!changed);
        assert!(next.starts_with("before"));
        assert!(next.ends_with("after\n"));
    }

    #[test]
    fn malformed_delimiters_are_errors_not_guesses() {
        let block = agent_block();
        assert_eq!(
            upsert_block("x\n<!-- BEGIN ambidiff (v0) -->\nhalf\n", &block),
            Err(BlockError::MissingEnd)
        );
        assert_eq!(
            upsert_block(
                "<!-- END ambidiff -->\n<!-- BEGIN ambidiff (v0) -->\n",
                &block
            ),
            Err(BlockError::EndBeforeBegin)
        );
        assert_eq!(
            upsert_block("<!-- END ambidiff -->\n", &block),
            Err(BlockError::EndBeforeBegin)
        );
        let twice = format!("{block}\n{block}\n");
        assert_eq!(
            upsert_block(&twice, &block),
            Err(BlockError::DuplicateBegin)
        );
    }

    #[test]
    fn human_lines_are_sanitised_before_the_terminal() {
        let comment = Comment {
            id: "c-1".into(),
            rev: 1,
            status: Status::Open,
            path: Some("a\x1b[2Jb".into()),
            side: None,
            line: None,
            end_line: None,
            snippet: None,
            body: "x".into(),
            response: None,
            author: "a".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
            extra: serde_json::Map::new(),
        };
        assert_eq!(sanitize_line(&comment_location(&comment)), "ab");
    }
}
