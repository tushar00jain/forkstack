use std::collections::{BTreeMap, BTreeSet};
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

fn remote_branch_refs(
    remote: &str,
    branches: impl IntoIterator<Item = String>,
) -> Vec<(String, String)> {
    branches
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|branch| {
            (
                format!("refs/heads/{branch}"),
                format!("refs/remotes/{remote}/{branch}"),
            )
        })
        .collect()
}

pub fn fetch_remote_branches(
    runner: &dyn CommandRunner,
    repo: &Path,
    remote: &str,
    branches: impl IntoIterator<Item = String>,
    branch_prefixes: impl IntoIterator<Item = String>,
) -> Result<(), String> {
    let refs = remote_branch_refs(remote, branches);
    let prefix_refs: Vec<_> = branch_prefixes
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|prefix| {
            (
                format!("refs/heads/{prefix}/*"),
                format!("refs/remotes/{remote}/{prefix}/*"),
            )
        })
        .collect();
    if refs.is_empty() && prefix_refs.is_empty() {
        return Ok(());
    }

    let mut ls_args = vec![
        "ls-remote".into(),
        "--refs".into(),
        "--heads".into(),
        "--end-of-options".into(),
        remote.into(),
    ];
    ls_args.extend(refs.iter().map(|(source, _)| source.clone()));
    ls_args.extend(prefix_refs.iter().map(|(source, _)| source.clone()));
    let advertised: BTreeSet<_> = run_owned(runner, repo, &ls_args)?
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
        .collect();

    let mut fetch_args = vec![
        "fetch".into(),
        "--no-tags".into(),
        "--no-write-fetch-head".into(),
    ];
    if !prefix_refs.is_empty() {
        fetch_args.push("--prune".into());
    }
    fetch_args.extend(["--end-of-options".into(), remote.into()]);
    let fixed_arg_count = fetch_args.len();
    fetch_args.extend(
        refs.iter()
            .filter(|(source, _)| advertised.contains(source))
            .filter(|(source, _)| {
                !prefix_refs
                    .iter()
                    .any(|(prefix, _)| source.starts_with(prefix.trim_end_matches('*')))
            })
            .map(|(source, destination)| format!("+{source}:{destination}")),
    );
    fetch_args.extend(
        prefix_refs
            .iter()
            .map(|(source, destination)| format!("+{source}:{destination}")),
    );
    if fetch_args.len() > fixed_arg_count {
        run_owned(runner, repo, &fetch_args)?;
    }

    let deletes = refs
        .iter()
        .filter(|(source, _)| !advertised.contains(source))
        .filter(|(source, _)| {
            !prefix_refs
                .iter()
                .any(|(prefix, _)| source.starts_with(prefix.trim_end_matches('*')))
        })
        .map(|(_, destination)| format!("delete {destination}\n"))
        .collect::<String>();
    if !deletes.is_empty() {
        run_input(
            runner,
            repo,
            &["update-ref".into(), "--stdin".into()],
            &deletes,
            &BTreeMap::new(),
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Runner {
        calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
    }

    impl CommandRunner for Runner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _repo: &Path,
            input: Option<&str>,
            _env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            assert_eq!(program, "git");
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), input.map(str::to_owned)));
            if args.first().is_some_and(|arg| arg == "ls-remote") {
                Ok(["aaaa\trefs/heads/fs-head/topic/1", "bbbb\trefs/heads/main"].join("\n"))
            } else {
                Ok(String::new())
            }
        }
    }

    #[test]
    fn targeted_fetch_constructs_exact_refspecs_and_deletes_only_missing_refs() {
        let runner = Runner::default();
        fetch_remote_branches(
            &runner,
            Path::new("."),
            "upstream",
            [
                "main".into(),
                "fs-head/topic/1".into(),
                "fs-base/topic/1".into(),
                "fs-head/topic/1".into(),
            ],
            [],
        )
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0].0,
            [
                "ls-remote",
                "--refs",
                "--heads",
                "--end-of-options",
                "upstream",
                "refs/heads/fs-base/topic/1",
                "refs/heads/fs-head/topic/1",
                "refs/heads/main",
            ]
        );
        assert_eq!(
            calls[1].0,
            [
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--end-of-options",
                "upstream",
                "+refs/heads/fs-head/topic/1:refs/remotes/upstream/fs-head/topic/1",
                "+refs/heads/main:refs/remotes/upstream/main",
            ]
        );
        assert_eq!(calls[2].0, ["update-ref", "--stdin"]);
        assert_eq!(
            calls[2].1.as_deref(),
            Some("delete refs/remotes/upstream/fs-base/topic/1\n")
        );
    }

    #[test]
    fn targeted_fetch_with_no_branches_runs_no_commands() {
        let runner = Runner::default();
        fetch_remote_branches(&runner, Path::new("."), "origin", [], []).unwrap();
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn targeted_fetch_constructs_a_pruned_identity_prefix_refspec() {
        let runner = Runner::default();
        fetch_remote_branches(
            &runner,
            Path::new("."),
            "origin",
            ["main".into(), "fs-head/topic/1".into()],
            ["fs-head/topic".into()],
        )
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].0,
            [
                "ls-remote",
                "--refs",
                "--heads",
                "--end-of-options",
                "origin",
                "refs/heads/fs-head/topic/1",
                "refs/heads/main",
                "refs/heads/fs-head/topic/*",
            ]
        );
        assert_eq!(
            calls[1].0,
            [
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--prune",
                "--end-of-options",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
                "+refs/heads/fs-head/topic/*:refs/remotes/origin/fs-head/topic/*",
            ]
        );
    }
}
