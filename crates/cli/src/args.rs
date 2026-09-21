//! Command-line interface definitions.

use ambidiff_core::review::Source;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ambidiff",
    version,
    about = "Git-backed diff review for humans and AI agents",
    long_about = "A diff viewer with a file-based review representation (.ambidiff.json) that\n\
                  AI agents working in the same tree read and write directly. Run with no\n\
                  arguments to open the TUI."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// TUI options accepted without a subcommand (`ambidiff --commit HEAD`,
    /// `ambidiff --stack`).
    #[command(flatten)]
    pub tui: TuiArgs,
}

/// How to choose the comparison when opening a frontend without (or ahead
/// of) a review file, and what `init` records. Absent means today's
/// meaning: the review file decides, and `init` records a base.
#[derive(Args, Default, Clone, Debug)]
pub struct SourceFlags {
    /// Review one commit (a ref like HEAD, abc123, or main~2) against its
    /// first parent
    #[arg(long, value_name = "REF", conflicts_with_all = ["stack", "upstream"])]
    pub commit: Option<String>,
    /// Review a stack of PR branches above trunk, one target per branch
    #[arg(long)]
    pub stack: bool,
    /// The trunk the stack sits on (default: origin/HEAD, then origin/main,
    /// origin/master, main, master)
    #[arg(long, value_name = "REF", requires = "stack")]
    pub upstream: Option<String>,
}

impl SourceFlags {
    /// The review source these flags ask for, if any.
    pub fn source(&self) -> Option<Source> {
        if let Some(spec) = &self.commit {
            return Some(Source::git_commit(spec.clone()));
        }
        if self.stack {
            return Some(Source::git_stack(self.upstream.clone()));
        }
        None
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a review file (.ambidiff.json) at the current directory
    Init(InitArgs),
    /// Show review status; exits 10 when actionable comments exist
    Status(StatusArgs),
    /// Manage review comments
    #[command(subcommand)]
    Comment(CommentCommand),
    /// Manage review passes (revisions)
    #[command(subcommand)]
    Rev(RevCommand),
    /// Install the agent instruction block into AGENTS.md / CLAUDE.md
    AgentSetup(AgentSetupArgs),
    /// Open the terminal UI (the default when no subcommand is given)
    Tui(TuiArgs),
    /// Run the JSON-RPC engine over stdio (used by editor plugins)
    Engine(EngineArgs),
    /// Serve the browser frontend on loopback
    Web(WebArgs),
}

#[derive(Args)]
pub struct InitArgs {
    /// Review name (defaults to the directory name)
    #[arg(long)]
    pub review: Option<String>,
    /// What to review: a ref like "main" reviews the current branch from
    /// its merge base with that ref (branch commits plus staged, unstaged
    /// and untracked changes); "A..B" compares two commits; "A...B"
    /// compares B with its merge base with A; omitted means working tree
    /// vs index
    #[arg(long, conflicts_with_all = ["commit", "stack"])]
    pub base: Option<String>,
    /// Review staged changes only
    #[arg(long, conflicts_with_all = ["commit", "stack"])]
    pub staged: bool,
    #[command(flatten)]
    pub source: SourceFlags,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum CommentCommand {
    /// Add a comment (review-level, file-level with --path, or line-level
    /// with --path and --line)
    Add(CommentAddArgs),
    /// List comments
    List(CommentListArgs),
    /// Show one comment
    Show(CommentIdArgs),
    /// Mark a comment addressed, optionally with a one-line response
    /// (the agent verb)
    Addressed(CommentAddressedArgs),
    /// Resolve a comment (human only; agents must never run this)
    Resolve(CommentIdArgs),
    /// Reopen a comment (human only; agents must never run this)
    Reopen(CommentIdArgs),
    /// Edit a comment body
    Edit(CommentEditArgs),
    /// Delete a comment entirely (prefer resolve; human only)
    Delete(CommentIdArgs),
}

#[derive(Args)]
pub struct CommentAddArgs {
    /// File path relative to the review root
    #[arg(long, short = 'p')]
    pub path: Option<String>,
    /// 1-based line number (requires --path)
    #[arg(long, short = 'l')]
    pub line: Option<u32>,
    /// Last line of a range (requires --line)
    #[arg(long)]
    pub end_line: Option<u32>,
    /// Which side the line number refers to: removed lines live on "old",
    /// added and unchanged lines on "new"
    #[arg(long, value_parser = ["old", "new"])]
    pub side: Option<String>,
    /// Comment body; end a question with ?? so agents answer instead of
    /// changing code
    #[arg(long, short = 'm')]
    pub body: String,
    /// Author name (default: $AMBIDIFF_AUTHOR, then $USER)
    #[arg(long)]
    pub author: Option<String>,
    /// Stack target the comment is made against: a PR branch name, or
    /// "stack", "worktree", "head" ("branch:<name>" forces a branch).
    /// Required for path comments in a stack review
    #[arg(long, value_name = "TARGET")]
    pub target: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CommentListArgs {
    /// Filter by status (comma-separated: open,addressed,resolved,reopened)
    #[arg(long)]
    pub status: Option<String>,
    /// Only actionable comments (open + reopened): the agent to-do list
    #[arg(long)]
    pub todo: bool,
    /// Filter by file path
    #[arg(long, short = 'p')]
    pub path: Option<String>,
    /// Filter by stack target (same spellings as `comment add --target`)
    #[arg(long, value_name = "TARGET")]
    pub target: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CommentIdArgs {
    /// Comment id (e.g. c-7f3a2b1c)
    pub id: String,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CommentAddressedArgs {
    /// Comment id
    pub id: String,
    /// One-line response describing what was done
    #[arg(long, short = 'm')]
    pub response: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CommentEditArgs {
    /// Comment id
    pub id: String,
    /// New body
    #[arg(long, short = 'm')]
    pub body: String,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum RevCommand {
    /// Manually start the next review pass
    Bump(RevBumpArgs),
}

#[derive(Args)]
pub struct RevBumpArgs {
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct AgentSetupArgs {
    /// Verify the block is present and current instead of writing; exits 1
    /// when setup is needed
    #[arg(long)]
    pub check: bool,
    /// Also install the Claude Code skill at
    /// .claude/skills/ambidiff-review/SKILL.md
    #[arg(long)]
    pub claude_skill: bool,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Default)]
pub struct TuiArgs {
    /// Start in side-by-side layout
    #[arg(long)]
    pub split: bool,
    /// Use the light theme
    #[arg(long)]
    pub light: bool,
    #[command(flatten)]
    pub source: SourceFlags,
}

#[derive(Args)]
pub struct EngineArgs {
    /// Speak newline-delimited JSON on stdin/stdout (required)
    #[arg(long)]
    pub stdio: bool,
    #[command(flatten)]
    pub source: SourceFlags,
}

#[derive(Args)]
pub struct WebArgs {
    /// Port to bind on 127.0.0.1 (0 picks a free port)
    #[arg(long, default_value = "0")]
    pub port: u16,
    /// Open the browser automatically
    #[arg(long)]
    pub open: bool,
    #[command(flatten)]
    pub source: SourceFlags,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn source_flags_parse_without_a_subcommand_and_map_to_sources() {
        let cli = Cli::parse_from(["ambidiff", "--commit", "HEAD"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.tui.source.source(), Some(Source::git_commit("HEAD")));
        let cli = Cli::parse_from(["ambidiff", "--stack", "--upstream", "origin/main"]);
        assert_eq!(
            cli.tui.source.source(),
            Some(Source::git_stack(Some("origin/main".into())))
        );
        let cli = Cli::parse_from(["ambidiff"]);
        assert_eq!(cli.tui.source.source(), None);
        let cli = Cli::parse_from(["ambidiff", "web", "--stack"]);
        let Some(Command::Web(web)) = cli.command else {
            panic!("web subcommand");
        };
        assert!(web.source.stack);
    }

    #[test]
    fn conflicting_source_flags_are_rejected() {
        assert!(Cli::try_parse_from(["ambidiff", "--commit", "HEAD", "--stack"]).is_err());
        assert!(Cli::try_parse_from(["ambidiff", "--upstream", "main"]).is_err());
        assert!(Cli::try_parse_from(["ambidiff", "init", "--base", "main", "--stack"]).is_err());
        assert!(Cli::try_parse_from(["ambidiff", "init", "--staged", "--commit", "HEAD"]).is_err());
        assert!(Cli::try_parse_from(["ambidiff", "init", "--stack", "--upstream", "main"]).is_ok());
    }
}
