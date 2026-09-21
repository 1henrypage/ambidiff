//! Stacked-PR vocabulary: targets, the assembled stack, and the policy that
//! turns raw git facts into it.
//!
//! A stack is a linear chain of commits on one branch above trunk. Every PR
//! is a local branch pointing at its tip commit; child PRs physically
//! contain their parents' commits. Reviewing PR `n` means diffing
//! `tip(PR n-1)..tip(PR n)`. PR identity is the BRANCH NAME: an amend, a
//! `git rebase --update-refs`, or a bottom merge moves every tip oid but
//! keeps the names, so comments tagged with a branch survive all of them.
//! Oids are a cache for display and comparison, never a key.
//!
//! This module is pure (no git): [`assemble_stack`] takes the facts
//! `git_source` discovers and applies every policy decision, so the
//! partitions are unit-testable without a repository, and the wasm painter
//! shares the exact same vocabulary the native side serialises.

use serde::{Deserialize, Serialize};

use crate::source::{Comparison, Endpoint};

/// Which comparison inside a stack review a comment or a view refers to.
/// Serialises as `{"kind":"branch","name":"auth-2"}` / `{"kind":"head"}` /
/// `{"kind":"stack"}` / `{"kind":"worktree"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetId {
    /// One PR: the local branch at its tip commit.
    Branch { name: String },
    /// Commits above the topmost PR branch up to `HEAD` (a detached HEAD, or
    /// work not yet given a branch).
    Head,
    /// The whole stack: merge-base with trunk up to `HEAD`.
    Stack,
    /// Uncommitted changes: `HEAD` against the working tree.
    Worktree,
}

impl TargetId {
    /// Human label: the branch name, or the kind.
    pub fn label(&self) -> &str {
        match self {
            TargetId::Branch { name } => name,
            TargetId::Head => "head",
            TargetId::Stack => "stack",
            TargetId::Worktree => "worktree",
        }
    }

    /// Stable string key (`branch:<name>` or the kind), for maps and hashes
    /// on the browser side where object identity is not usable.
    pub fn key(&self) -> String {
        match self {
            TargetId::Branch { name } => format!("branch:{name}"),
            other => other.label().to_string(),
        }
    }

    /// Parse the CLI spelling: `stack`, `worktree`, `head` are keywords;
    /// `branch:<name>` forces a branch (for a branch that happens to be
    /// named like a keyword); anything else is a branch name.
    pub fn parse_cli(s: &str) -> Result<TargetId, String> {
        match s {
            "stack" => Ok(TargetId::Stack),
            "worktree" => Ok(TargetId::Worktree),
            "head" => Ok(TargetId::Head),
            "" => Err("target must not be empty".to_string()),
            other => {
                let name = other.strip_prefix("branch:").unwrap_or(other);
                if name.is_empty() {
                    return Err("branch target needs a name after `branch:`".to_string());
                }
                Ok(TargetId::Branch {
                    name: name.to_string(),
                })
            }
        }
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One reviewable comparison inside a stack. Every field is serialised
/// (no `skip_serializing_if`) so the wire fixtures are unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: TargetId,
    pub label: String,
    /// 1-based position among the PR targets (bottom to top); `None` for
    /// the head, stack, and worktree targets.
    pub position: Option<u32>,
    /// Tip commit oid; `None` for the worktree target.
    pub tip: Option<String>,
    /// Commits this target adds over the previous one.
    pub commit_count: u32,
    /// Tip commit subject, display only (never matched on).
    pub subject: Option<String>,
    /// Other branches pointing at the same tip.
    pub aliases: Vec<String>,
    pub comparison: Comparison,
}

/// A discovered stack: the trunk it sits on, its endpoints, and the ordered
/// targets (PR tips bottom to top, then `head` when HEAD has no branch,
/// then `stack`, then `worktree` when the tree is dirty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stack {
    /// The trunk ref as resolved (e.g. `refs/remotes/origin/main`).
    pub trunk: String,
    /// Merge-base of trunk and HEAD (the bottom of the stack).
    pub base: String,
    /// HEAD's commit oid.
    pub head: String,
    pub targets: Vec<Target>,
}

impl Stack {
    pub fn find(&self, id: &TargetId) -> Option<&Target> {
        self.targets.iter().find(|t| &t.id == id)
    }

    pub fn ids(&self) -> Vec<TargetId> {
        self.targets.iter().map(|t| t.id.clone()).collect()
    }

    /// What opens when nothing was asked for: the topmost PR, else the
    /// whole stack, else the worktree.
    pub fn default_target(&self) -> Option<TargetId> {
        self.targets
            .iter()
            .rev()
            .find(|t| matches!(t.id, TargetId::Branch { .. }))
            .or_else(|| self.find(&TargetId::Stack))
            .or_else(|| self.find(&TargetId::Worktree))
            .map(|t| t.id.clone())
    }

    /// Where a selection lands when its branch leaves the stack: the whole
    /// stack, never a neighbouring PR (that would silently show someone
    /// else's diff under the old name).
    pub fn fallback_target(&self) -> Option<TargetId> {
        self.find(&TargetId::Stack)
            .map(|t| t.id.clone())
            .or_else(|| self.default_target())
    }

    /// A string that changes whenever the strip must repaint: any tip move,
    /// branch added or removed, alias change, or dirty flip. Feeds the diff
    /// signature so the watch controller notices a restack.
    pub fn shape_key(&self) -> String {
        let mut key = format!("{}\u{1}{}\u{1}", self.base, self.head);
        for target in &self.targets {
            key.push_str(&target.id.key());
            key.push('=');
            key.push_str(target.tip.as_deref().unwrap_or("-"));
            key.push('[');
            key.push_str(&target.aliases.join(","));
            key.push_str("];");
        }
        key
    }
}

/// One commit of the first-parent chain `base..head`, bottom first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainCommit {
    pub oid: String,
    pub subject: String,
}

/// The raw facts discovery collects; [`assemble_stack`] applies the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackInput {
    pub trunk: String,
    pub base: String,
    pub head: String,
    /// `base..head`, bottom to top. Empty when HEAD is at (or below) trunk.
    pub chain: Vec<ChainCommit>,
    /// Every local branch as `(oid, name)`.
    pub branches: Vec<(String, String)>,
    /// The checked-out branch, if HEAD is not detached.
    pub current_branch: Option<String>,
    /// The working tree (or index) differs from HEAD.
    pub dirty: bool,
}

fn commit(oid: &str) -> Endpoint {
    Endpoint::Commit {
        oid: oid.to_string(),
    }
}

/// Turn discovery facts into a stack. Only branches whose oid is IN the
/// chain are PR tips (a branch at the merge-base or below is not part of
/// this stack: that is how a merged bottom PR disappears). Several branches
/// on one commit: the current branch wins, else alphabetical; the rest are
/// aliases. A top commit without a branch adds `head`; a non-empty chain
/// adds `stack`; a dirty tree adds `worktree`.
pub fn assemble_stack(input: StackInput) -> Stack {
    let StackInput {
        trunk,
        base,
        head,
        chain,
        branches,
        current_branch,
        dirty,
    } = input;

    let mut targets = Vec::new();
    let mut previous_tip = base.clone();
    let mut previous_index: Option<usize> = None;
    let mut position = 0u32;

    for (index, commit_at) in chain.iter().enumerate() {
        let mut names: Vec<&str> = branches
            .iter()
            .filter(|(oid, _)| oid == &commit_at.oid)
            .map(|(_, name)| name.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        names.sort_unstable();
        names.dedup();
        if let Some(current) = current_branch.as_deref()
            && let Some(at) = names.iter().position(|n| *n == current)
        {
            names.rotate_left(at);
            // Keep the rest alphabetical: rotating moved the head of the
            // list behind the current branch, so re-sort the tail.
            names[1..].sort_unstable();
        }
        position += 1;
        let commit_count = match previous_index {
            Some(prev) => index - prev,
            None => index + 1,
        };
        targets.push(Target {
            id: TargetId::Branch {
                name: names[0].to_string(),
            },
            label: names[0].to_string(),
            position: Some(position),
            tip: Some(commit_at.oid.clone()),
            commit_count: commit_count as u32,
            subject: Some(commit_at.subject.clone()),
            aliases: names[1..].iter().map(|n| n.to_string()).collect(),
            comparison: Comparison {
                old: commit(&previous_tip),
                new: commit(&commit_at.oid),
            },
        });
        previous_tip = commit_at.oid.clone();
        previous_index = Some(index);
    }

    if let Some(top) = chain.last()
        && previous_index != Some(chain.len() - 1)
    {
        let commit_count = chain.len() - previous_index.map_or(0, |i| i + 1);
        targets.push(Target {
            id: TargetId::Head,
            label: TargetId::Head.label().to_string(),
            position: None,
            tip: Some(head.clone()),
            commit_count: commit_count as u32,
            subject: Some(top.subject.clone()),
            aliases: Vec::new(),
            comparison: Comparison {
                old: commit(&previous_tip),
                new: commit(&head),
            },
        });
    }

    if !chain.is_empty() {
        targets.push(Target {
            id: TargetId::Stack,
            label: TargetId::Stack.label().to_string(),
            position: None,
            tip: Some(head.clone()),
            commit_count: chain.len() as u32,
            subject: None,
            aliases: Vec::new(),
            comparison: Comparison {
                old: commit(&base),
                new: commit(&head),
            },
        });
    }

    if dirty {
        targets.push(Target {
            id: TargetId::Worktree,
            label: TargetId::Worktree.label().to_string(),
            position: None,
            tip: None,
            commit_count: 0,
            subject: None,
            aliases: Vec::new(),
            comparison: Comparison {
                old: commit(&head),
                new: Endpoint::Worktree,
            },
        });
    }

    Stack {
        trunk,
        base,
        head,
        targets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn oid(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    fn chain(subjects: &[(char, &str)]) -> Vec<ChainCommit> {
        subjects
            .iter()
            .map(|(c, s)| ChainCommit {
                oid: oid(*c),
                subject: s.to_string(),
            })
            .collect()
    }

    fn input() -> StackInput {
        StackInput {
            trunk: "refs/remotes/origin/main".into(),
            base: oid('1'),
            head: oid('3'),
            chain: chain(&[('2', "Add login form"), ('3', "Wire the session cookie")]),
            branches: vec![
                (oid('1'), "main".into()),
                (oid('2'), "auth-1".into()),
                (oid('3'), "auth-2".into()),
            ],
            current_branch: Some("auth-2".into()),
            dirty: true,
        }
    }

    fn branch(name: &str) -> TargetId {
        TargetId::Branch { name: name.into() }
    }

    #[test]
    fn three_commits_two_branches_assemble_in_order() {
        let stack = assemble_stack(input());
        assert_eq!(
            stack.ids(),
            vec![
                branch("auth-1"),
                branch("auth-2"),
                TargetId::Stack,
                TargetId::Worktree
            ]
        );
        let first = &stack.targets[0];
        assert_eq!(first.position, Some(1));
        assert_eq!(first.commit_count, 1);
        assert_eq!(first.subject.as_deref(), Some("Add login form"));
        assert_eq!(first.comparison.old, Endpoint::Commit { oid: oid('1') });
        assert_eq!(first.comparison.new, Endpoint::Commit { oid: oid('2') });
        let second = &stack.targets[1];
        assert_eq!(second.position, Some(2));
        assert_eq!(second.comparison.old, Endpoint::Commit { oid: oid('2') });
        assert_eq!(second.comparison.new, Endpoint::Commit { oid: oid('3') });
        let whole = &stack.targets[2];
        assert_eq!(whole.commit_count, 2);
        assert_eq!(whole.subject, None);
        assert_eq!(whole.comparison.old, Endpoint::Commit { oid: oid('1') });
        let worktree = &stack.targets[3];
        assert_eq!(worktree.tip, None);
        assert_eq!(worktree.comparison.new, Endpoint::Worktree);
        assert_eq!(worktree.comparison.old, Endpoint::Commit { oid: oid('3') });
    }

    #[test]
    fn a_branch_at_the_base_is_not_a_pr() {
        // `main` sits at the merge-base and must not become a target; that
        // is also how a bottom PR fast-forwarded into trunk disappears.
        let stack = assemble_stack(input());
        assert!(stack.find(&branch("main")).is_none());
    }

    #[test]
    fn several_branches_on_one_tip_prefer_current_then_alphabetical() {
        let mut input = input();
        input.branches.push((oid('3'), "zeta".into()));
        input.branches.push((oid('3'), "alpha".into()));
        let stack = assemble_stack(input.clone());
        let top = stack.find(&branch("auth-2")).expect("current branch wins");
        assert_eq!(top.aliases, vec!["alpha".to_string(), "zeta".to_string()]);

        input.current_branch = None;
        let stack = assemble_stack(input);
        let top = &stack.targets[1];
        assert_eq!(top.id, branch("alpha"), "alphabetical without a current");
        assert_eq!(top.aliases, vec!["auth-2".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn commits_above_the_last_branch_add_a_head_target() {
        let mut input = input();
        input.head = oid('5');
        input.chain = chain(&[
            ('2', "Add login form"),
            ('3', "Wire the session cookie"),
            ('4', "wip"),
            ('5', "more wip"),
        ]);
        input.current_branch = None;
        input.dirty = false;
        let stack = assemble_stack(input);
        assert_eq!(
            stack.ids(),
            vec![
                branch("auth-1"),
                branch("auth-2"),
                TargetId::Head,
                TargetId::Stack
            ]
        );
        let head = stack.find(&TargetId::Head).expect("head");
        assert_eq!(head.commit_count, 2);
        assert_eq!(head.subject.as_deref(), Some("more wip"));
        assert_eq!(head.comparison.old, Endpoint::Commit { oid: oid('3') });
        assert_eq!(head.comparison.new, Endpoint::Commit { oid: oid('5') });
        assert_eq!(stack.find(&TargetId::Stack).expect("stack").commit_count, 4);
    }

    #[test]
    fn detached_head_without_any_branch_is_head_plus_stack() {
        let mut input = input();
        input.branches = vec![(oid('1'), "main".into())];
        input.current_branch = None;
        input.dirty = false;
        let stack = assemble_stack(input);
        assert_eq!(stack.ids(), vec![TargetId::Head, TargetId::Stack]);
        assert_eq!(stack.targets[0].commit_count, 2);
        assert_eq!(stack.default_target(), Some(TargetId::Stack));
    }

    #[test]
    fn empty_chain_is_no_targets_when_clean_and_worktree_when_dirty() {
        let mut input = input();
        input.head = oid('1');
        input.chain = Vec::new();
        input.dirty = false;
        let clean = assemble_stack(input.clone());
        assert!(clean.targets.is_empty());
        assert_eq!(clean.default_target(), None);
        assert_eq!(clean.fallback_target(), None);

        input.dirty = true;
        let dirty = assemble_stack(input);
        assert_eq!(dirty.ids(), vec![TargetId::Worktree]);
        assert_eq!(dirty.default_target(), Some(TargetId::Worktree));
        assert_eq!(dirty.fallback_target(), Some(TargetId::Worktree));
    }

    #[test]
    fn default_is_the_topmost_pr_and_fallback_is_the_stack() {
        let stack = assemble_stack(input());
        assert_eq!(stack.default_target(), Some(branch("auth-2")));
        assert_eq!(stack.fallback_target(), Some(TargetId::Stack));
    }

    #[test]
    fn shape_key_moves_on_tip_branch_alias_and_dirty_changes() {
        let base = assemble_stack(input()).shape_key();
        assert_eq!(assemble_stack(input()).shape_key(), base, "deterministic");

        let mut retipped = input();
        retipped.chain[1].oid = oid('9');
        retipped.head = oid('9');
        retipped.branches[2].0 = oid('9');
        assert_ne!(assemble_stack(retipped).shape_key(), base, "tip move");

        let mut removed = input();
        removed.branches.retain(|(_, n)| n != "auth-1");
        assert_ne!(assemble_stack(removed).shape_key(), base, "branch removed");

        let mut aliased = input();
        aliased.branches.push((oid('3'), "alias".into()));
        assert_ne!(assemble_stack(aliased).shape_key(), base, "alias added");

        let mut clean = input();
        clean.dirty = false;
        assert_ne!(assemble_stack(clean).shape_key(), base, "dirty flip");

        let mut resubjected = input();
        resubjected.chain[1].subject = "reworded".into();
        assert_eq!(
            assemble_stack(resubjected).shape_key(),
            base,
            "subjects are display only"
        );
    }

    #[test]
    fn parse_cli_keywords_prefix_and_bare_names() {
        assert_eq!(TargetId::parse_cli("stack"), Ok(TargetId::Stack));
        assert_eq!(TargetId::parse_cli("worktree"), Ok(TargetId::Worktree));
        assert_eq!(TargetId::parse_cli("head"), Ok(TargetId::Head));
        assert_eq!(TargetId::parse_cli("auth-2"), Ok(branch("auth-2")));
        assert_eq!(TargetId::parse_cli("branch:stack"), Ok(branch("stack")));
        assert!(TargetId::parse_cli("").is_err());
        assert!(TargetId::parse_cli("branch:").is_err());
    }

    #[test]
    fn serde_spellings_are_tagged_objects() {
        assert_eq!(
            serde_json::to_value(branch("auth-2")).expect("json"),
            json!({"kind": "branch", "name": "auth-2"})
        );
        assert_eq!(
            serde_json::to_value(TargetId::Worktree).expect("json"),
            json!({"kind": "worktree"})
        );
        let parsed: TargetId = serde_json::from_value(json!({"kind": "stack"})).expect("parse");
        assert_eq!(parsed, TargetId::Stack);
        assert!(serde_json::from_value::<TargetId>(json!({"kind": "branch"})).is_err());
        assert!(serde_json::from_value::<TargetId>(json!("stack")).is_err());

        let target = &assemble_stack(input()).targets[3];
        let json = serde_json::to_value(target).expect("json");
        assert_eq!(
            json,
            json!({
                "id": {"kind": "worktree"},
                "label": "worktree",
                "position": null,
                "tip": null,
                "commitCount": 0,
                "subject": null,
                "aliases": [],
                "comparison": {"old": {"kind": "commit", "oid": oid('3')}, "new": {"kind": "worktree"}}
            }),
            "every field present, none skipped"
        );
        let back: Target = serde_json::from_value(json).expect("round trip");
        assert_eq!(&back, target);
    }

    #[test]
    fn labels_keys_and_display() {
        assert_eq!(branch("x").label(), "x");
        assert_eq!(branch("x").key(), "branch:x");
        assert_eq!(TargetId::Stack.key(), "stack");
        assert_eq!(TargetId::Head.to_string(), "head");
    }
}
