use clap::Args;
use git2::Repository;
use std::path::{Path, PathBuf};

use crate::ui::git::{apply_move, load_graph_for};
use crate::ui::model::{Graph, MoveMode, MovePlan};

#[derive(Clone, Debug, Args)]
pub struct RewriteArgs {
    /// Branch, revision, or commit to move.
    pub source: String,
    /// Branch, revision, or commit to place the source after.
    pub destination: String,
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
    #[arg(long, help = "move the selected commit and its descendant stack")]
    pub substack: bool,
    #[arg(long, help = "apply the planned rewrite")]
    pub execute: bool,
}

fn resolve_commit(repo: &Path, revision: &str) -> Result<String, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    repository
        .revparse_single(revision)
        .and_then(|object| object.peel_to_commit())
        .map(|commit| commit.id().to_string())
        .map_err(|error| {
            format!(
                "could not resolve {revision:?} to a commit: {}",
                error.message()
            )
        })
}

fn print_plan(graph: &Graph, plan: &MovePlan, args: &RewriteArgs, mode: MoveMode) {
    let operation = match mode {
        MoveMode::Direct => "move",
        MoveMode::Reorder => "reorder",
    };
    println!(
        "{operation}: {} {} onto {}\nreplay:  {} commit(s)",
        args.source,
        if args.substack { "substack" } else { "commit" },
        args.destination,
        plan.commits.len()
    );
    for id in &plan.commits {
        let short = &id[..12.min(id.len())];
        let subject = graph
            .commits
            .get(id)
            .map(|commit| commit.subject.as_str())
            .unwrap_or_default();
        println!("  {short}  {subject}");
    }
    println!("checkout: {}", plan.checkout_branch);
}

pub fn run(args: RewriteArgs, mode: MoveMode) -> Result<(), String> {
    let graph = load_graph_for(&args.repo, &args.remote, &args.base)?;
    let source = resolve_commit(&args.repo, &args.source)?;
    let destination = resolve_commit(&args.repo, &args.destination)?;
    let plan = match mode {
        MoveMode::Direct => graph.plan_move(&source, &destination, args.substack)?,
        MoveMode::Reorder => graph.plan_reorder(&source, &destination, args.substack)?,
    };
    print_plan(&graph, &plan, &args, mode);
    if args.execute {
        apply_move(&args.repo, &plan)
    } else {
        println!("\nDry run. Re-run with --execute to apply.");
        Ok(())
    }
}
