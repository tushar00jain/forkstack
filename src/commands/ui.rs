use clap::Args;
use std::path::PathBuf;

use crate::core::submit::SubmitOptions;
use crate::ui::app::App;

#[derive(Clone, Debug, Args)]
pub struct UiArgs {
    #[arg(
        long,
        default_value = ".",
        help = "repository or parent directory to scan one level down (default: cwd)"
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
        help = "identity prefix, required when the stack has untagged commits"
    )]
    pub prefix: Option<String>,
    #[arg(long = "no-draft", help = "open new PRs ready for review")]
    pub no_draft: bool,
}

pub fn run(args: UiArgs) -> Result<(), String> {
    App::new(SubmitOptions {
        repo: args.repo,
        remote: args.remote,
        base: args.base,
        prefix: args.prefix,
        draft: !args.no_draft,
    })?
    .run()
}
