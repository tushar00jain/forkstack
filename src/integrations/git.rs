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

fn changed_refspecs(
    wanted: &BTreeMap<String, (String, String)>,
    local_targets: &BTreeMap<String, String>,
) -> Vec<String> {
    wanted
        .iter()
        .filter(|(destination, (_, oid))| local_targets.get(*destination) != Some(oid))
        .map(|(destination, (source, _))| format!("+{source}:{destination}"))
        .collect()
}

trait LocalRefs {
    fn targets(
        &self,
        exact: &[String],
        prefixes: &[String],
    ) -> Result<BTreeMap<String, String>, String>;
}

struct Git2LocalRefs<'a> {
    repo: &'a Path,
}

impl LocalRefs for Git2LocalRefs<'_> {
    fn targets(
        &self,
        exact: &[String],
        prefixes: &[String],
    ) -> Result<BTreeMap<String, String>, String> {
        let repository =
            git2::Repository::discover(self.repo).map_err(|error| error.message().to_owned())?;
        let mut targets = BTreeMap::new();
        for name in exact {
            if let Ok(reference) = repository.find_reference(name) {
                targets.insert(
                    name.clone(),
                    reference
                        .target()
                        .map(|oid| oid.to_string())
                        .unwrap_or_default(),
                );
            }
        }
        for pattern in prefixes {
            for reference in repository
                .references_glob(pattern)
                .map_err(|error| error.message().to_owned())?
            {
                let reference = reference.map_err(|error| error.message().to_owned())?;
                if let Some(name) = reference.name() {
                    targets.insert(
                        name.to_owned(),
                        reference
                            .target()
                            .map(|oid| oid.to_string())
                            .unwrap_or_default(),
                    );
                }
            }
        }
        Ok(targets)
    }
}

pub fn fetch_remote_branches(
    runner: &dyn CommandRunner,
    repo: &Path,
    remote: &str,
    branches: impl IntoIterator<Item = String>,
    branch_prefixes: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, String> {
    fetch_remote_branches_with(
        runner,
        &Git2LocalRefs { repo },
        repo,
        remote,
        branches,
        branch_prefixes,
    )
}

fn fetch_remote_branches_with(
    runner: &dyn CommandRunner,
    local_refs: &dyn LocalRefs,
    repo: &Path,
    remote: &str,
    branches: impl IntoIterator<Item = String>,
    branch_prefixes: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, String> {
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
        return Ok(Vec::new());
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
    let advertised: BTreeMap<_, _> = run_owned(runner, repo, &ls_args)?
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
        })
        .map(|(oid, reference)| (reference, oid))
        .collect();

    let mut wanted = BTreeMap::new();
    for (source, destination) in &refs {
        if let Some(oid) = advertised.get(source) {
            wanted.insert(destination.clone(), (source.clone(), oid.clone()));
        }
    }
    for (source_pattern, destination_pattern) in &prefix_refs {
        let source_root = source_pattern.trim_end_matches('*');
        let destination_root = destination_pattern.trim_end_matches('*');
        for (source, oid) in advertised
            .iter()
            .filter(|(source, _)| source.starts_with(source_root))
        {
            let destination = format!("{destination_root}{}", &source[source_root.len()..]);
            wanted.insert(destination, (source.clone(), oid.clone()));
        }
    }
    let exact_destinations: Vec<_> = refs
        .iter()
        .map(|(_, destination)| destination.clone())
        .collect();
    let prefix_destinations: Vec<_> = prefix_refs
        .iter()
        .map(|(_, destination)| destination.clone())
        .collect();
    let local_targets = local_refs.targets(&exact_destinations, &prefix_destinations)?;

    let mut fetch_args = vec![
        "fetch".into(),
        "--no-tags".into(),
        "--no-write-fetch-head".into(),
    ];
    fetch_args.extend(["--end-of-options".into(), remote.into()]);
    let fixed_arg_count = fetch_args.len();
    fetch_args.extend(changed_refspecs(&wanted, &local_targets));
    if fetch_args.len() > fixed_arg_count {
        run_owned(runner, repo, &fetch_args)?;
    }

    let mut missing: BTreeSet<_> = refs
        .iter()
        .filter(|(source, _)| !advertised.contains_key(source))
        .map(|(_, destination)| destination.clone())
        .collect();
    for (source_pattern, destination_pattern) in &prefix_refs {
        let source_root = source_pattern.trim_end_matches('*');
        let destination_root = destination_pattern.trim_end_matches('*');
        for destination in local_targets
            .keys()
            .filter(|destination| destination.starts_with(destination_root))
        {
            let source = format!("{source_root}{}", &destination[destination_root.len()..]);
            if !advertised.contains_key(&source) {
                missing.insert(destination.clone());
            }
        }
    }
    Ok(missing.into_iter().collect())
}

pub fn delete_refs(repo: &Path, references: &[String]) -> Result<(), String> {
    let repository =
        git2::Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    let existing: Vec<_> = references
        .iter()
        .filter(|name| repository.find_reference(name).is_ok())
        .collect();
    if existing.is_empty() {
        return Ok(());
    }
    let mut transaction = repository
        .transaction()
        .map_err(|error| error.message().to_owned())?;
    for name in &existing {
        transaction
            .lock_ref(name)
            .map_err(|error| error.message().to_owned())?;
    }
    for name in existing {
        transaction
            .remove(name)
            .map_err(|error| error.message().to_owned())?;
    }
    transaction
        .commit()
        .map_err(|error| error.message().to_owned())
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

    impl LocalRefs for BTreeMap<String, String> {
        fn targets(
            &self,
            exact: &[String],
            prefixes: &[String],
        ) -> Result<BTreeMap<String, String>, String> {
            Ok(self
                .iter()
                .filter(|(name, _)| {
                    exact.contains(name)
                        || prefixes
                            .iter()
                            .any(|prefix| name.starts_with(prefix.trim_end_matches('*')))
                })
                .map(|(name, oid)| (name.clone(), oid.clone()))
                .collect())
        }
    }

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
    fn targeted_fetch_constructs_exact_refspecs_and_reports_only_missing_refs() {
        let runner = Runner::default();
        let missing = fetch_remote_branches_with(
            &runner,
            &BTreeMap::new(),
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
        assert_eq!(calls.len(), 2);
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
        assert_eq!(missing, ["refs/remotes/upstream/fs-base/topic/1"]);
    }

    #[test]
    fn targeted_fetch_with_no_branches_runs_no_commands() {
        let runner = Runner::default();
        let missing =
            fetch_remote_branches_with(&runner, &BTreeMap::new(), Path::new("."), "origin", [], [])
                .unwrap();
        assert!(runner.calls.lock().unwrap().is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn targeted_fetch_expands_identity_prefixes_to_exact_refspecs() {
        let runner = Runner::default();
        fetch_remote_branches_with(
            &runner,
            &BTreeMap::new(),
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
                "--end-of-options",
                "origin",
                "+refs/heads/fs-head/topic/1:refs/remotes/origin/fs-head/topic/1",
                "+refs/heads/main:refs/remotes/origin/main",
            ]
        );
    }

    #[test]
    fn unchanged_tracking_refs_run_ls_remote_without_fetch() {
        let runner = Runner::default();
        let local = BTreeMap::from([
            ("refs/remotes/origin/fs-head/topic/1".into(), "aaaa".into()),
            ("refs/remotes/origin/main".into(), "bbbb".into()),
        ]);
        let missing = fetch_remote_branches_with(
            &runner,
            &local,
            Path::new("."),
            "origin",
            ["main".into(), "fs-head/topic/1".into()],
            [],
        )
        .unwrap();
        assert!(missing.is_empty());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0[0], "ls-remote");
    }
}
