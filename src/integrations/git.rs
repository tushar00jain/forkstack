use std::collections::BTreeMap;
use std::path::Path;

use super::CommandRunner;

pub fn run(runner: &dyn CommandRunner, repo: &Path, args: &[&str]) -> Result<String, String> {
    runner.run(
        "git",
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        repo,
        None,
        &BTreeMap::new(),
    )
}

pub fn run_owned(
    runner: &dyn CommandRunner,
    repo: &Path,
    args: &[String],
) -> Result<String, String> {
    runner.run("git", args, repo, None, &BTreeMap::new())
}

pub fn run_input(
    runner: &dyn CommandRunner,
    repo: &Path,
    args: &[String],
    input: &str,
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    runner.run("git", args, repo, Some(input), env)
}

pub fn fetch(runner: &dyn CommandRunner, repo: &Path, remote: &str) -> Result<(), String> {
    run(runner, repo, &["fetch", remote]).map(|_| ())
}

pub fn push_atomic(
    runner: &dyn CommandRunner,
    repo: &Path,
    remote: &str,
    updates: impl IntoIterator<Item = (String, String)>,
) -> Result<(), String> {
    let mut args = vec![
        "push".into(),
        "--atomic".into(),
        "--force-with-lease".into(),
        remote.into(),
    ];
    args.extend(
        updates
            .into_iter()
            .map(|(branch, rev)| format!("{rev}:refs/heads/{branch}")),
    );
    run_owned(runner, repo, &args).map(|_| ())
}
