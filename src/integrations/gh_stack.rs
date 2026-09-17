use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::CommandRunner;

#[derive(Debug, PartialEq, Eq)]
struct RemoteStack {
    number: u64,
    pull_requests: BTreeSet<u64>,
}

fn parse_stacks(output: &str) -> Result<Vec<RemoteStack>, String> {
    let stacks: Vec<Value> = serde_json::from_str(output)
        .map_err(|error| format!("could not parse GitHub stack lookup: {error}"))?;
    stacks
        .into_iter()
        .map(|stack| {
            let number = stack
                .get("number")
                .and_then(Value::as_u64)
                .ok_or("GitHub stack lookup omitted its number")?;
            let pull_requests = stack
                .get("pull_requests")
                .and_then(Value::as_array)
                .ok_or("GitHub stack lookup omitted its pull requests")?
                .iter()
                .map(|pr| {
                    pr.as_u64()
                        .or_else(|| pr.get("number").and_then(Value::as_u64))
                        .ok_or_else(|| {
                            format!("GitHub stack #{number} contained an invalid pull request")
                        })
                })
                .collect::<Result<_, _>>()?;
            Ok(RemoteStack {
                number,
                pull_requests,
            })
        })
        .collect()
}

fn existing_stacks(
    runner: &dyn CommandRunner,
    repo: &Path,
    github_repo: &str,
    pull_requests: &[u64],
) -> Result<BTreeMap<u64, BTreeSet<u64>>, String> {
    let env = BTreeMap::from([("GH_REPO".into(), github_repo.to_owned())]);
    let mut stacks = BTreeMap::new();
    for pull_request in pull_requests {
        let endpoint = format!("repos/{github_repo}/stacks?pull_request={pull_request}");
        let output = runner.run("gh", &["api".into(), endpoint], repo, None, &env)?;
        for stack in parse_stacks(&output)? {
            stacks.insert(stack.number, stack.pull_requests);
        }
    }
    Ok(stacks)
}

fn unstack(
    runner: &dyn CommandRunner,
    repo: &Path,
    github_repo: &str,
    stack_number: u64,
) -> Result<(), String> {
    let env = BTreeMap::from([("GH_REPO".into(), github_repo.to_owned())]);
    runner
        .run(
            "gh",
            &["stack".into(), "unstack".into(), stack_number.to_string()],
            repo,
            None,
            &env,
        )
        .map(|_| ())
        .map_err(|error| format!("could not unstack GitHub stack #{stack_number}: {error}"))
}

pub fn link(
    runner: &dyn CommandRunner,
    repo: &Path,
    github_repo: &str,
    remote: &str,
    base: &str,
    branches: &[String],
) -> Result<(), String> {
    let mut args = ["stack", "link", "--remote", remote, "--base", base]
        .map(str::to_owned)
        .to_vec();
    args.extend(branches.iter().cloned());
    let env = BTreeMap::from([("GH_REPO".into(), github_repo.to_owned())]);
    runner.run("gh", &args, repo, None, &env).map(|_| ())
}

pub struct PullRequestMembership<'a> {
    pub existing: &'a [u64],
    pub desired: &'a [u64],
}

pub fn reconcile_and_link(
    runner: &dyn CommandRunner,
    repo: &Path,
    github_repo: &str,
    remote: &str,
    base: &str,
    branches: &[String],
    pull_requests: PullRequestMembership<'_>,
) -> Result<(), String> {
    let stacks = existing_stacks(runner, repo, github_repo, pull_requests.existing)?;
    let desired: BTreeSet<_> = pull_requests.desired.iter().copied().collect();
    let stale_members = stacks.values().any(|members| !members.is_subset(&desired));
    if stacks.len() > 1 || stale_members {
        for stack_number in stacks.keys() {
            unstack(runner, repo, github_repo, *stack_number)?;
        }
    }
    link(runner, repo, github_repo, remote, base, branches)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Call = (String, Vec<String>, BTreeMap<String, String>);

    #[derive(Default)]
    struct Runner {
        calls: Mutex<Vec<Call>>,
    }

    impl CommandRunner for Runner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _repo: &Path,
            input: Option<&str>,
            env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            assert!(input.is_none());
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec(), env.clone()));
            Ok(String::new())
        }
    }

    #[test]
    fn link_runs_for_the_published_origin_branches() {
        let runner = Runner::default();

        link(
            &runner,
            Path::new("repo"),
            "owner/repo",
            "origin",
            "main",
            &["fs-head/topic/1".into(), "fs-head/topic/2".into()],
        )
        .unwrap();

        assert_eq!(
            *runner.calls.lock().unwrap(),
            [(
                "gh".into(),
                [
                    "stack",
                    "link",
                    "--remote",
                    "origin",
                    "--base",
                    "main",
                    "fs-head/topic/1",
                    "fs-head/topic/2",
                ]
                .map(str::to_owned)
                .to_vec(),
                BTreeMap::from([("GH_REPO".into(), "owner/repo".into())]),
            )]
        );
    }

    struct ReconcileRunner {
        response: String,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for ReconcileRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _repo: &Path,
            _input: Option<&str>,
            env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            assert_eq!(program, "gh");
            assert_eq!(env.get("GH_REPO").map(String::as_str), Some("owner/repo"));
            self.calls.lock().unwrap().push(args.to_vec());
            Ok(if args.starts_with(&["api".into()]) {
                self.response.clone()
            } else {
                String::new()
            })
        }
    }

    #[test]
    fn stale_stack_is_unstacked_before_relinking() {
        let runner = ReconcileRunner {
            response: r#"[{"number":7,"pull_requests":[101,{"number":102},103]}]"#.into(),
            calls: Mutex::new(Vec::new()),
        };

        reconcile_and_link(
            &runner,
            Path::new("repo"),
            "owner/repo",
            "origin",
            "main",
            &["fs-head/topic/1".into(), "fs-head/topic/3".into()],
            PullRequestMembership {
                existing: &[101, 103],
                desired: &[101, 103],
            },
        )
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[2], ["stack", "unstack", "7"]);
        assert!(calls[3].starts_with(&["stack".into(), "link".into()]));
    }

    #[test]
    fn additive_stack_is_relinked_without_unstacking() {
        let runner = ReconcileRunner {
            response: r#"[{"number":7,"pull_requests":[101]}]"#.into(),
            calls: Mutex::new(Vec::new()),
        };

        reconcile_and_link(
            &runner,
            Path::new("repo"),
            "owner/repo",
            "origin",
            "main",
            &["fs-head/topic/1".into(), "fs-head/topic/2".into()],
            PullRequestMembership {
                existing: &[101],
                desired: &[101, 102],
            },
        )
        .unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].starts_with(&["stack".into(), "link".into()]));
    }
}
