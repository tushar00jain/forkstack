use clap::Args;
use std::path::PathBuf;

use crate::ui::git::{delete_forkstack_branch, load_graph_for};

#[derive(Clone, Debug, Args)]
pub struct DeleteArgs {
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
        help = "local branch to check out after deletion"
    )]
    pub base: String,
    #[arg(long, help = "delete the checked-out layer and its lower stack")]
    pub substack: bool,
    #[arg(long, help = "apply the planned local and remote deletions")]
    pub execute: bool,
}

pub fn run(args: DeleteArgs) -> Result<(), String> {
    let graph = load_graph_for(&args.repo, &args.remote, &args.base)?;
    let plan = graph.plan_delete_current_branches(&args.base, args.substack)?;

    println!("delete local branches:");
    for branch in &plan.branches {
        println!("  {branch}");
    }
    println!("delete matching origin/upstream head/base branches");
    println!("checkout: {}", plan.checkout_branch);

    if args.execute {
        println!("{}", delete_forkstack_branch(&args.repo, &plan)?);
    } else {
        println!("\nDry run. Re-run with --execute to apply.");
    }
    Ok(())
}
