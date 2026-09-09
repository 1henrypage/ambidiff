//! ambidiff: one binary carrying the CLI verbs, the TUI, the stdio engine
//! for editor plugins, and the loopback web server.

mod application;
mod args;
mod context;
mod engine;
mod tui;
mod verbs;
mod web;

use clap::Parser;

use crate::args::{Cli, Command, CommentCommand, RevCommand, TuiArgs};

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        None => run_tui(TuiArgs::default()),
        Some(Command::Tui(args)) => run_tui(args),
        Some(Command::Init(args)) => verbs::init(args),
        Some(Command::Status(args)) => verbs::status(args),
        Some(Command::Comment(cmd)) => match cmd {
            CommentCommand::Add(args) => verbs::comment_add(args),
            CommentCommand::List(args) => verbs::comment_list(args),
            CommentCommand::Show(args) => verbs::comment_show(args),
            CommentCommand::Addressed(args) => verbs::comment_addressed(args),
            CommentCommand::Resolve(args) => verbs::comment_resolve(args),
            CommentCommand::Reopen(args) => verbs::comment_reopen(args),
            CommentCommand::Edit(args) => verbs::comment_edit(args),
            CommentCommand::Delete(args) => verbs::comment_delete(args),
        },
        Some(Command::Rev(RevCommand::Bump(args))) => verbs::rev_bump(args),
        Some(Command::AgentSetup(args)) => verbs::agent_setup(args),
        Some(Command::Engine(args)) => run_engine(args),
        Some(Command::Web(args)) => run_web(args),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // Errors quote review-derived text (ids, paths, bodies), so the
            // terminal boundary sanitises every line of the message.
            for line in format!("{err:#}").lines() {
                eprintln!("ambidiff: {}", ambidiff_core::sanitize::sanitize_line(line));
            }
            std::process::exit(1);
        }
    }
}

fn run_tui(args: TuiArgs) -> anyhow::Result<i32> {
    tui::run(args)
}

fn run_engine(args: args::EngineArgs) -> anyhow::Result<i32> {
    engine::run(args)
}

fn run_web(args: args::WebArgs) -> anyhow::Result<i32> {
    web::run(args)
}
