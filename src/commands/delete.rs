use clap::Args;
use std::path::PathBuf;

use crate::ui::git::{delete_forkstack_branch, load_graph_for};

#[derive(Clone, Debug, Args)]
pub struct DeleteArgs {
    /// Local Forkstack branch to delete.
    pub source: String,
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
        help = "fallback branch when deleting the current checkout"
    )]
    pub base: String,
    #[arg(long, help = "delete the selected layer and its lower stack")]
    pub substack: bool,
    #[arg(long, help = "apply the planned local and remote deletions")]
    pub execute: bool,
}

pub fn run(args: DeleteArgs) -> Result<(), String> {
    let graph = load_graph_for(&args.repo, &args.remote, &args.base)?;
    let branch = args
        .source
        .strip_prefix("refs/heads/")
        .unwrap_or(&args.source);
    let plan = graph.plan_delete_branches(branch, &args.base, args.substack)?;

    println!("delete local branches:");
    for branch in &plan.branches {
        println!("  {branch}");
    }
    println!("delete matching origin/upstream head/base branches");
    if plan
        .current_branch
        .as_ref()
        .is_some_and(|branch| plan.branches.contains(branch))
    {
        println!("checkout: {}", plan.checkout_branch);
    } else {
        println!("checkout: unchanged");
    }

    if args.execute {
        println!("{}", delete_forkstack_branch(&args.repo, &plan)?);
    } else {
        println!("\nDry run. Re-run with --execute to apply.");
    }
    Ok(())
}
