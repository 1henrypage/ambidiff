//! Command-line interface definitions.

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
    /// Git base to diff against (a ref like "main", or a range "A..B");
    /// omitted means working tree vs index
    #[arg(long)]
    pub base: Option<String>,
    /// Review staged changes only
    #[arg(long)]
    pub staged: bool,
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
}

#[derive(Args)]
pub struct EngineArgs {
    /// Speak newline-delimited JSON on stdin/stdout (required)
    #[arg(long)]
    pub stdio: bool,
}

#[derive(Args)]
pub struct WebArgs {
    /// Port to bind on 127.0.0.1 (0 picks a free port)
    #[arg(long, default_value = "0")]
    pub port: u16,
    /// Open the browser automatically
    #[arg(long)]
    pub open: bool,
}
