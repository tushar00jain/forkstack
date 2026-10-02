use clap::Args;
use std::path::PathBuf;

use crate::ui::git;

#[derive(Clone, Debug, Args)]
pub struct AmendArgs {
    #[arg(
        long,
        default_value = ".",
        help = "repository directory (default: cwd)"
    )]
    pub repo: PathBuf,
    #[arg(long, default_value = "origin", help = "remote for your fork")]
    pub remote: String,
    #[arg(
        long,
        default_value = "main",
        help = "branch in the fork the stack sits on"
    )]
    pub base: String,
}

pub fn run(args: AmendArgs) -> Result<(), String> {
    println!(
        "{}",
        git::amend_and_restack(&args.repo, &args.remote, &args.base)?
    );
    Ok(())
}
