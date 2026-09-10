use clap::Args;
use std::path::PathBuf;

use crate::core::submit::{self, SubmitOptions};

#[derive(Clone, Debug, Args)]
pub struct SubmitArgs {
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
    #[arg(
        long,
        help = "identity prefix, required only when the stack has untagged commits"
    )]
    pub prefix: Option<String>,
    #[arg(long = "no-draft", help = "open PRs ready for review")]
    pub no_draft: bool,
    #[arg(long, help = "actually push branches and create PRs")]
    pub execute: bool,
}

impl SubmitArgs {
    pub fn options(&self) -> SubmitOptions {
        SubmitOptions {
            repo: self.repo.clone(),
            remote: self.remote.clone(),
            base: self.base.clone(),
            prefix: self.prefix.clone(),
            draft: !self.no_draft,
        }
    }
}

pub fn run(args: SubmitArgs) -> Result<(), String> {
    let options = args.options();
    if args.execute {
        println!("fetching {}", options.remote);
        crate::integrations::git::fetch(
            &crate::integrations::ProcessRunner,
            &options.repo,
            &options.remote,
        )?;
        let plan = submit::plan(options)?;
        submit::print_summary(&plan);
        submit::execute(plan)
    } else {
        submit::print_plan(&submit::plan(options)?);
        Ok(())
    }
}
