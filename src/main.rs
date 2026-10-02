use std::path::PathBuf;

use clap::{Parser, Subcommand};
use forkstack::commands::{
    self, amend::AmendArgs, delete::DeleteArgs, log::LogArgs, rewrite::RewriteArgs,
    submit::SubmitArgs, ui::UiArgs,
};
use forkstack::ui::model::MoveMode;

#[derive(Debug, Parser)]
#[command(about = "Create a stack of one-commit pull requests inside your own fork")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Amend the checked-out stack layer and replay its descendants.
    Amend(AmendArgs),
    /// Delete a Forkstack layer or its lower stack.
    Delete(DeleteArgs),
    /// Show a Git graph of stacks and branches.
    Log(LogArgs),
    /// Move a commit or substack onto a destination.
    Move(RewriteArgs),
    /// Reorder a commit or substack while preserving destination descendants.
    Reorder(RewriteArgs),
    /// Plan, create, or publish a stack of branches.
    Submit(SubmitArgs),
    /// Open the interactive stack graph.
    Ui(UiArgs),
    #[command(hide = true)]
    SequenceEditor { order: PathBuf, todo: PathBuf },
    #[command(hide = true)]
    TodoEditor { prepared: PathBuf, todo: PathBuf },
}

fn main() {
    let result = match Cli::parse().command {
        Command::Amend(args) => commands::amend::run(args),
        Command::Delete(args) => commands::delete::run(args),
        Command::Log(args) => commands::log::run(args),
        Command::Move(args) => commands::rewrite::run(args, MoveMode::Direct),
        Command::Reorder(args) => commands::rewrite::run(args, MoveMode::Reorder),
        Command::Submit(args) => commands::submit::run(args),
        Command::Ui(args) => commands::ui::run(args),
        Command::SequenceEditor { order, todo } => {
            forkstack::ui::git::run_sequence_editor(&order, &todo)
        }
        Command::TodoEditor { prepared, todo } => {
            forkstack::ui::git::run_todo_editor(&prepared, &todo)
        }
    };
    if let Err(error) = result {
        eprintln!("forkstack: {error}");
        std::process::exit(1);
    }
}
