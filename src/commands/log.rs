use std::path::PathBuf;

use clap::Args;

use crate::integrations::log::{self, LogSpec};

#[derive(Clone, Debug, Args)]
#[command(
    about = "Graph of the repository's commits and where the branches point. In the pager, R re-runs the walk and q quits.",
    trailing_var_arg = true
)]
pub struct LogArgs {
    #[arg(
        long,
        default_value = ".",
        help = "repository directory (default: cwd)"
    )]
    pub repo: PathBuf,
    #[arg(
        long,
        action = clap::ArgAction::Append,
        value_name = "NAME",
        help = "show only this remote's refs (repeatable or comma-separated; default: origin)"
    )]
    pub remote: Vec<String>,
    #[arg(long, help = "show no remote refs at all")]
    pub no_remotes: bool,
    #[arg(long, help = "also walk and decorate tags")]
    pub tags: bool,
    #[arg(short = 'n', long, value_name = "N", help = "limit to N commits")]
    pub max_count: Option<i64>,
    #[arg(
        value_name = "GIT_ARG",
        allow_hyphen_values = true,
        help = "extra arguments for git log"
    )]
    pub git_args: Vec<String>,
}

pub fn run(args: LogArgs) -> Result<(), String> {
    log::run(
        &args.repo,
        &LogSpec {
            remotes: log::normalize_remotes(&args.remote, args.no_remotes),
            tags: args.tags,
            max_count: args.max_count,
            git_args: args.git_args,
        },
    )
}
