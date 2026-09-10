use std::path::PathBuf;

use clap::{Parser, Subcommand};
use forkstack::commands::{self, log::LogArgs, submit::SubmitArgs, ui::UiArgs};

#[derive(Debug, Parser)]
#[command(about = "Create a stack of one-commit pull requests inside your own fork")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show a Git graph of stacks and branches.
    Log(LogArgs),
    /// Push branches and open the pull requests.
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
        Command::Log(args) => commands::log::run(args),
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
