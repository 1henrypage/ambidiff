//! The review file: schema, salvage-mode parsing, strict input validation,
//! and revision bookkeeping.
//!
//! `.ambidiff.json` is the source of truth AND the sync bus: every engine
//! watches it and agents edit it directly, so reads must never brick.
//! Parsing quarantines bad comment records (preserved under `quarantined`,
//! never silently dropped), fills salvageable top-level fields with warnings,
//! and marks files written by a newer schema major as read-only. Writes are
//! strict: CLI/UI-created comments are validated before they enter the file.
//!
//! This module is pure (string in, string out) so it runs in wasm; the
//! native lock/atomic-write layer lives in `store`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::lifecycle::{self, LifecycleError};
pub use crate::lifecycle::{Action, Actor, Status};

/// Schema major this build writes.
pub const SCHEMA_MAJOR: u64 = 1;

/// File name of the review file at the review root.
pub const REVIEW_FILE_NAME: &str = ".ambidiff.json";

/// File name of the create-exclusive lock sidecar (the ownership record).
pub const LOCK_FILE_NAME: &str = ".ambidiff.json.lock";

/// File name of the persistent, never-unlinked lock guard. Peers (and every
/// pre-repair binary) unlink and recreate [`LOCK_FILE_NAME`], so a second
/// process taking an OS advisory lock on that path locks a different inode
/// than the first. The guard file is created once and never removed, so its
/// inode is stable: creation, stale recovery, and release of the sidecar all
/// happen while holding an OS lock on the guard.
pub const GUARD_FILE_NAME: &str = ".ambidiff.json.guard";

/// Prefix of a same-directory temp file used for atomic review-file writes.
pub const TMP_FILE_PREFIX: &str = ".ambidiff.json.tmp.";

/// True when `name` is one of ambidiff's own review-directory sidecars
/// (lock, guard, or a write's temp file) rather than the review file itself
/// or unrelated tree content. Used by the watcher to ignore its own churn.
pub fn is_review_sidecar(name: &str) -> bool {
    name == LOCK_FILE_NAME || name == GUARD_FILE_NAME || name.starts_with(TMP_FILE_PREFIX)
}

/// Which side of the diff a line comment anchors to. Removed lines anchor
/// old-side numbers; added and context lines anchor new-side (revdiff's
/// proven rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

/// The diff source recorded in the review file. Git is the only v1 kind;
/// unknown kinds survive round-trips untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Source {
    pub fn git(base: Option<String>) -> Self {
        Source {
            kind: "git".to_string(),
            base,
            extra: Map::new(),
        }
    }
}

/// One review comment. Anchor levels via nulls: `path: null` is
/// review-level, `line: null` is file-level, otherwise a line or range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    /// Review pass this comment was raised in (provenance; never mutated by
    /// reopen).
    pub rev: u32,
    pub status: Status,
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub side: Option<Side>,
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_line: Option<u32>,
    /// Source text captured at comment time; drift detection compares it
    /// whitespace-insensitively against the current line.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub snippet: Option<String>,
    pub body: String,
    /// Agent's one-line response recorded when addressing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response: Option<String>,
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Comment {
    /// True when the comment body asks a question rather than giving a
    /// directive (the `??` convention).
    pub fn is_question(&self) -> bool {
        self.body.contains("??")
    }
}

/// Tallies per status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StatusCounts {
    pub open: usize,
    pub addressed: usize,
    pub resolved: usize,
    pub reopened: usize,
}

impl StatusCounts {
    pub fn todo(&self) -> usize {
        self.open + self.reopened
    }
    pub fn total(&self) -> usize {
        self.open + self.addressed + self.resolved + self.reopened
    }
}

/// The parsed review file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFile {
    pub ambidiff: u64,
    pub review: String,
    pub revision: u32,
    pub source: Source,
    pub created_at: String,
    pub updated_at: String,
    pub comments: Vec<Comment>,
    /// Records that failed validation on load, preserved verbatim so no
    /// agent- or human-authored data is ever silently destroyed.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub quarantined: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ReviewFile {
    pub fn new(review: String, source: Source, now: &str) -> Self {
        ReviewFile {
            ambidiff: SCHEMA_MAJOR,
            review,
            revision: 1,
            source,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            comments: Vec::new(),
            quarantined: Vec::new(),
            extra: Map::new(),
        }
    }

    pub fn counts(&self) -> StatusCounts {
        let mut c = StatusCounts::default();
        for comment in &self.comments {
            match comment.status {
                Status::Open => c.open += 1,
                Status::Addressed => c.addressed += 1,
                Status::Resolved => c.resolved += 1,
                Status::Reopened => c.reopened += 1,
            }
        }
        c
    }

    pub fn find_comment(&self, id: &str) -> Option<&Comment> {
        self.comments.iter().find(|c| c.id == id)
    }

    /// A new pass opens when review activity lands while nothing is
    /// outstanding: the previous pass's comments are all addressed or
    /// resolved (the agent finished), so the next human comment or reopen
    /// starts pass N+1.
    fn pass_is_complete(&self) -> bool {
        !self.comments.is_empty() && self.comments.iter().all(|c| !c.status.is_todo())
    }

    /// The revision the next pass-opening event would use, checked against
    /// overflow. `u32::MAX` cannot be incremented further.
    pub fn next_revision(&self) -> Result<u32, ReviewError> {
        self.revision
            .checked_add(1)
            .ok_or(ReviewError::RevisionExhausted)
    }

    /// Add a validated, uniquely-identified comment, bumping the revision
    /// first when it opens a new pass. Validates, checks for a duplicate id,
    /// and checks revision overflow before mutating anything; a failure
    /// leaves the review file completely unchanged. Returns a reference to
    /// the stored comment.
    pub fn try_add_comment(
        &mut self,
        new: NewComment,
        id: String,
        now: &str,
    ) -> Result<&Comment, ReviewError> {
        validate_new_comment(&new)?;
        if self.comments.iter().any(|c| c.id == id) {
            return Err(ReviewError::DuplicateId { id });
        }
        let rev = if self.pass_is_complete() {
            self.next_revision()?
        } else {
            self.revision
        };
        // No mutation above this line: everything that can fail has failed
        // already, so from here the operation cannot be aborted partway.
        self.revision = rev;
        let comment = Comment {
            id,
            rev,
            status: Status::Open,
            path: new.path,
            side: new.side,
            line: new.line,
            end_line: new.end_line,
            snippet: new.snippet,
            body: new.body,
            response: None,
            author: new.author,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            extra: Map::new(),
        };
        self.comments.push(comment);
        self.updated_at = now.to_string();
        Ok(self.comments.last().expect("just pushed"))
    }

    /// Edit a comment's body in place. Rejects a blank body or an unknown id
    /// without mutating anything.
    pub fn edit_comment(
        &mut self,
        id: &str,
        body: &str,
        now: &str,
    ) -> Result<&Comment, ReviewError> {
        if body.trim().is_empty() {
            return Err(ReviewError::Validation {
                msg: "body must not be empty".to_string(),
            });
        }
        let idx = self
            .comments
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| ReviewError::CommentNotFound { id: id.to_string() })?;
        self.comments[idx].body = body.to_string();
        self.comments[idx].updated_at = now.to_string();
        self.updated_at = now.to_string();
        Ok(&self.comments[idx])
    }

    /// Remove a comment by id, returning the removed record so a caller can
    /// offer to restore it. Unknown ids fail without mutating anything.
    pub fn delete_comment(&mut self, id: &str, now: &str) -> Result<Comment, ReviewError> {
        let idx = self
            .comments
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| ReviewError::CommentNotFound { id: id.to_string() })?;
        let removed = self.comments.remove(idx);
        self.updated_at = now.to_string();
        Ok(removed)
    }

    /// Apply a lifecycle action to a comment by id. A human reopen that lands
    /// while the pass is complete opens a new pass, same as a new comment.
    /// The lifecycle transition and revision overflow are both checked
    /// before any field is mutated.
    pub fn apply_lifecycle(
        &mut self,
        id: &str,
        action: Action,
        actor: Actor,
        response: Option<String>,
        now: &str,
    ) -> Result<&Comment, ReviewError> {
        let idx = self
            .comments
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| ReviewError::CommentNotFound { id: id.to_string() })?;
        let next = lifecycle::transition(self.comments[idx].status, action, actor)?;
        let bumped = if action == Action::Reopen && self.pass_is_complete() {
            Some(self.next_revision()?)
        } else {
            None
        };
        if let Some(rev) = bumped {
            self.revision = rev;
        }
        let comment = &mut self.comments[idx];
        comment.status = next;
        if let Some(response) = response
            && !response.is_empty()
        {
            comment.response = Some(response);
        }
        comment.updated_at = now.to_string();
        self.updated_at = now.to_string();
        Ok(&self.comments[idx])
    }

    /// Manual revision bump: the escape hatch when pass detection does not
    /// match reality. Fails without mutating anything at `u32::MAX`.
    pub fn try_rev_bump(&mut self, now: &str) -> Result<u32, ReviewError> {
        let rev = self.next_revision()?;
        self.revision = rev;
        self.updated_at = now.to_string();
        Ok(self.revision)
    }

    /// Structural invariants a review file must hold before it is
    /// serialised: every comment id is well-formed and unique, and the
    /// revision counter is in range. Called by `Store::mutate` after `f`
    /// runs and before the write, so a mutation that violates an invariant
    /// is rejected rather than persisted.
    pub fn validate(&self) -> Result<(), ReviewError> {
        if self.revision == 0 {
            return Err(ReviewError::Validation {
                msg: "revision must not be 0".to_string(),
            });
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for comment in &self.comments {
            if !valid_id(&comment.id) {
                return Err(ReviewError::Validation {
                    msg: format!("invalid comment id {:?}", comment.id),
                });
            }
            if !seen.insert(comment.id.as_str()) {
                return Err(ReviewError::DuplicateId {
                    id: comment.id.clone(),
                });
            }
            if comment.path.is_none() && (comment.line.is_some() || comment.end_line.is_some()) {
                return Err(ReviewError::Validation {
                    msg: format!("comment {}: line without path", comment.id),
                });
            }
            if let (Some(line), Some(end)) = (comment.line, comment.end_line)
                && end < line
            {
                return Err(ReviewError::Validation {
                    msg: format!("comment {}: endLine < line", comment.id),
                });
            }
        }
        Ok(())
    }
}

/// Input for a new comment, validated by [`validate_new_comment`] before it
/// reaches [`ReviewFile::try_add_comment`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewComment {
    pub path: Option<String>,
    pub side: Option<Side>,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub snippet: Option<String>,
    pub body: String,
    pub author: String,
}

/// Errors from review file operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReviewError {
    #[error("review file is not valid JSON: {msg}")]
    Json { msg: String },
    #[error("review file root is not a JSON object")]
    NotAnObject,
    #[error(
        "review file was written by a newer ambidiff (schema {found}, this build understands {SCHEMA_MAJOR}); refusing to modify it"
    )]
    SchemaTooNew { found: u64 },
    #[error("no comment with id {id}")]
    CommentNotFound { id: String },
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("invalid comment: {msg}")]
    Validation { msg: String },
    #[error("revision counter exhausted (already at u32::MAX)")]
    RevisionExhausted,
    #[error("duplicate comment id {id}")]
    DuplicateId { id: String },
    #[error("could not serialize review file: {msg}")]
    Serialize { msg: String },
}

/// Strict validation for comments entering through the CLI/UI boundary.
pub fn validate_new_comment(new: &NewComment) -> Result<(), ReviewError> {
    let fail = |msg: &str| {
        Err(ReviewError::Validation {
            msg: msg.to_string(),
        })
    };
    if new.body.trim().is_empty() {
        return fail("body must not be empty");
    }
    if new.path.is_none() {
        if new.line.is_some() || new.end_line.is_some() {
            return fail("a line requires a file path");
        }
        if new.side.is_some() {
            return fail("a side requires a file path and line");
        }
    }
    if new.line.is_none() {
        if new.end_line.is_some() {
            return fail("endLine requires line");
        }
        if new.side.is_some() {
            return fail("a side requires a line");
        }
    }
    if let Some(line) = new.line {
        if line == 0 {
            return fail("line numbers are 1-based");
        }
        if let Some(end) = new.end_line
            && end < line
        {
            return fail("endLine must be >= line");
        }
    }
    if let Some(path) = &new.path
        && path.is_empty()
    {
        return fail("path must not be empty");
    }
    Ok(())
}

/// Result of a salvage-mode parse.
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub review: ReviewFile,
    /// Human-readable warnings for anything salvaged or quarantined.
    pub warnings: Vec<String>,
    /// True when the file must not be modified: a newer schema major, or a
    /// present top-level value that cannot be represented without dropping
    /// or changing its content.
    pub read_only: bool,
    /// The first warning that caused `read_only`, if any. Surfaced to
    /// callers (e.g. `Store::mutate`'s `ReadOnly { reason }`) so a human can
    /// see why writes are refused without re-deriving it from `warnings`.
    pub read_only_reason: Option<String>,
    /// Present-but-unrepresentable top-level values, verbatim, keyed by
    /// their JSON field name. Never written back (the file is read-only
    /// whenever this is non-empty); kept only for diagnostics/transport so a
    /// read-only load is never silently rewritten as a normalised, lossy
    /// document.
    pub retained: BTreeMap<String, Value>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Semantic validation for a structurally parsed comment record. Returns a
/// rejection reason, or None when the record is acceptable. `warnings`
/// collects salvageable oddities that do not quarantine.
fn check_comment(comment: &mut Comment, warnings: &mut Vec<String>) -> Option<String> {
    if !valid_id(&comment.id) {
        return Some(format!("invalid id {:?}", comment.id));
    }
    if comment.rev == 0 {
        warnings.push(format!("comment {}: rev 0 coerced to 1", comment.id));
        comment.rev = 1;
    }
    if comment.path.is_none() && (comment.line.is_some() || comment.end_line.is_some()) {
        return Some(format!("comment {}: line without path", comment.id));
    }
    if comment.line.is_none() && comment.end_line.is_some() {
        return Some(format!("comment {}: endLine without line", comment.id));
    }
    if let (Some(line), Some(end)) = (comment.line, comment.end_line)
        && end < line
    {
        return Some(format!("comment {}: endLine < line", comment.id));
    }
    if comment.line.is_some() && comment.side.is_none() {
        warnings.push(format!(
            "comment {}: missing side defaulted to new",
            comment.id
        ));
        comment.side = Some(Side::New);
    }
    if comment.body.is_empty() {
        warnings.push(format!("comment {}: empty body", comment.id));
    }
    None
}

/// Accumulates warnings, read-only status, and retained originals while
/// [`parse_review`] walks the top-level fields.
#[derive(Default)]
struct Salvage {
    warnings: Vec<String>,
    read_only: bool,
    read_only_reason: Option<String>,
    retained: BTreeMap<String, Value>,
}

impl Salvage {
    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    fn mark_read_only(&mut self, reason: String) {
        self.warnings.push(reason.clone());
        self.read_only = true;
        self.read_only_reason.get_or_insert(reason);
    }

    /// Retain a present-but-unrepresentable top-level value verbatim under
    /// `field`, warn, and mark the document read-only.
    fn retain(&mut self, field: &str, value: Value, reason: String) {
        self.warnings.push(reason.clone());
        self.retained.insert(field.to_string(), value);
        self.read_only = true;
        self.read_only_reason.get_or_insert(reason);
    }
}

/// Parse review file content in salvage mode.
///
/// Hard errors only for content that cannot be represented at all (invalid
/// JSON, non-object root). Everything else degrades: bad comment records are
/// quarantined with warnings, missing top-level fields are filled with
/// defaults and warned about, and a newer schema major marks the outcome
/// read-only.
pub fn parse_review(content: &str) -> Result<LoadOutcome, ReviewError> {
    let value: Value =
        serde_json::from_str(content).map_err(|e| ReviewError::Json { msg: e.to_string() })?;
    let Value::Object(mut root) = value else {
        return Err(ReviewError::NotAnObject);
    };

    let mut salvage = Salvage::default();

    let ambidiff = match root.remove("ambidiff") {
        Some(Value::Number(n)) if n.as_u64().is_some() => n.as_u64().unwrap_or(SCHEMA_MAJOR),
        Some(other) => {
            let reason = format!("ambidiff field {other} is not a non-negative integer");
            salvage.retain("ambidiff", other, reason);
            SCHEMA_MAJOR
        }
        None => {
            salvage.warn("missing ambidiff schema field; assuming 1");
            SCHEMA_MAJOR
        }
    };
    if ambidiff > SCHEMA_MAJOR {
        salvage.mark_read_only(format!(
            "file written by newer ambidiff (schema {ambidiff}); opening read-only"
        ));
    }

    let review = match root.remove("review") {
        Some(Value::String(s)) => s,
        Some(other) => {
            let reason = format!("review name {other} is not a string");
            salvage.retain("review", other, reason);
            String::new()
        }
        None => {
            salvage.warn("missing review name");
            String::new()
        }
    };

    let revision = match root.remove("revision") {
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v @ 1..) if v <= u64::from(u32::MAX) => v as u32,
            _ => {
                let reason = format!("revision {n} is out of range (must be 1..=u32::MAX)");
                salvage.retain("revision", Value::Number(n), reason);
                1
            }
        },
        Some(other) => {
            let reason = format!("revision {other} is not a number");
            salvage.retain("revision", other, reason);
            1
        }
        None => {
            salvage.warn("missing revision; using 1");
            1
        }
    };

    let source = match root.remove("source") {
        Some(v @ Value::Object(_)) => match serde_json::from_value::<Source>(v.clone()) {
            Ok(s) => s,
            Err(e) => {
                let reason = format!("unreadable source: {e}");
                salvage.retain("source", v, reason);
                Source::git(None)
            }
        },
        Some(other) => {
            let reason = format!("source {other} is not an object");
            salvage.retain("source", other, reason);
            Source::git(None)
        }
        None => {
            salvage.warn("missing source; assuming git");
            Source::git(None)
        }
    };

    let string_field =
        |root: &mut Map<String, Value>, salvage: &mut Salvage, key: &str| match root.remove(key) {
            Some(Value::String(s)) => s,
            Some(other) => {
                let reason = format!("{key} {other} is not a string");
                salvage.retain(key, other, reason);
                String::new()
            }
            None => {
                salvage.warn(format!("missing {key}; using empty"));
                String::new()
            }
        };
    let created_at = string_field(&mut root, &mut salvage, "createdAt");
    let updated_at = string_field(&mut root, &mut salvage, "updatedAt");

    let mut comments: Vec<Comment> = Vec::new();
    let mut quarantined: Vec<Value> = Vec::new();
    match root.remove("comments") {
        Some(Value::Array(items)) => {
            for (i, item) in items.into_iter().enumerate() {
                match serde_json::from_value::<Comment>(item.clone()) {
                    Ok(mut comment) => {
                        if let Some(reason) = check_comment(&mut comment, &mut salvage.warnings) {
                            salvage.warn(format!("comment[{i}] quarantined: {reason}"));
                            quarantined.push(item);
                        } else if comments.iter().any(|c| c.id == comment.id) {
                            salvage.warn(format!(
                                "comment[{i}] quarantined: duplicate id {}",
                                comment.id
                            ));
                            quarantined.push(item);
                        } else {
                            comments.push(comment);
                        }
                    }
                    Err(e) => {
                        salvage.warn(format!("comment[{i}] quarantined: {e}"));
                        quarantined.push(item);
                    }
                }
            }
        }
        Some(other) => {
            // The comments field exists but is unusable; without a readable
            // array any write would destroy whatever it held, so retain the
            // original verbatim and lock writes.
            let reason = format!("comments is not an array ({})", type_name(&other));
            salvage.retain("comments", other, reason);
        }
        None => {
            salvage.warn("missing comments array");
        }
    }

    // Pre-existing quarantine from an earlier salvage round-trips; an
    // unreadable container (present but not an array) is retained verbatim
    // rather than silently dropped.
    match root.remove("quarantined") {
        Some(Value::Array(items)) => quarantined.extend(items),
        Some(other) => {
            let reason = format!("quarantined is not an array ({})", type_name(&other));
            salvage.retain("quarantined", other, reason);
        }
        None => {}
    }

    let Salvage {
        warnings,
        read_only,
        read_only_reason,
        retained,
    } = salvage;

    let review = ReviewFile {
        ambidiff,
        review,
        revision,
        source,
        created_at,
        updated_at,
        comments,
        quarantined,
        extra: root,
    };
    Ok(LoadOutcome {
        review,
        warnings,
        read_only,
        read_only_reason,
        retained,
    })
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Serialize a review file to its canonical on-disk form: pretty-printed,
/// two-space indent, trailing newline (agent- and diff-friendly). A
/// serialization failure is surfaced as an error rather than silently
/// substituting `"{}"`, which would discard the entire file.
pub fn to_json(review: &ReviewFile) -> Result<String, ReviewError> {
    let mut out = serde_json::to_string_pretty(review)
        .map_err(|e| ReviewError::Serialize { msg: e.to_string() })?;
    out.push('\n');
    Ok(out)
}

/// Whitespace-insensitive snippet comparison: the "outdated" test. A
/// captured snippet still matches when only indentation or spacing changed.
pub fn snippet_matches(snippet: &str, current_line: &str) -> bool {
    let strip = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    strip(snippet) == strip(current_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_file() -> ReviewFile {
        ReviewFile::new(
            "test-review".to_string(),
            Source::git(Some("main".to_string())),
            "2026-01-01T00:00:00Z",
        )
    }

    fn new_comment(body: &str) -> NewComment {
        NewComment {
            path: Some("src/a.rs".to_string()),
            side: Some(Side::New),
            line: Some(3),
            end_line: None,
            snippet: Some("let x = 1;".to_string()),
            body: body.to_string(),
            author: "henry".to_string(),
        }
    }

    #[test]
    fn round_trip_is_identity() {
        let mut review = base_file();
        review
            .try_add_comment(
                new_comment("first?"),
                "c-0001".into(),
                "2026-01-01T00:01:00Z",
            )
            .expect("add comment");
        let json = to_json(&review).expect("serialize");
        let outcome = parse_review(&json).expect("parse");
        assert_eq!(outcome.review, review);
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert!(!outcome.read_only);
    }

    #[test]
    fn plan_schema_example_parses_cleanly() {
        let json = r#"{
          "ambidiff": 1,
          "review": "worktree-auth-fix",
          "revision": 2,
          "source": { "kind": "git", "base": "main" },
          "createdAt": "2026-01-01T00:00:00Z",
          "updatedAt": "2026-01-02T00:00:00Z",
          "comments": [{
            "id": "c-7f3a", "rev": 1, "status": "reopened",
            "path": "src/login.ts", "side": "new", "line": 42, "endLine": 45,
            "snippet": "if (user == null) return;",
            "body": "null check inverted?? explain",
            "response": "fixed: now throws AuthError",
            "author": "henry",
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:05:00Z"
          }]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        let c = &outcome.review.comments[0];
        assert_eq!(c.id, "c-7f3a");
        assert_eq!(c.status, Status::Reopened);
        assert_eq!(c.side, Some(Side::New));
        assert_eq!(c.end_line, Some(45));
        assert!(c.is_question());
        assert_eq!(outcome.review.counts().todo(), 1);
    }

    #[test]
    fn unknown_fields_survive_round_trip() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1,
          "source": {"kind": "git", "base": "main", "customSourceKey": 1},
          "createdAt": "t", "updatedAt": "t",
          "comments": [{
            "id": "c-1", "rev": 1, "status": "open", "path": null, "line": null,
            "body": "b", "author": "a", "createdAt": "t", "updatedAt": "t",
            "agentMetadata": {"tool": "claude"}
          }],
          "futureTopLevel": [1, 2]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        let out = to_json(&outcome.review).expect("serialize");
        assert!(out.contains("customSourceKey"));
        assert!(out.contains("agentMetadata"));
        assert!(out.contains("futureTopLevel"));
    }

    #[test]
    fn missing_required_comment_field_quarantines_record() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [
            {"id": "c-1", "rev": 1, "status": "open", "path": null, "line": null,
             "body": "good", "author": "a", "createdAt": "t", "updatedAt": "t"},
            {"id": "c-2", "rev": 1, "status": "open"}
          ]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert_eq!(outcome.review.comments.len(), 1);
        assert_eq!(outcome.review.quarantined.len(), 1);
        assert_eq!(outcome.review.quarantined[0]["id"], "c-2");
        assert!(!outcome.warnings.is_empty());
        assert!(!outcome.read_only);
    }

    #[test]
    fn wrong_type_comment_field_quarantines_record() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [
            {"id": "c-1", "rev": "one", "status": "open", "path": null, "line": null,
             "body": "b", "author": "a", "createdAt": "t", "updatedAt": "t"}
          ]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.review.comments.is_empty());
        assert_eq!(outcome.review.quarantined.len(), 1);
    }

    #[test]
    fn unknown_status_quarantines_record() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [
            {"id": "c-1", "rev": 1, "status": "wontfix", "path": null, "line": null,
             "body": "b", "author": "a", "createdAt": "t", "updatedAt": "t"}
          ]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.review.comments.is_empty());
        assert_eq!(outcome.review.quarantined.len(), 1);
    }

    #[test]
    fn duplicate_id_quarantines_second_record() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [
            {"id": "c-1", "rev": 1, "status": "open", "path": null, "line": null,
             "body": "first", "author": "a", "createdAt": "t", "updatedAt": "t"},
            {"id": "c-1", "rev": 1, "status": "open", "path": null, "line": null,
             "body": "second", "author": "a", "createdAt": "t", "updatedAt": "t"}
          ]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert_eq!(outcome.review.comments.len(), 1);
        assert_eq!(outcome.review.comments[0].body, "first");
        assert_eq!(outcome.review.quarantined[0]["body"], "second");
    }

    #[test]
    fn newer_schema_major_is_read_only() {
        let json = r#"{
          "ambidiff": 2, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.review.ambidiff, 2);
    }

    #[test]
    fn non_array_comments_is_read_only_and_preserved() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": "oops"
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(
            outcome.read_only_reason.as_deref(),
            Some("comments is not an array (string)")
        );
        assert_eq!(outcome.retained["comments"], "oops");
        assert!(outcome.review.comments.is_empty());
    }

    #[test]
    fn invalid_json_is_a_hard_error() {
        assert!(matches!(
            parse_review("{not json"),
            Err(ReviewError::Json { .. })
        ));
        assert!(matches!(
            parse_review("[1,2]"),
            Err(ReviewError::NotAnObject)
        ));
    }

    #[test]
    fn quarantine_survives_round_trip() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [{"id": "c-2", "rev": 1, "status": "open"}]
        }"#;
        let outcome = parse_review(json).expect("parse");
        let rewritten = to_json(&outcome.review).expect("serialize");
        let again = parse_review(&rewritten).expect("reparse");
        assert_eq!(again.review.quarantined.len(), 1);
        assert_eq!(again.review.quarantined[0]["id"], "c-2");
    }

    #[test]
    fn missing_side_on_line_comment_is_salvaged_to_new() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t",
          "comments": [{"id": "c-1", "rev": 1, "status": "open", "path": "f", "line": 3,
            "body": "b", "author": "a", "createdAt": "t", "updatedAt": "t"}]
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert_eq!(outcome.review.comments[0].side, Some(Side::New));
        assert!(!outcome.warnings.is_empty());
    }

    #[test]
    fn is_question_requires_double_question_mark() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("plain directive"), "c-1".into(), "t")
            .expect("add comment");
        review
            .try_add_comment(new_comment("is this ok?"), "c-2".into(), "t")
            .expect("add comment");
        review
            .try_add_comment(new_comment("why?? explain"), "c-3".into(), "t")
            .expect("add comment");
        assert!(!review.comments[0].is_question());
        assert!(
            !review.comments[1].is_question(),
            "single ? is not the convention"
        );
        assert!(review.comments[2].is_question());
    }

    #[test]
    fn counts_tally_each_status_exactly() {
        let mut review = base_file();
        for (i, body) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            review
                .try_add_comment(new_comment(body), format!("c-{i}"), "t")
                .expect("add comment");
        }
        // c-0,c-1 stay open; c-2,c-3 addressed; c-4 resolved; c-5 reopened.
        for id in ["c-2", "c-3", "c-4", "c-5"] {
            review
                .apply_lifecycle(id, Action::Address, Actor::Agent, None, "t")
                .expect("address");
        }
        review
            .apply_lifecycle("c-4", Action::Resolve, Actor::Human, None, "t")
            .expect("resolve");
        review
            .apply_lifecycle("c-5", Action::Reopen, Actor::Human, None, "t")
            .expect("reopen");
        let counts = review.counts();
        assert_eq!(
            (
                counts.open,
                counts.addressed,
                counts.resolved,
                counts.reopened
            ),
            (2, 2, 1, 1)
        );
        assert_eq!(counts.todo(), 3, "open + reopened");
        assert_eq!(counts.total(), 6);
    }

    #[test]
    fn first_comment_does_not_bump_revision() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        assert_eq!(review.revision, 1);
        assert_eq!(review.comments[0].rev, 1);
    }

    #[test]
    fn comment_after_pass_completion_opens_new_pass() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .try_add_comment(new_comment("b"), "c-2".into(), "t2")
            .expect("add comment");
        assert_eq!(review.revision, 1);

        // Agent addresses both: pass 1 is complete.
        review
            .apply_lifecycle("c-1", Action::Address, Actor::Agent, None, "t3")
            .expect("address");
        review
            .apply_lifecycle(
                "c-2",
                Action::Address,
                Actor::Agent,
                Some("done".into()),
                "t4",
            )
            .expect("address");

        // Next human comment opens pass 2.
        review
            .try_add_comment(new_comment("c"), "c-3".into(), "t5")
            .expect("add comment");
        assert_eq!(review.revision, 2);
        assert_eq!(review.comments[2].rev, 2);
        // Earlier comments keep their origin pass.
        assert_eq!(review.comments[0].rev, 1);
    }

    #[test]
    fn comment_while_pass_incomplete_stays_in_pass() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .apply_lifecycle("c-1", Action::Address, Actor::Agent, None, "t2")
            .expect("address");
        review
            .try_add_comment(new_comment("b"), "c-2".into(), "t3")
            .expect("add comment");
        assert_eq!(review.revision, 2, "all addressed -> new pass");
        review
            .try_add_comment(new_comment("c"), "c-3".into(), "t4")
            .expect("add comment");
        assert_eq!(review.revision, 2, "c-2 still open -> same pass");
    }

    #[test]
    fn reopen_after_pass_completion_opens_new_pass() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .apply_lifecycle("c-1", Action::Address, Actor::Agent, None, "t2")
            .expect("address");
        review
            .apply_lifecycle("c-1", Action::Reopen, Actor::Human, None, "t3")
            .expect("reopen");
        assert_eq!(review.revision, 2);
        // Provenance: the comment keeps the pass it was raised in.
        assert_eq!(review.comments[0].rev, 1);
        assert_eq!(review.comments[0].status, Status::Reopened);
    }

    #[test]
    fn agent_resolve_via_review_is_rejected() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        let err = review
            .apply_lifecycle("c-1", Action::Resolve, Actor::Agent, None, "t2")
            .expect_err("agent resolve must fail");
        assert!(matches!(
            err,
            ReviewError::Lifecycle(LifecycleError::HumanOnly { .. })
        ));
    }

    #[test]
    fn lifecycle_on_missing_id_errors() {
        let mut review = base_file();
        assert!(matches!(
            review.apply_lifecycle("nope", Action::Address, Actor::Agent, None, "t"),
            Err(ReviewError::CommentNotFound { .. })
        ));
    }

    #[test]
    fn rev_bump_is_manual_escape_hatch() {
        let mut review = base_file();
        assert_eq!(review.try_rev_bump("t").expect("rev bump"), 2);
        assert_eq!(review.revision, 2);
    }

    #[test]
    fn validation_partitions() {
        let ok = new_comment("body");
        assert!(validate_new_comment(&ok).is_ok());

        let mut c = new_comment("  ");
        assert!(validate_new_comment(&c).is_err(), "blank body");

        c = new_comment("b");
        c.path = None;
        c.side = None;
        c.line = None;
        assert!(validate_new_comment(&c).is_ok(), "review-level");

        c = new_comment("b");
        c.path = None;
        assert!(validate_new_comment(&c).is_err(), "line without path");

        c = new_comment("b");
        c.line = None;
        c.end_line = Some(4);
        assert!(validate_new_comment(&c).is_err(), "endLine without line");

        c = new_comment("b");
        c.line = None;
        assert!(validate_new_comment(&c).is_err(), "side without line");

        c = new_comment("b");
        c.line = None;
        c.side = None;
        assert!(validate_new_comment(&c).is_ok(), "file-level");

        c = new_comment("b");
        c.end_line = Some(2);
        assert!(validate_new_comment(&c).is_err(), "endLine < line");

        c = new_comment("b");
        c.line = Some(0);
        assert!(validate_new_comment(&c).is_err(), "0 line");
    }

    #[test]
    fn snippet_matching_is_whitespace_insensitive() {
        assert!(snippet_matches(
            "if (x == null) return;",
            "  if (x==null)  return;"
        ));
        assert!(snippet_matches("a\tb", "ab"));
        assert!(!snippet_matches("if (x == null)", "if (x != null)"));
        assert!(snippet_matches("", "   "));
    }

    // --- is_review_sidecar -------------------------------------------------

    #[test]
    fn sidecar_names_are_recognized_review_file_is_not() {
        assert!(is_review_sidecar(LOCK_FILE_NAME));
        assert!(is_review_sidecar(GUARD_FILE_NAME));
        assert!(is_review_sidecar(&format!("{TMP_FILE_PREFIX}abc123")));
        assert!(!is_review_sidecar(REVIEW_FILE_NAME));
        assert!(!is_review_sidecar("unrelated.txt"));
    }

    // --- revision overflow (B03) --------------------------------------------

    #[test]
    fn next_revision_errors_at_u32_max_without_mutating() {
        let mut review = base_file();
        review.revision = u32::MAX;
        assert!(matches!(
            review.next_revision(),
            Err(ReviewError::RevisionExhausted)
        ));
        assert_eq!(review.revision, u32::MAX, "no mutation on failure");
    }

    #[test]
    fn try_rev_bump_errors_at_u32_max_without_mutating() {
        let mut review = base_file();
        review.revision = u32::MAX;
        let before = review.clone();
        assert!(matches!(
            review.try_rev_bump("t"),
            Err(ReviewError::RevisionExhausted)
        ));
        assert_eq!(review, before, "no partial mutation on failure");
    }

    #[test]
    fn try_add_comment_errors_when_pass_bump_would_overflow() {
        let mut review = base_file();
        review.revision = u32::MAX;
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .apply_lifecycle("c-1", Action::Address, Actor::Agent, None, "t2")
            .expect("address");
        // Pass is complete; the next comment would need revision MAX+1.
        let before = review.clone();
        let err = review
            .try_add_comment(new_comment("b"), "c-2".into(), "t3")
            .expect_err("must not overflow");
        assert!(matches!(err, ReviewError::RevisionExhausted));
        assert_eq!(review, before, "no partial mutation: comment not added");
    }

    #[test]
    fn apply_lifecycle_reopen_errors_when_pass_bump_would_overflow() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .apply_lifecycle("c-1", Action::Address, Actor::Agent, None, "t2")
            .expect("address");
        review.revision = u32::MAX;
        let before = review.clone();
        let err = review
            .apply_lifecycle("c-1", Action::Reopen, Actor::Human, None, "t3")
            .expect_err("must not overflow");
        assert!(matches!(err, ReviewError::RevisionExhausted));
        assert_eq!(review, before, "no partial mutation: status unchanged");
    }

    #[test]
    fn revision_out_of_range_is_retained_and_read_only() {
        for revision in ["0", "-1", "1.5", "4294967296"] {
            let json = format!(
                r#"{{
                  "ambidiff": 1, "review": "r", "revision": {revision},
                  "source": {{"kind": "git"}}, "createdAt": "t", "updatedAt": "t",
                  "comments": []
                }}"#
            );
            let outcome = parse_review(&json).expect("parse");
            assert!(outcome.read_only, "revision {revision} must be read-only");
            assert!(
                outcome.retained.contains_key("revision"),
                "revision {revision} must be retained"
            );
            assert_eq!(outcome.review.revision, 1, "falls back to 1 for reads");
        }
    }

    // --- try_add_comment / edit_comment / delete_comment --------------------

    #[test]
    fn try_add_comment_rejects_duplicate_id_without_mutating() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        let before = review.clone();
        let err = review
            .try_add_comment(new_comment("b"), "c-1".into(), "t2")
            .expect_err("duplicate id must be rejected");
        assert!(matches!(err, ReviewError::DuplicateId { id } if id == "c-1"));
        assert_eq!(review, before);
    }

    #[test]
    fn try_add_comment_rejects_invalid_input_without_mutating() {
        let mut review = base_file();
        let mut bad = new_comment("body");
        bad.body = "   ".to_string();
        let before = review.clone();
        assert!(review.try_add_comment(bad, "c-1".into(), "t").is_err());
        assert_eq!(review, before);
    }

    #[test]
    fn edit_comment_replaces_body_and_bumps_updated_at() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("original"), "c-1".into(), "t1")
            .expect("add comment");
        let updated = review.edit_comment("c-1", "revised", "t2").expect("edit");
        assert_eq!(updated.body, "revised");
        assert_eq!(updated.updated_at, "t2");
    }

    #[test]
    fn edit_comment_rejects_blank_body() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("original"), "c-1".into(), "t1")
            .expect("add comment");
        assert!(matches!(
            review.edit_comment("c-1", "   ", "t2"),
            Err(ReviewError::Validation { .. })
        ));
        assert_eq!(review.comments[0].body, "original");
    }

    #[test]
    fn edit_comment_missing_id_errors() {
        let mut review = base_file();
        assert!(matches!(
            review.edit_comment("nope", "text", "t"),
            Err(ReviewError::CommentNotFound { .. })
        ));
    }

    #[test]
    fn delete_comment_removes_and_returns_it() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("keep"), "c-1".into(), "t1")
            .expect("add comment");
        review
            .try_add_comment(new_comment("remove me"), "c-2".into(), "t2")
            .expect("add comment");
        let removed = review.delete_comment("c-2", "t3").expect("delete");
        assert_eq!(removed.id, "c-2");
        assert_eq!(review.comments.len(), 1);
        assert_eq!(review.comments[0].id, "c-1");
    }

    #[test]
    fn delete_comment_missing_id_errors_without_mutating() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("keep"), "c-1".into(), "t1")
            .expect("add comment");
        let before = review.clone();
        assert!(matches!(
            review.delete_comment("nope", "t"),
            Err(ReviewError::CommentNotFound { .. })
        ));
        assert_eq!(review, before);
    }

    #[test]
    fn apply_lifecycle_empty_response_stores_nothing() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        let updated = review
            .apply_lifecycle(
                "c-1",
                Action::Address,
                Actor::Agent,
                Some(String::new()),
                "t2",
            )
            .expect("address");
        assert_eq!(updated.response, None);
    }

    // --- validate() ----------------------------------------------------------

    #[test]
    fn validate_accepts_a_well_formed_review() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        assert!(review.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_revision() {
        let mut review = base_file();
        review.revision = 0;
        assert!(matches!(
            review.validate(),
            Err(ReviewError::Validation { .. })
        ));
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review.comments.push(review.comments[0].clone());
        assert!(matches!(
            review.validate(),
            Err(ReviewError::DuplicateId { .. })
        ));
    }

    #[test]
    fn validate_rejects_invalid_id() {
        let mut review = base_file();
        review
            .try_add_comment(new_comment("a"), "c-1".into(), "t1")
            .expect("add comment");
        review.comments[0].id = "bad id!".to_string();
        assert!(matches!(
            review.validate(),
            Err(ReviewError::Validation { .. })
        ));
    }

    // --- salvage retention for other top-level fields (B02) -----------------

    #[test]
    fn quarantine_container_that_is_an_object_is_retained_not_dropped() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": [],
          "quarantined": {"not": "an array"}
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["quarantined"]["not"], "an array");
    }

    #[test]
    fn unreadable_source_object_is_retained_not_replaced_silently() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1,
          "source": {"kind": 123},
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["source"]["kind"], 123);
    }

    #[test]
    fn non_object_source_is_retained() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": "git",
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["source"], "git");
    }

    #[test]
    fn non_string_review_name_is_retained() {
        let json = r#"{
          "ambidiff": 1, "review": 42, "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["review"], 42);
    }

    #[test]
    fn non_string_timestamp_is_retained() {
        let json = r#"{
          "ambidiff": 1, "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": 1234, "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["createdAt"], 1234);
    }

    #[test]
    fn non_integer_ambidiff_is_retained() {
        let json = r#"{
          "ambidiff": "one", "review": "r", "revision": 1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(outcome.retained["ambidiff"], "one");
    }

    #[test]
    fn missing_fields_stay_writable_with_warnings() {
        let json = r#"{"comments": []}"#;
        let outcome = parse_review(json).expect("parse");
        assert!(!outcome.read_only, "missing fields alone are not read-only");
        assert!(outcome.retained.is_empty());
        assert!(!outcome.warnings.is_empty());
    }

    #[test]
    fn read_only_reason_is_the_first_triggering_warning() {
        let json = r#"{
          "ambidiff": 1, "review": 42, "revision": -1, "source": {"kind": "git"},
          "createdAt": "t", "updatedAt": "t", "comments": []
        }"#;
        let outcome = parse_review(json).expect("parse");
        assert!(outcome.read_only);
        assert_eq!(
            outcome.read_only_reason.as_deref(),
            Some("review name 42 is not a string")
        );
    }
}
